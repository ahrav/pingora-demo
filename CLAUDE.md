# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Session Workflow

### Starting a Session
1. Read `tasks/todo.md` for current work items
2. Run `git status` to check for uncommitted work
3. Run `cargo check` to verify compilation
4. Select ONE task from todo.md to focus on

### Implementation Loop
For each task:
1. Read relevant source files first
2. Make smallest possible change
3. Run: `cargo build && cargo test && cargo clippy --all-targets --all-features`
4. Commit with descriptive message
5. Mark task complete in todo.md

### Ending a Session
1. Update `tasks/todo.md` - mark completed, add discovered items
2. Add Review entry with: what was done, decisions, issues, what's next
3. Ensure code compiles and tests pass
4. Commit any uncommitted work

### Principles
- Make every change as simple as possible
- Avoid massive or complex changes - each change should impact minimal code
- Check in with the user before beginning significant work
- Provide high-level explanations of changes made

## Build and Test Commands

```bash
# Build
cargo build

# Run tests
cargo test

# Run a single test
cargo test test_name

# Run tests with output
cargo test -- --nocapture

# Check without building
cargo check

# Format code
cargo fmt

# Lint
cargo clippy
```

## Architecture Overview

This is a multi-tier caching system inspired by Pingora's cache architecture. It implements a four-tier cache hierarchy with policy-driven promotions between tiers.

### Four-Tier Hierarchy

- **L0**: In-process memory cache (default 2MB gate) - fastest, smallest
- **L1**: Distributed cache (default 64MB gate) - fast network tier
- **L2**: Mid-latency cache (unlimited) - optional intermediate tier
- **L3**: Durable storage (unlimited) - blob store + index store composition

### Core Modules

**`src/cache.rs`** - Core cache abstractions:
- `Storage` trait: main interface all tiers implement (`lookup`, `get_miss_handler`, `purge`, `update_meta`)
- `HitHandler` trait: streaming reads from cache hits
- `MissHandler` trait: streaming writes on cache misses
- `CacheKey`, `CacheMeta`, `CacheError` types

**`src/policy.rs`** - Tier promotion configuration:
- `Policy` struct: byte gates (`l0_max`, `l1_max`, `l2_max`) and promotion rules
- `Promotion` enum: `Never` or `OnStreamCommitAtFinish`
- Controls which tiers receive data based on content length

**`src/storage/multi_tier.rs`** - Multi-tier orchestrator:
- `MultiTier` struct: composes L0/L1/L2/L3 with policy-driven fanout
- `HitFromL1PromoteL0`: wrapper for L1 hits promoting to L0 (sync)
- `HitFromLower`: wrapper for L2/L3 hits promoting to upper tiers (L0 sync, L1/L2 async)
- `FanoutMiss`: multiplexes writes to multiple tiers, drops oversized handlers mid-stream

**`src/storage/traits.rs`** - Durable storage abstractions:
- `BlobStore` trait: object storage operations (`get`, `head`, `put`, `delete`)
- `IndexStore` trait: manifest CRUD with CAS support (`get`, `put_new`, `cas_update`, `delete`, `touch_ttl`)
- `BlobRead`/`BlobWrite` traits: streaming blob I/O
- `Manifest` struct: per-key metadata pointing to blob location

**`src/storage/durable.rs`** - L3 durable tier implementation:
- `DurableTier<B: BlobStore, I: IndexStore>`: composes blob and index stores
- Implements `Storage` trait by coordinating blob reads/writes with manifest management

**`src/storage/memory.rs`** - In-memory implementations for testing:
- `MemoryStore`: simple L0/L1/L2 in-memory cache
- `InMemoryBlobStore`, `InMemoryIndexStore`: test doubles for durable tier

### Key Design Patterns

1. **Promotion is best-effort**: failures don't affect client response path
2. **Byte gates + dynamic dropping**: prevents memory exhaustion on oversized objects
3. **Sync vs async promotions**: L0 is always sync (fast), L1/L2 use async workers with bounded channels
4. **Lazy async workers**: spawned on first chunk, zero overhead when policy is `Never`
5. **CAS semantics**: manifest updates use compare-and-swap for concurrent write safety

## References

- See `docs/05-session-guide.md` for detailed session protocols
- See `tasks/mvp-checklist.md` for full MVP implementation context
