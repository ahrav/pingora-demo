# MVP Implementation Checklist

Reference: [docs/06-mvp-architecture.md](../docs/06-mvp-architecture.md)

---

## Phase 1: Storage Layer Refactoring

### Task 1.1: Expand CacheMeta with HTTP headers
- [ ] Add `content_type: String` field
- [ ] Add `etag: Option<String>` field
- [ ] Add `last_modified: Option<String>` field
- [ ] Add `cache_control: Option<String>` field
- [ ] Add `created_at: u64` field (Unix timestamp)
- [ ] Add `expires_at: u64` field (Unix timestamp)
- [ ] Add `is_fresh(&self) -> bool` method
- [ ] Remove/deprecate `ttl: Duration` field
- [ ] Update all usages of CacheMeta

**File**: `src/cache.rs`

### Task 1.2: Create LookupResult enum
- [ ] Define `LookupResult` enum
  ```rust
  pub enum LookupResult {
      Fresh { meta: CacheMeta, body_key: String },
      Stale { meta: CacheMeta, body_key: String },
      Miss,
  }
  ```
- [ ] Update `Storage::lookup()` return type
- [ ] Update all implementations of `Storage::lookup()`

**File**: `src/cache.rs`

### Task 1.3: Add touch() method for TTL refresh
- [ ] Add `touch(&self, key: &CacheKey, new_expiry: u64) -> Result<()>` to `Storage` trait
- [ ] Implement `touch()` in `DurableTier` using `IndexStore::touch_ttl()`
- [ ] Add tests for touch functionality

**Files**: `src/cache.rs`, `src/storage/durable.rs`

### Task 1.4: Update Manifest structure
- [ ] Expand `Manifest` to store HTTP headers (not just `meta_blob`)
- [ ] Ensure `expires_at` remains first-class field
- [ ] Update serialization/deserialization
- [ ] Update `InMemoryIndexStore` test implementation

**File**: `src/storage/traits.rs`

---

## Phase 2: Abstract Policy Module

### Task 2.1: Create CachePolicy trait
- [ ] Define `CachePolicy` trait:
  ```rust
  pub trait CachePolicy: Send + Sync {
      fn should_cache(&self, meta: &CacheMeta) -> bool;
      fn max_size(&self) -> Option<usize>;
  }
  ```
- [ ] Implement `NoOpPolicy` (caches everything)
- [ ] Update existing code to use trait

**File**: `src/policy.rs`

### Task 2.2: Feature-gate multi-tier code
- [ ] Add `#[cfg(feature = "multi-tier")]` to `MultiTier` struct
- [ ] Add `#[cfg(feature = "multi-tier")]` to promotion wrappers
- [ ] Update `Cargo.toml` with feature flag
- [ ] Ensure MVP compiles without `multi-tier` feature

**File**: `src/storage/multi_tier.rs`, `Cargo.toml`

---

## Phase 3: Pingora Proxy Integration

### Task 3.1: Create CacheDecision enum
- [ ] Define `CacheDecision` enum:
  ```rust
  pub enum CacheDecision {
      ServeFromCache { meta: CacheMeta, body_key: String },
      NotModified { meta: CacheMeta },
      RevalidateWithOrigin { stale: Option<CacheMeta> },
      FetchFromOrigin,
  }
  ```
- [ ] Implement `CacheDecision::from_lookup()`

**File**: `src/proxy.rs` (new)

### Task 3.2: Implement conditional matching
- [ ] Implement `matches_etag()`:
  - [ ] Parse If-None-Match header
  - [ ] Handle comma-separated ETags
  - [ ] Handle weak ETags (W/"...")
  - [ ] Handle wildcard (*)
- [ ] Implement `matches_if_modified_since()`:
  - [ ] Parse If-Modified-Since header
  - [ ] Parse HTTP date format
  - [ ] Compare timestamps

**File**: `src/proxy.rs`

### Task 3.3: Implement ProxyHttp trait
- [ ] Implement `request_filter()`:
  - [ ] Cache lookup
  - [ ] Apply CacheDecision
  - [ ] Early return for hits/304s
- [ ] Implement `upstream_request_filter()`:
  - [ ] Inject If-None-Match header
  - [ ] Inject If-Modified-Since header
- [ ] Implement `response_filter()`:
  - [ ] Handle origin 304 (call touch())
  - [ ] Handle origin errors (stale-if-error)
- [ ] Implement `response_body_filter()`:
  - [ ] Store new responses in cache

**File**: `src/proxy.rs`

---

## Phase 4: Request Flow Testing

### Task 4.1: Unit tests for conditional matching
- [ ] Test ETag exact match
- [ ] Test ETag weak comparison
- [ ] Test ETag wildcard
- [ ] Test multiple ETags in header
- [ ] Test If-Modified-Since comparison
- [ ] Test edge cases (missing headers, invalid formats)

**File**: `tests/conditional.rs` (new)

### Task 4.2: Integration tests for request flows
- [ ] Test: Fresh hit → 200
- [ ] Test: Miss → origin fetch → store → 200
- [ ] Test: Conditional hit (ETag match) → 304
- [ ] Test: Conditional hit (IMS match) → 304
- [ ] Test: Stale → revalidate → origin 304 → touch → 200
- [ ] Test: Stale → revalidate → origin 200 → re-store → 200
- [ ] Test: Stale → origin error → stale-if-error → 200 + Warning
- [ ] Test: PURGE → invalidate → 200/404

**File**: `tests/proxy.rs` (new)

---

## Phase 5: Documentation

### Task 5.1: Update existing docs
- [ ] Update README with MVP architecture
- [ ] Update docs/README.md index
- [ ] Archive/update multi-tier docs to reflect feature-gating

### Task 5.2: Create new docs
- [ ] Create `docs/07-request-flows.md` with detailed flow diagrams
- [ ] Create `docs/08-future-multi-tier.md` explaining how to re-enable

---

## Validation

### Build & Test Commands
```bash
# Build (should work without multi-tier feature)
cargo build

# Run all tests
cargo test

# Run clippy
cargo clippy --all-targets --all-features

# Format
cargo fmt
```

### Final Checklist
- [ ] `cargo build` succeeds
- [ ] `cargo test` passes all tests
- [ ] `cargo clippy` reports no warnings
- [ ] All 8 request flows work correctly
- [ ] Documentation is complete

---

## Notes

- Keep changes minimal and focused
- Preserve multi-tier code behind feature flag (don't delete)
- Storage layer should NOT know about HTTP conditional semantics
- Proxy layer handles all HTTP-specific logic
- Policy abstraction enables future memory tier support
