# Task Tracking

This file tracks current work items for agent sessions. Update this file at the start and end of each session.

## Current Sprint

### Phase 1: Storage Layer Refactoring

- [ ] Task 1.1: Expand CacheMeta with HTTP headers (`src/cache.rs`)
- [ ] Task 1.2: Create LookupResult enum (`src/cache.rs`)
- [ ] Task 1.3: Add touch() method for TTL refresh (`src/cache.rs`, `src/storage/durable.rs`)
- [ ] Task 1.4: Update Manifest structure (`src/storage/traits.rs`)

### Phase 2: Abstract Policy Module

- [ ] Task 2.1: Create CachePolicy trait (`src/policy.rs`)
- [ ] Task 2.2: Feature-gate multi-tier code (`src/storage/multi_tier.rs`, `Cargo.toml`)

## Blocked

<!-- Items waiting on external dependencies or decisions -->

## Backlog

<!-- Future work items, prioritized -->

## Completed

<!-- Recently completed items for context. Prune periodically. -->

---

## Review

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
