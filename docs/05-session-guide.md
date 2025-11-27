# Agent Session Guide

This document defines the workflow for AI-assisted development sessions on this codebase. It ensures context persists across sessions and work proceeds incrementally.

## Session Startup Protocol

Every session begins with these steps:

```
1. pwd                           # Confirm working directory
2. Read tasks/todo.md            # Current work items
3. git log --oneline -10         # Recent changes context
4. git status                    # Any uncommitted work?
5. cargo check                   # Does it compile?
6. cargo test                    # What's passing/failing?
7. Select ONE item from todo.md  # Focus on single task
```

## Implementation Loop

For each task:

```
┌─────────────────────────────────────────┐
│ 1. Read relevant source files           │
│ 2. Make smallest possible change        │
│ 3. cargo build && cargo test            │
│ 4. cargo clippy --all-targets           │
│ 5. Commit with descriptive message      │
│ 6. Repeat until task complete           │
└─────────────────────────────────────────┘
```

### Verification Command

Run after every change:

```bash
cargo build && cargo test && cargo clippy --all-targets --all-features
```

## Session End Protocol

Before ending a session:

1. Update `tasks/todo.md`:
   - Mark completed items with `[x]`
   - Add new items discovered during work
   - Move blocked items with explanation
2. Add entry to Review section with:
   - What was done
   - Key decisions made
   - Issues encountered
   - What's next
3. Ensure code compiles and tests pass
4. Commit any uncommitted work

## Codebase Navigation

### Finding Things

```bash
# Find trait implementations
rg "impl.*TraitName.*for"

# Find all public interfaces in a module
rg "pub fn|pub async fn" src/module.rs

# Find unsafe blocks (review carefully)
rg "unsafe"

# Find tests for a file
rg "#\[test\]" src/file.rs

# Find TODOs
rg "TODO|FIXME"
```

### Module Ownership

| Path | Responsibility |
|------|----------------|
| `src/cache.rs` | Core types, `Storage` trait |
| `src/policy.rs` | Promotion rules, byte gates |
| `src/storage/multi_tier.rs` | Tier orchestration, fanout |
| `src/storage/durable.rs` | L3 blob+index composition |
| `src/storage/traits.rs` | `BlobStore`, `IndexStore` traits |
| `src/storage/memory.rs` | In-memory test implementations |

## Working with Large Changes

For changes spanning multiple files:

1. **Plan first**: Write the plan to `tasks/todo.md` before coding
2. **Get approval**: Check in with user before implementing
3. **Incremental commits**: One logical change per commit
4. **Test continuously**: Don't batch testing to the end

## When to Ask Questions

Use `AskUserQuestion` when:

- Multiple valid approaches exist
- Change affects public API
- Uncertain about performance tradeoffs
- Change might break existing behavior

## Context Recovery

If starting a session with no memory of prior work:

1. Read `tasks/todo.md` Review section for recent history
2. Check `git log --oneline -20` for commit messages
3. Look for `TODO` or `FIXME` comments in recently modified files
4. Read `docs/04-invariants.md` to understand constraints

## Principles

1. **One task at a time**: Complete before moving to next
2. **Smallest change possible**: Avoid scope creep
3. **Test before commit**: Never commit failing code
4. **Document decisions**: Future sessions need context
