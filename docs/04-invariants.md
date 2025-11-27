# System Invariants

This document captures safety properties and guarantees that must always hold. Violating these invariants indicates a bug. Update this document when adding new components or changing system behavior.

## Memory Safety

| ID | Invariant | Enforced By |
|----|-----------|-------------|
| M1 | L0 byte gate (default 2MB) is never exceeded in-process | `FanoutMiss::write_body` drops handler on oversize |
| M2 | L1 byte gate (default 64MB) is never exceeded | `FanoutMiss::write_body` drops handler on oversize |
| M3 | Async promotion channels are bounded | `bounded(capacity)` in channel construction |
| M4 | Backpressure handled via drop, not block | `try_send` + drop on full |

## Correctness

| ID | Invariant | Enforced By |
|----|-----------|-------------|
| C1 | Promotion failures never affect client response | Best-effort semantics in hit wrappers |
| C2 | Source `HitHandler::finish()` called before promotion finalize | Wrapper sequencing in `HitFromLower` |
| C3 | Manifest updates are atomic (no partial states) | CAS semantics in `IndexStore::cas_update` |
| C4 | Dropped handlers never call `finish()` | `take()` + drop pattern |

## Concurrency

| ID | Invariant | Enforced By |
|----|-----------|-------------|
| CON1 | Concurrent writes to same key use CAS | `IndexStore::cas_update` with version check |
| CON2 | Async workers are lazily spawned | First-chunk spawn pattern |
| CON3 | L0 promotions are always synchronous | Direct writes, no channel |
| CON4 | L1/L2 promotions are always asynchronous | Worker + channel pattern |

## Performance

| ID | Invariant | Enforced By |
|----|-----------|-------------|
| P1 | L0 hit is zero-copy hot path | No wrapper, direct return |
| P2 | Async promotions never block TTFB | Channel + worker isolation |
| P3 | `Never` policy has zero overhead | No worker spawned |

## Data Integrity

| ID | Invariant | Enforced By |
|----|-----------|-------------|
| D1 | L3 is authoritative source of truth | Manifest in IndexStore |
| D2 | Blob and manifest are consistent | Write blob before manifest |
| D3 | TTL expiration handled by IndexStore | `touch_ttl` on access |

---

## Adding New Invariants

When adding new components or changing behavior:

1. Identify what properties MUST hold
2. Document how they are enforced
3. Add tests that verify the invariant
4. Consider failure modes that could violate it

## Violation Response

If an invariant is violated:

1. The bug is in the enforcement mechanism, not the invariant
2. Fix the code, don't weaken the invariant
3. Add a regression test
