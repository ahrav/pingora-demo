use async_trait::async_trait;
use std::{fmt, sync::Arc, time::Duration};

// TODO: Why is this a struct? New Type?
// Can we use an array instead of a vec if we know
// the size of the cache key ahead of time?
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey(pub Vec<u8>);

impl CacheKey {
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<&str> for CacheKey {
    fn from(value: &str) -> Self {
        Self(value.as_bytes().to_vec())
    }
}

impl From<Vec<u8>> for CacheKey {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
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
    Fresh { meta: CacheMeta, hit: Box<dyn HitHandler> },
    Stale { meta: CacheMeta, hit: Box<dyn HitHandler> },
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
}
