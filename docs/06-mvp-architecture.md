# Architecture Comparison: Multi-Tier vs Simplified 2-Tier Design

## Executive Summary

This document compares the current 4-tier multi-tier caching architecture with a proposed simplified 2-tier design focused on K/V metadata + Object storage without the policy mechanism.

---

## 1. Areas of Agreement

### 1.1 Core Abstractions Match

Both designs share fundamental architectural concepts:

| Concept | Current Implementation | Proposed Implementation |
|---------|----------------------|------------------------|
| **Metadata Store** | `IndexStore` trait | `MetaStorage` trait |
| **Object Store** | `BlobStore` trait | `ObjectStorage` trait |
| **Cache Key** | `CacheKey` struct | `CacheKey` struct |
| **Cache Metadata** | `CacheMeta` + `Manifest` | `CacheMeta` struct |
| **Streaming I/O** | `BlobRead`/`BlobWrite` traits | `AsyncRead`-based |

### 1.2 Durable Layer Alignment

The current `DurableTier<B: BlobStore, I: IndexStore>` closely matches the proposed `ImageCache<M: MetaStorage, O: ObjectStorage>`:

```
Current L3 Tier:                    Proposed Design:
┌─────────────────────┐             ┌─────────────────────┐
│  DurableTier<B, I>  │             │  ImageCache<M, O>   │
│  ├─ BlobStore       │      ≈      │  ├─ ObjectStorage   │
│  └─ IndexStore      │             │  └─ MetaStorage     │
└─────────────────────┘             └─────────────────────┘
```

### 1.3 Shared Operations

- **Lookup**: Both check metadata first, then fetch body
- **Store**: Both write to object storage, then metadata
- **Purge**: Both delete from both stores
- **TTL/Expiry**: Both track expiration times

---

## 2. Areas of Disagreement

### 2.1 Tier Architecture

**Current**: 4-tier hierarchy with promotions
```
L0 (in-process) ←─ promote ─┐
       ↑                     │
L1 (distributed) ←─ promote ─┤
       ↑                     │
L2 (mid-latency) ←─ promote ─┤
       ↑                     │
L3 (durable) ←───────────────┘
```

**Proposed**: 2-tier flat structure
```
K/V Store (metadata) ←→ Object Storage (data)
```

**Impact**: Proposed removes ~500 lines of promotion orchestration code.

### 2.2 Policy Mechanism

**Current**: Complex policy with byte gates + 6 promotion edges
```rust
pub struct Policy {
    pub l0_max: usize,              // 2MB gate
    pub l1_max: usize,              // 64MB gate
    pub l2_max: usize,              // unlimited
    pub promote_from_l1_to_l0: Promotion,
    pub promote_from_l2_to_l0: Promotion,
    pub promote_from_l2_to_l1: Promotion,
    pub promote_from_l3_to_l0: Promotion,
    pub promote_from_l3_to_l1: Promotion,
    pub promote_from_l3_to_l2: Promotion,
}
```

**Proposed**: No policy - all objects go to both stores unconditionally.

**Trade-off**: Simplicity vs memory protection for large objects.

### 2.3 HTTP Semantics in CacheMeta

**Current** (minimal):
```rust
pub struct CacheMeta {
    pub content_length: Option<usize>,
    pub ttl: Duration,
}
```

**Proposed** (full HTTP):
```rust
pub struct CacheMeta {
    pub storage_key: String,
    pub headers: CachedHeaders,  // content_type, etag, last_modified, cache_control
    pub created_at: u64,
    pub expires_at: u64,
    pub size: u64,
}
```

**Impact**: Proposed enables conditional request handling (304s), revalidation.

### 2.4 Conditional Request Handling

**Current**: Not implemented at storage layer.

**Proposed**: Full implementation:
- `matches_conditions()` method for ETag/If-Modified-Since
- `LookupResult::NotModified` variant for 304 responses
- Direct 304 return without body fetch

### 2.5 Revalidation Flow

**Current**: Not implemented.

**Proposed**:
- Stale entries trigger origin revalidation
- Origin 304 → `touch()` to refresh TTL
- Origin 200 → `store()` with new content
- Stale-if-error fallback on origin failure

### 2.6 Pingora Integration Level

**Current**: Storage abstraction only (`Storage` trait).

**Proposed**: Full `ProxyHttp` implementation with:
- `request_filter` - cache lookup + early 304/200 return
- `upstream_request_filter` - conditional header injection
- `response_filter` - 304 handling, stale-if-error
- `response_body_filter` - async cache population

---

## 3. MVP Architecture Decision

Based on requirements analysis, the MVP will follow these principles:

1. **2-tier storage**: K/V metadata + Object storage (no memory tiers)
2. **Policy abstraction**: Keep promotion policy logic but make it optional/pluggable for future use
3. **Split HTTP handling**: Freshness/TTL in storage layer, conditional matching (304s) in proxy layer

---

## 4. MVP Target Architecture

### 4.1 High-Level Design

```
                    CDN Request
                         │
                         ▼
              ┌─────────────────────┐
              │   Pingora Proxy     │  ← Conditional request matching (304)
              │   (ProxyHttp impl)  │  ← Stale-if-error handling
              └──────────┬──────────┘  ← Revalidation orchestration
                         │
                         ▼
              ┌─────────────────────┐
              │    ImageCache       │  ← Freshness/TTL checking
              │  (Storage Layer)    │  ← LookupResult enum
              └──────────┬──────────┘
                         │
           ┌─────────────┴─────────────┐
           ↓                           ↓
    ┌─────────────┐             ┌─────────────┐
    │ IndexStore  │             │  BlobStore  │
    │ (metadata)  │             │   (data)    │
    └─────────────┘             └─────────────┘
```

### 4.2 Layer Responsibilities

**Pingora Proxy Layer** (HTTP-aware):
- Parse conditional headers (If-None-Match, If-Modified-Since)
- Match ETags and timestamps against cached metadata
- Return 304 Not Modified when conditions match
- Inject conditional headers for origin revalidation
- Handle origin 304 responses (touch TTL)
- Serve stale content on origin errors (stale-if-error)

**Storage Layer** (HTTP-agnostic freshness):
- Return `LookupResult` with freshness state (Fresh, Stale, Miss)
- Track TTL/expiry times
- Store and retrieve metadata + body
- Provide `touch()` for TTL refresh
- NO knowledge of ETag matching or 304 semantics

**Policy Module** (optional, abstracted):
- Defines tier admission rules (byte gates)
- Defines promotion strategies
- MVP: disabled/no-op implementation
- Future: pluggable memory tier support

### 4.3 Key Data Structures

```rust
// Storage layer - expanded metadata
pub struct CacheMeta {
    pub content_type: String,
    pub content_length: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cache_control: Option<String>,
    pub created_at: u64,      // Unix timestamp
    pub expires_at: u64,      // Unix timestamp (TTL)
}

// Storage layer - lookup result (freshness only)
pub enum LookupResult {
    Fresh { meta: CacheMeta, body_key: String },
    Stale { meta: CacheMeta, body_key: String },
    Miss,
}

// Proxy layer - cache decision (after conditional matching)
pub enum CacheDecision {
    ServeFromCache { meta: CacheMeta, body: Bytes },
    NotModified { meta: CacheMeta },  // 304
    RevalidateWithOrigin { stale: Option<CacheMeta> },
    FetchFromOrigin,
}
```

### 4.4 Conditional Evaluation Order

- **Problem**: Previous flow ran `matches_conditions` before checking freshness/client directives, causing 304s even when the entry was stale or the client sent `Cache-Control: no-cache`.
- **Correct order**:
  1. Fetch metadata/body handle from storage.
  2. Compute `fresh = now < expires_at` (or created_at + ttl) and `must_revalidate = !fresh || client sent Cache-Control: no-cache`.
  3. If `must_revalidate`: skip local 304; go to origin with conditional headers (revalidation/stale-if-error path). Serving stale is a separate policy decision, but never emits 304.
  4. If `fresh` and not `must_revalidate`: evaluate conditionals with RFC ordering—`If-None-Match` (ETag) wins over `If-Modified-Since`; return 304 only when a condition matches, otherwise 200 from cache.

### 4.5 Invalidation vs concurrent fills (gap + mitigation)

- **Current behavior**: `PURGE` deletes the manifest (meta) and then deletes the object. If a miss is already fetching from origin, the in-flight writer will still commit object and meta when it returns, effectively “resurrecting” the just-purged entry.
- **Race example**:
  - A starts miss → fetches origin → writes object → writes meta.
  - While A is in flight, admin issues PURGE → delete meta + object.
  - A completes and rewrites meta/object → stale copy is back.
- **MVP mitigation (no generations)**: add a simple CAS guard using a per-key `version`/`epoch`.
  - Extend `CacheMeta` to carry `version`.
  - Keep a version counter/tombstone in the index (or a separate KV row).
  - Lookup/read captures current version; store attempts `put`/`cas_update` only if `version` is unchanged.
  - PURGE bumps version (or sets tombstone) before deleting, so in-flight fills fail their CAS and drop the write, preventing post-purge resurrection.

---

## 5. Request Flows

### Flow 1: Simple Cache Hit
```
GET /image.jpg (no conditions, fresh cache)
→ lookup() returns Fresh
→ matches_etag() = false (no If-None-Match header)
→ CacheDecision::ServeFromCache
→ Return 200 with cached body
```

### Flow 2: Cache Miss
```
GET /image.jpg (not in cache)
→ lookup() returns Miss
→ CacheDecision::FetchFromOrigin
→ Fetch from origin → store() → Return 200
```

### Flow 3: Conditional Request - ETag Match (304)
```
GET /image.jpg
If-None-Match: "abc123"
→ lookup() returns Fresh { meta.etag = "abc123" }
→ must_revalidate = false (entry is fresh, no Cache-Control: no-cache)
→ matches_etag() = true
→ CacheDecision::NotModified
→ Return 304 (no body fetch needed)
```

### Flow 4: Conditional Request - ETag Mismatch
```
GET /image.jpg
If-None-Match: "old-etag"
→ lookup() returns Fresh { meta.etag = "abc123" }
→ must_revalidate = false (entry is fresh, no Cache-Control: no-cache)
→ matches_etag() = false
→ CacheDecision::ServeFromCache
→ Return 200 with cached body
```

### Flow 5: Stale Revalidation - Origin 304
```
GET /image.jpg (stale entry or client sent Cache-Control: no-cache)
→ lookup() returns Stale, or Fresh with must_revalidate = true
→ CacheDecision::RevalidateWithOrigin { stale: Some(meta) }
→ upstream_request_filter injects If-None-Match header
→ Origin returns 304
→ response_filter calls cache.touch()
→ Return 200 with cached body
```

### Flow 6: Stale Revalidation - Origin 200
```
GET /image.jpg (stale entry or client sent Cache-Control: no-cache)
→ lookup() returns Stale, or Fresh with must_revalidate = true
→ CacheDecision::RevalidateWithOrigin { stale: Some(meta) }
→ upstream_request_filter injects If-None-Match header
→ Origin returns 200 with new content
→ response_filter calls cache.store() with new data
→ Return 200 with new body
```

### Flow 7: Stale-if-error
```
GET /image.jpg (stale cache or must_revalidate, origin fails)
→ lookup() returns Stale, or Fresh with must_revalidate = true
→ CacheDecision::RevalidateWithOrigin { stale: Some(meta) }
→ Origin returns 502/timeout
→ response_filter checks stale-if-error in cache_control
→ Return 200 with stale body + Warning header
```

### Flow 8: Invalidation (PURGE)
```
PURGE /image.jpg
→ cache.invalidate(key)
→ Delete from IndexStore + BlobStore
→ Return 200 OK (or 404 if not found)
```

---

## 6. Files to Create/Modify

| File | Action | Description |
|------|--------|-------------|
| `src/cache.rs` | **Modify** | Expand CacheMeta, add LookupResult enum |
| `src/policy.rs` | **Refactor** | Create CachePolicy trait, NoOpPolicy impl |
| `src/proxy.rs` | **Create** | CacheDecision enum, ProxyHttp implementation |
| `src/storage/traits.rs` | **Modify** | Update Manifest, add touch_ttl |
| `src/storage/durable.rs` | **Modify** | Implement touch(), update for new CacheMeta |
| `src/storage/multi_tier.rs` | **Move** | Feature-gate or move to `_future.rs` |
| `src/lib.rs` | **Modify** | Export new proxy module |
| `tests/proxy.rs` | **Create** | Integration tests for request flows |
| `tests/conditional.rs` | **Create** | Unit tests for ETag/date matching |

---

## 7. Success Criteria

MVP is complete when:
- [ ] All request flows work correctly
- [ ] Conditional requests return 304 without body fetch
- [ ] Cache does not emit 304 when entries are stale or client requests no-cache; ETag takes precedence over If-Modified-Since
- [ ] Stale entries trigger revalidation with conditional headers
- [ ] Origin 304 refreshes TTL without re-storing body
- [ ] Stale-if-error serves stale content on origin failure
- [ ] PURGE resists post-purge resurrection by guarding writes with a version/epoch CAS
- [ ] Policy module is abstracted and pluggable (NoOpPolicy for MVP)
- [ ] Multi-tier code preserved but disabled
- [ ] Tests pass for all request flow scenarios
