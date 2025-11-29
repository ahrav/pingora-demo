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
// If-Modified-Since Conditional Matching (RFC 7232)
// =============================================================================

/// Parse an HTTP-date string into a Unix timestamp.
///
/// Supports the three HTTP date formats per RFC 7231 Section 7.1.1.1:
/// - IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT` (preferred)
/// - RFC 850: `Sunday, 06-Nov-94 08:49:37 GMT` (obsolete)
/// - asctime: `Sun Nov  6 08:49:37 1994` (obsolete)
///
/// Returns `None` if the date cannot be parsed.
pub fn parse_http_date(s: &str) -> Option<u64> {
    let s = s.trim();

    // Try IMF-fixdate first (most common): "Sun, 06 Nov 1994 08:49:37 GMT"
    if let Some(ts) = parse_imf_fixdate(s) {
        return Some(ts);
    }

    // Try RFC 850 format: "Sunday, 06-Nov-94 08:49:37 GMT"
    if let Some(ts) = parse_rfc850_date(s) {
        return Some(ts);
    }

    // Try asctime format: "Sun Nov  6 08:49:37 1994"
    if let Some(ts) = parse_asctime_date(s) {
        return Some(ts);
    }

    None
}

/// Parse IMF-fixdate format: "Sun, 06 Nov 1994 08:49:37 GMT"
fn parse_imf_fixdate(s: &str) -> Option<u64> {
    // Format: "Day, DD Mon YYYY HH:MM:SS GMT"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" {
        return None;
    }

    // parts[0] = "Sun," (day-name with comma)
    // parts[1] = "06" (day)
    // parts[2] = "Nov" (month)
    // parts[3] = "1994" (year)
    // parts[4] = "08:49:37" (time)
    // parts[5] = "GMT"

    let day: u32 = parts[1].parse().ok()?;
    let month = month_from_str(parts[2])?;
    let year: i32 = parts[3].parse().ok()?;
    let (hour, min, sec) = parse_time(parts[4])?;

    timestamp_from_components(year, month, day, hour, min, sec)
}

/// Parse RFC 850 format: "Sunday, 06-Nov-94 08:49:37 GMT"
fn parse_rfc850_date(s: &str) -> Option<u64> {
    // Format: "Dayname, DD-Mon-YY HH:MM:SS GMT"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 4 || parts[3] != "GMT" {
        return None;
    }

    // parts[0] = "Sunday," (day-name with comma)
    // parts[1] = "06-Nov-94" (date)
    // parts[2] = "08:49:37" (time)
    // parts[3] = "GMT"

    let date_parts: Vec<&str> = parts[1].split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }

    let day: u32 = date_parts[0].parse().ok()?;
    let month = month_from_str(date_parts[1])?;
    let year_short: i32 = date_parts[2].parse().ok()?;

    // Convert 2-digit year: 00-99 -> assume 1970-2069 range
    let year = if year_short >= 70 {
        1900 + year_short
    } else {
        2000 + year_short
    };

    let (hour, min, sec) = parse_time(parts[2])?;

    timestamp_from_components(year, month, day, hour, min, sec)
}

/// Parse asctime format: "Sun Nov  6 08:49:37 1994"
fn parse_asctime_date(s: &str) -> Option<u64> {
    // Format: "Day Mon DD HH:MM:SS YYYY" (DD may have leading space)
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }

    // parts[0] = "Sun" (day-name)
    // parts[1] = "Nov" (month)
    // parts[2] = "6" (day, no leading zero)
    // parts[3] = "08:49:37" (time)
    // parts[4] = "1994" (year)

    let month = month_from_str(parts[1])?;
    let day: u32 = parts[2].parse().ok()?;
    let (hour, min, sec) = parse_time(parts[3])?;
    let year: i32 = parts[4].parse().ok()?;

    timestamp_from_components(year, month, day, hour, min, sec)
}

/// Parse time string "HH:MM:SS" into components.
fn parse_time(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hour: u32 = parts[0].parse().ok()?;
    let min: u32 = parts[1].parse().ok()?;
    let sec: u32 = parts[2].parse().ok()?;

    // Validate ranges
    if hour > 23 || min > 59 || sec > 60 {
        // sec can be 60 for leap seconds
        return None;
    }

    Some((hour, min, sec))
}

/// Convert month abbreviation to 1-based month number.
fn month_from_str(s: &str) -> Option<u32> {
    match s.to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

/// Convert date/time components to Unix timestamp.
///
/// Uses a simplified algorithm that handles dates from 1970 onwards.
fn timestamp_from_components(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
) -> Option<u64> {
    // Validate basic ranges
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Days in each month (non-leap year)
    const DAYS_IN_MONTH: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    // Check if leap year
    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let max_day = if month == 2 && is_leap {
        29
    } else {
        DAYS_IN_MONTH[(month - 1) as usize]
    };

    if day > max_day {
        return None;
    }

    // Calculate days since Unix epoch (1970-01-01)
    let mut days: i64 = 0;

    // Add days for complete years
    for y in 1970..year {
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        days += if leap { 366 } else { 365 };
    }

    // Add days for complete months in current year
    for m in 1..month {
        days += DAYS_IN_MONTH[(m - 1) as usize] as i64;
        if m == 2 && is_leap {
            days += 1;
        }
    }

    // Add days in current month
    days += (day - 1) as i64;

    // Convert to seconds
    let timestamp = days * 86400 + (hour as i64) * 3600 + (min as i64) * 60 + (sec as i64);

    if timestamp < 0 {
        return None;
    }

    Some(timestamp as u64)
}

/// Check if a stored Last-Modified timestamp matches an If-Modified-Since header.
///
/// Per RFC 7232 Section 3.3, returns `true` (meaning "not modified") if:
/// - The stored Last-Modified date is **equal to or earlier than** the
///   If-Modified-Since date
///
/// Returns `false` if:
/// - The stored Last-Modified date is later than If-Modified-Since
/// - Either date cannot be parsed
/// - Either value is empty
///
/// # Arguments
/// * `stored_last_modified` - The Last-Modified header from the cached response
/// * `if_modified_since` - The If-Modified-Since header from the client request
pub fn matches_if_modified_since(stored_last_modified: &str, if_modified_since: &str) -> bool {
    let stored_ts = match parse_http_date(stored_last_modified) {
        Some(ts) => ts,
        None => return false,
    };

    let client_ts = match parse_http_date(if_modified_since) {
        Some(ts) => ts,
        None => return false,
    };

    // If stored Last-Modified <= client If-Modified-Since, content is not modified
    stored_ts <= client_ts
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

impl CacheDecision {
    /// Determine the cache decision based on lookup result and client conditional headers.
    ///
    /// Implements RFC 7232 conditional request semantics with the constraint that
    /// conditionals are only honored for **fresh** entries ("freshness before conditionals").
    ///
    /// # Decision Logic
    ///
    /// | Lookup State | Conditionals | Decision |
    /// |--------------|--------------|----------|
    /// | Fresh | If-None-Match matches | NotModified |
    /// | Fresh | If-Modified-Since matches (no INM) | NotModified |
    /// | Fresh | No match / no conditionals | ServeFromCache |
    /// | Stale | Any | RevalidateWithOrigin |
    /// | Miss | Any | FetchFromOrigin |
    ///
    /// Per RFC 7232 Section 6, If-None-Match takes precedence over If-Modified-Since.
    ///
    /// # Arguments
    /// * `lookup` - The result of the cache lookup
    /// * `if_none_match` - The If-None-Match header value from the client request
    /// * `if_modified_since` - The If-Modified-Since header value from the client request
    pub fn from_lookup(
        lookup: LookupResult,
        if_none_match: Option<&str>,
        if_modified_since: Option<&str>,
    ) -> Self {
        match lookup {
            LookupResult::Fresh { meta, hit } => {
                // Check If-None-Match first (has precedence per RFC 7232 Section 6)
                let etag_matches = if_none_match
                    .zip(meta.etag.as_ref())
                    .is_some_and(|(inm, stored)| matches_etag(stored, inm));

                if etag_matches {
                    return CacheDecision::NotModified { meta };
                }

                // Check If-Modified-Since (only if no If-None-Match matched)
                let ims_matches = if_modified_since
                    .zip(meta.last_modified.as_ref())
                    .is_some_and(|(ims, stored)| matches_if_modified_since(stored, ims));

                if ims_matches {
                    return CacheDecision::NotModified { meta };
                }

                // No conditional match - serve from cache
                CacheDecision::ServeFromCache { meta, hit }
            }

            LookupResult::Stale { meta, hit: _ } => {
                // Stale entries always trigger revalidation (hit handler dropped)
                CacheDecision::RevalidateWithOrigin { stale: Some(meta) }
            }

            LookupResult::Miss => CacheDecision::FetchFromOrigin,
        }
    }
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

    // =========================================================================
    // HTTP Date Parsing Tests (parse_http_date)
    // =========================================================================

    #[test]
    fn parse_http_date_imf_fixdate() {
        // Standard IMF-fixdate format (preferred)
        let ts = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        // 1994-11-06 08:49:37 UTC = 784111777
        assert_eq!(ts, 784111777);
    }

    #[test]
    fn parse_http_date_rfc850() {
        // RFC 850 format (obsolete)
        let ts = parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT").unwrap();
        assert_eq!(ts, 784111777);
    }

    #[test]
    fn parse_http_date_asctime() {
        // asctime format (obsolete)
        let ts = parse_http_date("Sun Nov  6 08:49:37 1994").unwrap();
        assert_eq!(ts, 784111777);
    }

    #[test]
    fn parse_http_date_epoch() {
        // Unix epoch
        let ts = parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT").unwrap();
        assert_eq!(ts, 0);
    }

    #[test]
    fn parse_http_date_y2k() {
        // Y2K date
        let ts = parse_http_date("Sat, 01 Jan 2000 00:00:00 GMT").unwrap();
        // 2000-01-01 00:00:00 UTC = 946684800
        assert_eq!(ts, 946684800);
    }

    #[test]
    fn parse_http_date_leap_year_feb29() {
        // February 29 in a leap year (2000)
        let ts = parse_http_date("Tue, 29 Feb 2000 12:00:00 GMT").unwrap();
        // 2000-02-29 12:00:00 UTC
        assert_eq!(ts, 951825600);
    }

    #[test]
    fn parse_http_date_non_leap_year_feb29_invalid() {
        // February 29 in a non-leap year (1999) - should fail
        assert!(parse_http_date("Mon, 29 Feb 1999 12:00:00 GMT").is_none());
    }

    #[test]
    fn parse_http_date_rfc850_2digit_year_2000s() {
        // RFC 850 with 2-digit year in 2000s
        let ts = parse_http_date("Wednesday, 15-Mar-23 10:30:00 GMT").unwrap();
        // 2023-03-15 10:30:00 UTC
        assert_eq!(ts, 1678876200);
    }

    #[test]
    fn parse_http_date_with_whitespace() {
        // Leading/trailing whitespace
        let ts = parse_http_date("  Sun, 06 Nov 1994 08:49:37 GMT  ").unwrap();
        assert_eq!(ts, 784111777);
    }

    #[test]
    fn parse_http_date_invalid_format() {
        assert!(parse_http_date("").is_none());
        assert!(parse_http_date("not a date").is_none());
        assert!(parse_http_date("2024-01-01").is_none()); // ISO format not supported
    }

    #[test]
    fn parse_http_date_invalid_month() {
        assert!(parse_http_date("Sun, 06 Foo 1994 08:49:37 GMT").is_none());
    }

    #[test]
    fn parse_http_date_invalid_day() {
        // Day 32 is invalid
        assert!(parse_http_date("Sun, 32 Nov 1994 08:49:37 GMT").is_none());
    }

    #[test]
    fn parse_http_date_invalid_time() {
        // Hour 25 is invalid
        assert!(parse_http_date("Sun, 06 Nov 1994 25:49:37 GMT").is_none());
    }

    #[test]
    fn parse_http_date_pre_epoch() {
        // Date before Unix epoch
        assert!(parse_http_date("Wed, 01 Jan 1969 00:00:00 GMT").is_none());
    }

    #[test]
    fn parse_http_date_missing_gmt() {
        // Missing GMT suffix for IMF-fixdate
        assert!(parse_http_date("Sun, 06 Nov 1994 08:49:37").is_none());
    }

    // =========================================================================
    // If-Modified-Since Matching Tests (matches_if_modified_since)
    // =========================================================================

    #[test]
    fn matches_ims_exact_same_date() {
        // Same date should match (not modified)
        assert!(matches_if_modified_since(
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT"
        ));
    }

    #[test]
    fn matches_ims_stored_earlier() {
        // Stored is earlier than client's IMS - not modified
        assert!(matches_if_modified_since(
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Mon, 07 Nov 1994 08:49:37 GMT"
        ));
    }

    #[test]
    fn matches_ims_stored_later() {
        // Stored is later than client's IMS - modified
        assert!(!matches_if_modified_since(
            "Mon, 07 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT"
        ));
    }

    #[test]
    fn matches_ims_different_formats() {
        // IMF-fixdate stored, RFC 850 client (same actual time)
        assert!(matches_if_modified_since(
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT"
        ));
    }

    #[test]
    fn matches_ims_invalid_stored() {
        // Invalid stored Last-Modified - returns false
        assert!(!matches_if_modified_since(
            "invalid date",
            "Sun, 06 Nov 1994 08:49:37 GMT"
        ));
    }

    #[test]
    fn matches_ims_invalid_client() {
        // Invalid client If-Modified-Since - returns false
        assert!(!matches_if_modified_since(
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "invalid date"
        ));
    }

    #[test]
    fn matches_ims_empty_strings() {
        assert!(!matches_if_modified_since("", ""));
        assert!(!matches_if_modified_since("", "Sun, 06 Nov 1994 08:49:37 GMT"));
        assert!(!matches_if_modified_since("Sun, 06 Nov 1994 08:49:37 GMT", ""));
    }

    #[test]
    fn matches_ims_one_second_difference() {
        // One second later should not match
        assert!(!matches_if_modified_since(
            "Sun, 06 Nov 1994 08:49:38 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT"
        ));

        // One second earlier should match
        assert!(matches_if_modified_since(
            "Sun, 06 Nov 1994 08:49:36 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT"
        ));
    }

    #[test]
    fn matches_ims_leap_second() {
        // Leap second (sec=60) should be parsed
        let ts = parse_http_date("Sat, 31 Dec 2016 23:59:60 GMT");
        assert!(ts.is_some());
    }

    // =========================================================================
    // CacheDecision::from_lookup() Tests
    // =========================================================================

    use crate::cache::CacheResult;
    use async_trait::async_trait;
    use std::time::Duration;

    /// Mock HitHandler for testing
    struct MockHitHandler;

    #[async_trait]
    impl HitHandler for MockHitHandler {
        async fn read_body(&mut self) -> CacheResult<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn finish(self: Box<Self>) -> CacheResult<()> {
            Ok(())
        }
    }

    fn fresh_lookup(meta: CacheMeta) -> LookupResult {
        LookupResult::Fresh {
            meta,
            hit: Box::new(MockHitHandler),
        }
    }

    fn stale_lookup(meta: CacheMeta) -> LookupResult {
        LookupResult::Stale {
            meta,
            hit: Box::new(MockHitHandler),
        }
    }

    fn test_meta() -> CacheMeta {
        CacheMeta {
            content_length: Some(100),
            ttl: Duration::from_secs(3600),
            content_type: Some("text/plain".to_string()),
            etag: Some(r#""abc123""#.to_string()),
            last_modified: Some("Sun, 06 Nov 1994 08:49:37 GMT".to_string()),
            cache_control: None,
            created_at: None,
            expires_at: None,
            version: None,
        }
    }

    #[test]
    fn from_lookup_fresh_with_matching_etag() {
        let lookup = fresh_lookup(test_meta());
        let decision = CacheDecision::from_lookup(lookup, Some(r#""abc123""#), None);
        match decision {
            CacheDecision::NotModified { meta } => {
                assert_eq!(meta.etag, Some(r#""abc123""#.to_string()));
            }
            _ => panic!("Expected NotModified"),
        }
    }

    #[test]
    fn from_lookup_fresh_with_matching_ims() {
        let lookup = fresh_lookup(test_meta());
        // Same date as stored Last-Modified
        let decision =
            CacheDecision::from_lookup(lookup, None, Some("Sun, 06 Nov 1994 08:49:37 GMT"));
        match decision {
            CacheDecision::NotModified { meta } => {
                assert_eq!(
                    meta.last_modified,
                    Some("Sun, 06 Nov 1994 08:49:37 GMT".to_string())
                );
            }
            _ => panic!("Expected NotModified"),
        }
    }

    #[test]
    fn from_lookup_fresh_etag_takes_precedence() {
        // When both If-None-Match and If-Modified-Since are present,
        // If-None-Match takes precedence per RFC 7232 Section 6
        let lookup = fresh_lookup(test_meta());
        let decision = CacheDecision::from_lookup(
            lookup,
            Some(r#""abc123""#),               // Matching ETag
            Some("Sun, 06 Nov 1980 00:00:00 GMT"), // Would not match (stored is later)
        );
        match decision {
            CacheDecision::NotModified { .. } => {}
            _ => panic!("Expected NotModified (ETag match should take precedence)"),
        }
    }

    #[test]
    fn from_lookup_fresh_no_conditional_match() {
        let lookup = fresh_lookup(test_meta());
        // Non-matching ETag
        let decision = CacheDecision::from_lookup(lookup, Some(r#""xyz789""#), None);
        match decision {
            CacheDecision::ServeFromCache { meta, .. } => {
                assert_eq!(meta.etag, Some(r#""abc123""#.to_string()));
            }
            _ => panic!("Expected ServeFromCache"),
        }
    }

    #[test]
    fn from_lookup_fresh_no_conditionals() {
        let lookup = fresh_lookup(test_meta());
        let decision = CacheDecision::from_lookup(lookup, None, None);
        match decision {
            CacheDecision::ServeFromCache { meta, .. } => {
                assert_eq!(meta.content_length, Some(100));
            }
            _ => panic!("Expected ServeFromCache"),
        }
    }

    #[test]
    fn from_lookup_fresh_missing_stored_etag() {
        let mut meta = test_meta();
        meta.etag = None;
        let lookup = fresh_lookup(meta);
        // If-None-Match provided but no stored ETag - should serve from cache
        let decision = CacheDecision::from_lookup(lookup, Some(r#""abc123""#), None);
        match decision {
            CacheDecision::ServeFromCache { .. } => {}
            _ => panic!("Expected ServeFromCache when stored ETag is missing"),
        }
    }

    #[test]
    fn from_lookup_fresh_missing_stored_last_modified() {
        let mut meta = test_meta();
        meta.last_modified = None;
        let lookup = fresh_lookup(meta);
        // If-Modified-Since provided but no stored Last-Modified - should serve from cache
        let decision =
            CacheDecision::from_lookup(lookup, None, Some("Sun, 06 Nov 1994 08:49:37 GMT"));
        match decision {
            CacheDecision::ServeFromCache { .. } => {}
            _ => panic!("Expected ServeFromCache when stored Last-Modified is missing"),
        }
    }

    #[test]
    fn from_lookup_stale_ignores_conditionals() {
        let lookup = stale_lookup(test_meta());
        // Even with matching conditional headers, stale entries should revalidate
        let decision = CacheDecision::from_lookup(
            lookup,
            Some(r#""abc123""#),                   // Would match if fresh
            Some("Sun, 06 Nov 1994 08:49:37 GMT"), // Would match if fresh
        );
        match decision {
            CacheDecision::RevalidateWithOrigin { stale } => {
                assert!(stale.is_some());
                assert_eq!(stale.unwrap().etag, Some(r#""abc123""#.to_string()));
            }
            _ => panic!("Expected RevalidateWithOrigin"),
        }
    }

    #[test]
    fn from_lookup_stale_no_conditionals() {
        let lookup = stale_lookup(test_meta());
        let decision = CacheDecision::from_lookup(lookup, None, None);
        match decision {
            CacheDecision::RevalidateWithOrigin { stale } => {
                assert!(stale.is_some());
            }
            _ => panic!("Expected RevalidateWithOrigin"),
        }
    }

    #[test]
    fn from_lookup_miss() {
        let decision = CacheDecision::from_lookup(LookupResult::Miss, None, None);
        match decision {
            CacheDecision::FetchFromOrigin => {}
            _ => panic!("Expected FetchFromOrigin"),
        }
    }

    #[test]
    fn from_lookup_miss_ignores_conditionals() {
        // Miss should return FetchFromOrigin regardless of client conditionals
        let decision = CacheDecision::from_lookup(
            LookupResult::Miss,
            Some(r#""abc123""#),
            Some("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        match decision {
            CacheDecision::FetchFromOrigin => {}
            _ => panic!("Expected FetchFromOrigin"),
        }
    }

    #[test]
    fn from_lookup_fresh_wildcard_if_none_match() {
        let lookup = fresh_lookup(test_meta());
        let decision = CacheDecision::from_lookup(lookup, Some("*"), None);
        match decision {
            CacheDecision::NotModified { .. } => {}
            _ => panic!("Expected NotModified for wildcard If-None-Match"),
        }
    }

    #[test]
    fn from_lookup_fresh_multiple_etags_one_matches() {
        let lookup = fresh_lookup(test_meta());
        // If-None-Match with multiple ETags, one matches stored
        let decision =
            CacheDecision::from_lookup(lookup, Some(r#""foo", "abc123", "bar""#), None);
        match decision {
            CacheDecision::NotModified { .. } => {}
            _ => panic!("Expected NotModified when one ETag matches"),
        }
    }

    #[test]
    fn from_lookup_fresh_ims_earlier_than_stored() {
        let lookup = fresh_lookup(test_meta());
        // Client's IMS is before stored Last-Modified - should serve from cache
        let decision = CacheDecision::from_lookup(
            lookup,
            None,
            Some("Sun, 05 Nov 1994 08:49:37 GMT"), // Earlier than stored
        );
        match decision {
            CacheDecision::ServeFromCache { .. } => {}
            _ => panic!("Expected ServeFromCache when IMS is earlier"),
        }
    }

    #[test]
    fn from_lookup_fresh_weak_strong_etag_mix() {
        // Weak ETag stored, client sends strong - should match via weak comparison
        let mut meta = test_meta();
        meta.etag = Some(r#"W/"abc123""#.to_string());
        let lookup = fresh_lookup(meta);
        let decision = CacheDecision::from_lookup(lookup, Some(r#""abc123""#), None);
        match decision {
            CacheDecision::NotModified { .. } => {}
            _ => panic!("Expected NotModified via weak comparison"),
        }
    }
}
