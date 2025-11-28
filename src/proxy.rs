//! Proxy integration layer for cache decision-making.
//!
//! This module provides the bridge between HTTP proxy request handling
//! and the underlying cache storage layer.

use crate::cache::{CacheMeta, HitHandler};

/// Parsed Cache-Control directives.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CacheControlDirectives {
    /// `max-age` (seconds)
    pub max_age: Option<u64>,
    /// `s-maxage` (seconds)
    pub s_maxage: Option<u64>,
    /// `stale-if-error` (seconds)
    pub stale_if_error: Option<u64>,
    pub no_cache: bool,
    pub no_store: bool,
    pub must_revalidate: bool,
}

impl CacheControlDirectives {
    /// Returns the most specific max-age value (`s-maxage` takes precedence).
    pub fn effective_max_age(&self) -> Option<u64> {
        self.s_maxage.or(self.max_age)
    }

    /// Returns true when responses may be stored locally.
    pub fn allows_store(&self) -> bool {
        !self.no_store
    }
}

/// Parse a Cache-Control header into structured directives.
pub fn parse_cache_control(value: &str) -> CacheControlDirectives {
    let mut directives = CacheControlDirectives::default();

    for part in value.split(',') {
        let token = part.trim();
        if token.is_empty() {
            continue;
        }

        let mut iter = token.splitn(2, '=');
        let name = iter
            .next()
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let raw_value = iter.next().map(str::trim).map(|s| s.trim_matches('"'));

        match name.as_str() {
            "no-cache" => directives.no_cache = true,
            "no-store" => directives.no_store = true,
            "must-revalidate" => directives.must_revalidate = true,
            "max-age" => {
                if let Some(v) = raw_value.and_then(|s| s.parse::<u64>().ok()) {
                    directives.max_age = Some(v);
                }
            }
            "s-maxage" => {
                if let Some(v) = raw_value.and_then(|s| s.parse::<u64>().ok()) {
                    directives.s_maxage = Some(v);
                }
            }
            "stale-if-error" => {
                if let Some(v) = raw_value.and_then(|s| s.parse::<u64>().ok()) {
                    directives.stale_if_error = Some(v);
                }
            }
            _ => {}
        }
    }

    directives
}

// =============================================================================
// ETag Conditional Matching (RFC 7232)
// =============================================================================

/// Parsed ETag with strong/weak indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ETag {
    /// True if this is a weak ETag (prefixed with `W/`)
    pub weak: bool,
    /// The opaque-tag value (without quotes or W/ prefix)
    pub value: String,
}

/// Parsed If-None-Match header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfNoneMatch {
    /// Wildcard `*` - matches any ETag
    Any,
    /// Specific ETags to match against
    Tags(Vec<ETag>),
}

/// Parse a single ETag from a string.
///
/// Handles formats:
/// - Strong: `"abc"`
/// - Weak: `W/"abc"`
/// - Unquoted fallback: `abc` (defensive, non-standard)
///
/// Returns `None` for empty or invalid input.
pub fn parse_etag(s: &str) -> Option<ETag> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Check for weak prefix
    let (weak, rest) = if s.starts_with("W/") || s.starts_with("w/") {
        (true, &s[2..])
    } else {
        (false, s)
    };

    // Extract value - prefer quoted, fall back to unquoted
    let value = if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
        // Standard quoted format: strip quotes
        rest[1..rest.len() - 1].to_string()
    } else if rest.starts_with('"') {
        // Malformed: starts with quote but doesn't end with one
        return None;
    } else {
        // Unquoted fallback (defensive)
        rest.to_string()
    };

    if value.is_empty() {
        return None;
    }

    Some(ETag { weak, value })
}

/// Parse an If-None-Match header into structured form.
///
/// Handles:
/// - Wildcard: `*`
/// - Single ETag: `"abc"` or `W/"abc"`
/// - Multiple ETags: `"foo", "bar", W/"baz"`
///
/// Invalid entries are skipped (defensive parsing).
pub fn parse_if_none_match(header: &str) -> IfNoneMatch {
    let header = header.trim();

    // Check for wildcard
    if header == "*" {
        return IfNoneMatch::Any;
    }

    // Parse comma-separated ETags
    let tags: Vec<ETag> = header.split(',').filter_map(parse_etag).collect();

    IfNoneMatch::Tags(tags)
}

/// Check if a stored ETag matches an If-None-Match header value.
///
/// Uses **weak comparison** per RFC 7232 Section 2.3.2, which is appropriate
/// for GET/HEAD conditional requests. In weak comparison, only the opaque-tag
/// values are compared; the weak/strong indicator is ignored.
///
/// Returns `true` if:
/// - The If-None-Match header is `*` (wildcard), OR
/// - Any ETag in the If-None-Match header has the same opaque-tag value
///
/// # Arguments
/// * `stored_etag` - The ETag from the cached response (e.g., `"abc"` or `W/"abc"`)
/// * `if_none_match` - The If-None-Match header value from the request
pub fn matches_etag(stored_etag: &str, if_none_match: &str) -> bool {
    // Parse the stored ETag
    let stored = match parse_etag(stored_etag) {
        Some(etag) => etag,
        None => return false, // No valid stored ETag means no match
    };

    // Parse and check the If-None-Match header
    match parse_if_none_match(if_none_match) {
        IfNoneMatch::Any => true, // Wildcard matches everything
        IfNoneMatch::Tags(tags) => {
            // Weak comparison: only compare opaque-tag values
            tags.iter().any(|tag| tag.value == stored.value)
        }
    }
}

// =============================================================================
// Cache Decision
// =============================================================================

/// Represents the cache decision for an incoming request.
///
/// The proxy layer uses this enum to determine how to handle a request:
/// - Serve directly from cache (streaming via HitHandler)
/// - Return 304 Not Modified for conditional requests
/// - Revalidate stale content with origin
/// - Fetch fresh content from origin
pub enum CacheDecision {
    /// Cache hit - serve content directly from cache.
    /// The `hit` handler provides streaming access to the cached body.
    ServeFromCache {
        meta: CacheMeta,
        hit: Box<dyn HitHandler>,
    },

    /// Conditional request matched - return 304 Not Modified.
    /// Client already has current content (ETag or Last-Modified matched).
    NotModified { meta: CacheMeta },

    /// Cache has stale content - revalidate with origin.
    /// The `stale` metadata enables conditional requests to origin
    /// (If-None-Match, If-Modified-Since).
    RevalidateWithOrigin { stale: Option<CacheMeta> },

    /// Cache miss - fetch from origin without conditional headers.
    FetchFromOrigin,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_directives() {
        let directives = parse_cache_control("max-age=60, s-maxage=120, stale-if-error=30");
        assert_eq!(directives.max_age, Some(60));
        assert_eq!(directives.s_maxage, Some(120));
        assert_eq!(directives.stale_if_error, Some(30));
        assert!(!directives.no_cache);
        assert!(!directives.no_store);
        assert!(!directives.must_revalidate);
        assert_eq!(directives.effective_max_age(), Some(120));
        assert!(directives.allows_store());
    }

    #[test]
    fn parses_flags_and_quotes() {
        let directives = parse_cache_control(
            "  no-cache , no-store, must-revalidate, max-age=\"0\", stale-if-error=\"45\" ",
        );
        assert_eq!(directives.max_age, Some(0));
        assert_eq!(directives.stale_if_error, Some(45));
        assert!(directives.no_cache);
        assert!(directives.no_store);
        assert!(directives.must_revalidate);
        assert!(!directives.allows_store());
    }

    #[test]
    fn ignores_unknown_and_invalid_values() {
        let directives = parse_cache_control("public, max-age=abc, stale-if-error=, foo=bar");
        assert_eq!(directives.max_age, None);
        assert_eq!(directives.stale_if_error, None);
        assert!(!directives.no_cache);
        assert!(directives.allows_store());
    }

    // =========================================================================
    // ETag Parsing Tests
    // =========================================================================

    #[test]
    fn parse_etag_strong() {
        let etag = parse_etag(r#""abc123""#).unwrap();
        assert!(!etag.weak);
        assert_eq!(etag.value, "abc123");
    }

    #[test]
    fn parse_etag_weak() {
        let etag = parse_etag(r#"W/"abc123""#).unwrap();
        assert!(etag.weak);
        assert_eq!(etag.value, "abc123");
    }

    #[test]
    fn parse_etag_weak_lowercase() {
        let etag = parse_etag(r#"w/"abc123""#).unwrap();
        assert!(etag.weak);
        assert_eq!(etag.value, "abc123");
    }

    #[test]
    fn parse_etag_with_whitespace() {
        let etag = parse_etag(r#"  "abc"  "#).unwrap();
        assert!(!etag.weak);
        assert_eq!(etag.value, "abc");
    }

    #[test]
    fn parse_etag_unquoted_fallback() {
        // Defensive: handles non-standard unquoted ETags
        let etag = parse_etag("abc123").unwrap();
        assert!(!etag.weak);
        assert_eq!(etag.value, "abc123");
    }

    #[test]
    fn parse_etag_empty_returns_none() {
        assert!(parse_etag("").is_none());
        assert!(parse_etag("   ").is_none());
    }

    #[test]
    fn parse_etag_empty_quoted_returns_none() {
        assert!(parse_etag(r#""""#).is_none());
        assert!(parse_etag(r#"W/"""#).is_none());
    }

    #[test]
    fn parse_etag_malformed_quote_returns_none() {
        // Starts with quote but doesn't end with one
        assert!(parse_etag(r#""abc"#).is_none());
    }

    // =========================================================================
    // If-None-Match Parsing Tests
    // =========================================================================

    #[test]
    fn parse_if_none_match_wildcard() {
        assert_eq!(parse_if_none_match("*"), IfNoneMatch::Any);
    }

    #[test]
    fn parse_if_none_match_single() {
        let result = parse_if_none_match(r#""abc""#);
        match result {
            IfNoneMatch::Tags(tags) => {
                assert_eq!(tags.len(), 1);
                assert_eq!(tags[0].value, "abc");
                assert!(!tags[0].weak);
            }
            IfNoneMatch::Any => panic!("Expected Tags"),
        }
    }

    #[test]
    fn parse_if_none_match_multiple() {
        let result = parse_if_none_match(r#""foo", "bar", W/"baz""#);
        match result {
            IfNoneMatch::Tags(tags) => {
                assert_eq!(tags.len(), 3);
                assert_eq!(tags[0].value, "foo");
                assert!(!tags[0].weak);
                assert_eq!(tags[1].value, "bar");
                assert!(!tags[1].weak);
                assert_eq!(tags[2].value, "baz");
                assert!(tags[2].weak);
            }
            IfNoneMatch::Any => panic!("Expected Tags"),
        }
    }

    #[test]
    fn parse_if_none_match_skips_invalid() {
        // Mix of valid and invalid entries
        let result = parse_if_none_match(r#""valid", "", "also-valid""#);
        match result {
            IfNoneMatch::Tags(tags) => {
                assert_eq!(tags.len(), 2);
                assert_eq!(tags[0].value, "valid");
                assert_eq!(tags[1].value, "also-valid");
            }
            IfNoneMatch::Any => panic!("Expected Tags"),
        }
    }

    // =========================================================================
    // ETag Matching Tests (matches_etag)
    // =========================================================================

    #[test]
    fn matches_etag_exact_strong() {
        assert!(matches_etag(r#""abc""#, r#""abc""#));
    }

    #[test]
    fn matches_etag_weak_comparison_both_weak() {
        assert!(matches_etag(r#"W/"abc""#, r#"W/"abc""#));
    }

    #[test]
    fn matches_etag_weak_comparison_mixed() {
        // Weak comparison: W/"abc" matches "abc"
        assert!(matches_etag(r#"W/"abc""#, r#""abc""#));
        assert!(matches_etag(r#""abc""#, r#"W/"abc""#));
    }

    #[test]
    fn matches_etag_wildcard() {
        assert!(matches_etag(r#""anything""#, "*"));
        assert!(matches_etag(r#"W/"anything""#, "*"));
    }

    #[test]
    fn matches_etag_multiple_tags_match() {
        // Stored: "bar", If-None-Match: "foo", "bar", "baz"
        assert!(matches_etag(r#""bar""#, r#""foo", "bar", "baz""#));
    }

    #[test]
    fn matches_etag_multiple_tags_no_match() {
        // Stored: "qux", If-None-Match: "foo", "bar", "baz"
        assert!(!matches_etag(r#""qux""#, r#""foo", "bar", "baz""#));
    }

    #[test]
    fn matches_etag_no_match() {
        assert!(!matches_etag(r#""abc""#, r#""xyz""#));
    }

    #[test]
    fn matches_etag_invalid_stored_returns_false() {
        // Invalid stored ETag should not match anything
        assert!(!matches_etag("", r#""abc""#));
        assert!(!matches_etag("", "*"));
    }

    #[test]
    fn matches_etag_empty_if_none_match_returns_false() {
        // Empty If-None-Match has no tags to match
        assert!(!matches_etag(r#""abc""#, ""));
    }
}
