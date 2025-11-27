# Task Tracking

This file tracks current work items for agent sessions. Update this file at the start and end of each session.

## Current Sprint

<!-- All Phase 1 & Phase 2 tasks complete -->

## Blocked

<!-- Items waiting on external dependencies or decisions -->

## Backlog

<!-- Future work items, prioritized -->

## Completed

### Phase 2: Abstract Policy Module

- [x] Task 2.1: Create CachePolicy trait (`src/policy.rs`)
- [x] Task 2.2: Feature-gate multi-tier code (`Cargo.toml`, `src/storage/multi_tier.rs`)

### Phase 1: Storage Layer Refactoring

- [x] Task 1.1: Expand CacheMeta with HTTP headers (commit 0d74ff2)
- [x] Task 1.2: Create LookupResult enum (commit c8f8338)
- [x] Task 1.3: Add touch() method for TTL refresh (commit de85047)
- [x] Task 1.4: Update serialization with versioned binary format (commit 8763872)

---

## Review

### 2025-11-27 - Phase 2 Complete: Abstract Policy Module

**Done:**
- Task 2.1: Created CachePolicy trait with `should_cache()` and `max_size()` methods
- Task 2.1: Implemented NoOpPolicy (always caches, no size limit)
- Task 2.2: Added `[features]` section to Cargo.toml with `multi-tier` as default feature
- Task 2.2: Feature-gated `Policy` struct and `Promotion` enum
- Task 2.2: Feature-gated multi_tier module in storage/mod.rs
- Task 2.2: Updated lib.rs with conditional re-exports

**Decisions:**
- CachePolicy trait is always available (generic abstraction)
- Policy/Promotion only available with multi-tier feature
- NoOpPolicy defers integration with MemoryStore/DurableTier (future work)
- parse_content_length() remains ungated as general utility

**Testing:**
- `cargo build` passes (with multi-tier)
- `cargo build --no-default-features` passes (without multi-tier)
- `cargo test` passes (6 tests with multi-tier)
- `cargo test --no-default-features` passes (4 tests without multi-tier)

**Issues:**
- Minor unused import warnings when building without multi-tier (expected)

**Next:**
- Phase 3: Pingora Proxy Integration

### 2025-11-27 - Phase 1 Complete: Storage Layer Refactoring

**Done:**
- Task 1.1: Expanded CacheMeta with HTTP headers (content_type, etag, last_modified, cache_control)
- Task 1.1: Added created_at/expires_at timestamps for precise freshness tracking
- Task 1.1: Implemented is_fresh() method with fallback logic
- Task 1.2: Created LookupResult enum (Fresh/Stale/Miss) replacing Option<(meta, hit)>
- Task 1.2: Updated all Storage implementations (MemoryStore, DurableTier, MultiTier)
- Task 1.3: Added touch() method to Storage trait for TTL refresh
- Task 1.3: Implemented touch() in all storage tiers
- Task 1.4: Created versioned binary serialization (Version 1 with backward compat for Version 0)
- Task 1.4: Added comprehensive tests for all new functionality

**Decisions:**
- LookupResult treats both Fresh and Stale as promotable hits in MultiTier
- touch() returns bool (true if key exists and was updated)
- Serialization uses u16 length prefixes for variable-length strings
- Version 0 detection: exact 17-byte length or version byte 0
- All new fields default to None for backward compatibility

**Testing:**
- All 6 tests pass (durable_round_trip, durable_respects_ttl, metadata_serialization_roundtrip, touch_extends_ttl, fanout_drops_oversize_l0, promotion_is_best_effort)
- Verified backward compatibility with old 17-byte format
- Verified full HTTP header roundtrip with new format

**Issues:**
- Pre-existing clippy warnings (collapsible_if, type_complexity, field_reassign_with_default)
- Not addressed as they are pre-existing style issues

**Next:**
- Phase 2: Abstract Policy Module (Task 2.1, 2.2)

### 2025-11-27 - Workflow Integration

**Done:**
- Updated `CLAUDE.md` with merged session workflow
- Converted Phase 1 & 2 MVP tasks to trackable format in this file

**Decisions:**
- Task-level granularity (1.1, 1.2, etc.) - subtask details in `mvp-checklist.md`
- Merged CLAUDE.md with session guide for unified workflow
- Kept `mvp-checklist.md` as reference for full implementation context

**Next:**
- Begin Task 1.1: Expand CacheMeta with HTTP headers

### 2025-11-27 - Agent Workflow Scaffolding

**Done:**
- Created `tasks/todo.md` for task tracking
- Created `docs/04-invariants.md` with system safety properties
- Created `docs/05-session-guide.md` with session protocols
- Updated `docs/README.md` to reference new files

**Decisions:**
- Followed existing numbered doc convention (04-, 05-)
- Pre-populated invariants based on existing architecture docs
- Kept templates minimal - fill in as needed

**Next:**
- Refine invariants as implementation progresses
- Add module ownership details to session guide as codebase grows
