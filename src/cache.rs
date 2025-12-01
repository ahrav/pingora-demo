use async_trait::async_trait;
use std::hash::{Hash, Hasher};
use std::{fmt, sync::Arc, time::Duration};

/// Cache key with hashed storage representation.
///
/// Keys are hashed to prevent path traversal attacks when used with
/// filesystem-backed storage. The hashed key is a safe hex-only identifier
/// in the format "idx-{hash:x}".
///
/// In debug builds, the original path is preserved for debugging purposes.
#[derive(Clone, PartialEq, Eq)]
pub struct CacheKey {
    /// Hashed key for storage (safe format: "idx-{hex}")
    hashed: Vec<u8>,
    /// Original path for debugging (debug builds only)
    #[cfg(debug_assertions)]
    original: String,
}

impl CacheKey {
    /// Create a new cache key from a URI path.
    ///
    /// The path is hashed using BLAKE3 to produce a stable, safe storage key
    /// that cannot contain path traversal sequences like `..` or `/`.
    /// The hash is truncated to 128 bits (32 hex chars) for a reasonable key length.
    pub fn new(path: &str) -> Self {
        let hash = blake3::hash(path.as_bytes());
        // Take first 16 bytes (128 bits) for 32 hex chars
        let hash_bytes: [u8; 16] = hash.as_bytes()[..16].try_into().unwrap();
        let hashed_key = format!("idx-{:032x}", u128::from_be_bytes(hash_bytes));

        Self {
            hashed: hashed_key.into_bytes(),
            #[cfg(debug_assertions)]
            original: path.to_string(),
        }
    }

    /// Returns the hashed key bytes for storage operations.
    pub fn as_slice(&self) -> &[u8] {
        &self.hashed
    }

    /// Returns the original path for debugging (debug builds only).
    #[cfg(debug_assertions)]
    pub fn original_path(&self) -> &str {
        &self.original
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hashed.hash(state);
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(debug_assertions)]
        {
            f.debug_struct("CacheKey")
                .field("hashed", &String::from_utf8_lossy(&self.hashed))
                .field("original", &self.original)
                .finish()
        }
        #[cfg(not(debug_assertions))]
        {
            f.debug_struct("CacheKey")
                .field("hashed", &String::from_utf8_lossy(&self.hashed))
                .finish()
        }
    }
}

impl From<&str> for CacheKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<Vec<u8>> for CacheKey {
    fn from(value: Vec<u8>) -> Self {
        let hash = blake3::hash(&value);
        // Take first 16 bytes (128 bits) for 32 hex chars
        let hash_bytes: [u8; 16] = hash.as_bytes()[..16].try_into().unwrap();
        let hashed_key = format!("idx-{:032x}", u128::from_be_bytes(hash_bytes));

        Self {
            hashed: hashed_key.into_bytes(),
            #[cfg(debug_assertions)]
            original: String::from_utf8_lossy(&value).into_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheMeta {
    pub content_length: Option<usize>,
    pub ttl: Duration,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cache_control: Option<String>,
    pub created_at: Option<u64>,
    pub expires_at: Option<u64>,
    pub version: Option<u64>,
}

impl CacheMeta {
    pub fn new(content_length: Option<usize>, ttl: Duration) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let created_at = Some(now);
        let expires_at = Some(now + ttl.as_secs());

        Self {
            content_length,
            ttl,
            content_type: None,
            etag: None,
            last_modified: None,
            cache_control: None,
            created_at,
            expires_at,
            version: None,
        }
    }

    pub fn is_fresh(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if let Some(expires_at) = self.expires_at {
            return now < expires_at;
        }

        if let Some(created_at) = self.created_at {
            return now < created_at + self.ttl.as_secs();
        }

        true
    }
}

const TTL: Duration = Duration::from_secs(300);

impl Default for CacheMeta {
    fn default() -> Self {
        Self {
            content_length: None,
            ttl: TTL,
            content_type: None,
            etag: None,
            last_modified: None,
            cache_control: None,
            created_at: None,
            expires_at: None,
            version: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CacheError {
    Backend(String),
    Conflict,
    NotFound,
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Backend(msg) => write!(f, "{msg}"),
            CacheError::Conflict => write!(f, "conflict"),
            CacheError::NotFound => write!(f, "not found"),
        }
    }
}

impl std::error::Error for CacheError {}

pub type CacheResult<T> = Result<T, CacheError>;
pub type StorageRef = Arc<dyn Storage>;

pub enum LookupResult {
    Fresh {
        meta: CacheMeta,
        hit: Box<dyn HitHandler>,
    },
    Stale {
        meta: CacheMeta,
        hit: Box<dyn HitHandler>,
    },
    Miss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissFinishType {
    Success,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurgeType {
    Soft,
    Hard,
}

#[async_trait]
pub trait HitHandler: Send {
    async fn read_body(&mut self) -> CacheResult<Option<Vec<u8>>>;
    async fn finish(self: Box<Self>) -> CacheResult<()>;
}

#[async_trait]
pub trait MissHandler: Send {
    async fn write_body(&mut self, data: Vec<u8>, eof: bool) -> CacheResult<()>;
    async fn finish(self: Box<Self>) -> CacheResult<MissFinishType>;
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn lookup(&self, key: &CacheKey) -> CacheResult<LookupResult>;

    async fn get_miss_handler(
        &self,
        key: &CacheKey,
        meta: &CacheMeta,
    ) -> CacheResult<Box<dyn MissHandler>>;

    async fn purge(&self, key: &CacheKey, typ: PurgeType) -> CacheResult<bool>;

    async fn update_meta(&self, key: &CacheKey, meta: &CacheMeta) -> CacheResult<bool>;

    async fn touch(&self, key: &CacheKey, new_expiry: u64) -> CacheResult<bool>;
}

#[cfg(test)]
mod cache_key_tests {
    use super::*;

    #[test]
    fn cache_key_hashes_path() {
        let key = CacheKey::from("/images/photo.jpg");
        let slice = key.as_slice();

        // Key should start with "idx-" prefix
        assert!(slice.starts_with(b"idx-"));

        // Key should be hex (no path chars)
        let key_str = std::str::from_utf8(slice).unwrap();
        assert!(!key_str.contains('/'));
        assert!(!key_str.contains('.'));
    }

    #[test]
    fn cache_key_prevents_path_traversal() {
        let malicious = CacheKey::from("/../../../etc/passwd");
        let slice = malicious.as_slice();
        let key_str = std::str::from_utf8(slice).unwrap();

        // Must not contain traversal sequences
        assert!(!key_str.contains(".."));
        assert!(!key_str.contains('/'));
        assert!(!key_str.contains('\\'));
    }

    #[test]
    fn cache_key_prevents_encoded_traversal() {
        // Even URL-encoded traversal attempts should be hashed away
        let encoded = CacheKey::from("/%2e%2e/%2e%2e/etc/passwd");
        let slice = encoded.as_slice();
        let key_str = std::str::from_utf8(slice).unwrap();

        assert!(key_str.starts_with("idx-"));
        assert!(!key_str.contains('%'));
        assert!(!key_str.contains('/'));
    }

    #[test]
    fn cache_key_prevents_null_byte_injection() {
        let null_byte = CacheKey::from("/images/foo\x00bar.jpg");
        let slice = null_byte.as_slice();
        let key_str = std::str::from_utf8(slice).unwrap();

        assert!(key_str.starts_with("idx-"));
        // Null byte should not appear in hashed output
        assert!(!slice.contains(&0u8));
    }

    #[test]
    fn cache_key_deterministic() {
        let key1 = CacheKey::from("/images/photo.jpg");
        let key2 = CacheKey::from("/images/photo.jpg");
        assert_eq!(key1.as_slice(), key2.as_slice());
    }

    #[test]
    fn cache_key_different_paths_different_hashes() {
        let key1 = CacheKey::from("/images/photo1.jpg");
        let key2 = CacheKey::from("/images/photo2.jpg");
        assert_ne!(key1.as_slice(), key2.as_slice());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cache_key_preserves_original_in_debug() {
        let key = CacheKey::from("/images/photo.jpg");
        assert_eq!(key.original_path(), "/images/photo.jpg");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cache_key_preserves_malicious_original_in_debug() {
        // In debug mode, we should be able to see what the original path was
        let key = CacheKey::from("/../../../etc/passwd");
        assert_eq!(key.original_path(), "/../../../etc/passwd");
    }

    #[test]
    fn cache_key_from_vec_u8() {
        let key = CacheKey::from(b"/test/path".to_vec());
        assert!(key.as_slice().starts_with(b"idx-"));
    }

    #[test]
    fn cache_key_hash_impl_uses_hashed_bytes() {
        use std::collections::HashMap;

        let key1 = CacheKey::from("/test/path");
        let key2 = CacheKey::from("/test/path");

        // Keys should be usable as HashMap keys
        let mut map = HashMap::new();
        map.insert(key1.clone(), "value1");

        assert_eq!(map.get(&key2), Some(&"value1"));
    }

    #[test]
    fn cache_key_format_is_consistent() {
        // Verify the format is "idx-" followed by exactly 32 hex chars (128-bit BLAKE3)
        let key = CacheKey::from("/any/path");
        let key_str = std::str::from_utf8(key.as_slice()).unwrap();

        assert!(key_str.starts_with("idx-"));

        // Everything after "idx-" should be exactly 32 hex chars
        let hex_part = &key_str[4..];
        assert_eq!(hex_part.len(), 32, "Hash should be 32 hex chars (128 bits)");
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cache_key_hash_is_stable_blake3() {
        // Pin known input/output to detect algorithm changes.
        // This test will fail if the hash algorithm or truncation changes,
        // which would break cache persistence across upgrades.
        let key = CacheKey::from("/test/stability");
        let key_str = std::str::from_utf8(key.as_slice()).unwrap();

        // BLAKE3 hash of "/test/stability" truncated to 128 bits (first 16 bytes)
        // Computed using: blake3::hash(b"/test/stability").as_bytes()[..16] as u128
        let expected = "idx-1473ae5860a6e513d83ed7abd886864a";

        assert_eq!(
            key_str, expected,
            "Hash output changed! This breaks cache persistence. \
             If intentional, update this test with the new expected value."
        );
    }
}
