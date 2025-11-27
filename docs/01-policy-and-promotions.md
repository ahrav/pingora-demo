# Multi-Tier Cache Policy and Promotions

## Core mapping of Policy → behavior

* `l0_max/l1_max/l2_max`: byte gates. If `Content-Length > gate`, you skip constructing a writer for that tier and never promote/admit there. Unknown length → start writing and drop that tier if you cross the gate mid-stream.
* `Promotion::{Never, OnStreamCommitAtFinish}`:

  * `Never`: no promotion to that target tier on a hit from the source tier.
  * `OnStreamCommitAtFinish`: write chunks to the target tier during `read_body`; call `finish()` on the target tier after the source hit's `finish()` completes. Best-effort and non-blocking.

## When promotions happen (by source tier)

* L0 hit:

  * No promotions. Hot path returns the inner hit unchanged.

* L1 hit:

  * `promote_from_l1_to_l0` checked.
  * If `OnStreamCommitAtFinish` and `len ≤ l0_max`, construct `l0_miss` and stream to L0 during `read_body`. Commit at `finish()`.
  * Wrapper: `HitFromL1PromoteL0`.

* L2 hit:

  * `promote_from_l2_to_l0` checked. If `OnStreamCommitAtFinish` and `len ≤ l0_max`, do synchronous L0 streaming via `l0_miss`.
  * `promote_from_l2_to_l1` checked. If `OnStreamCommitAtFinish` and `len ≤ l1_max`, lazily start an async writer to L1 on the first chunk; send chunks over a bounded channel; await the worker in `finish()`.
  * Wrapper: `HitFromLower` with L0 sync + L1 async lanes.

* L3 hit (DDB+S3 as you've modeled it):

  * `promote_from_l3_to_l0`, `promote_from_l3_to_l1`, `promote_from_l3_to_l2` each checked in the same way with their gates.
  * L0 is sync; L1/L2 are async and lazily started.
  * Wrapper: the same `HitFromLower` drives any combination of L0/L1/L2 promotions.

## Why "async lanes" for L1/L2

* You keep client TTFB and throughput dominated by the source tier.
* Promotions to slower or network tiers cannot stall the hot read path.
* The bounded channel plus "drop on full" avoids backpressure explosions. You count drops for tuning.

## Admission on MISS (origin fetch path)

* `get_miss_handler` builds one `FanoutMiss` that holds up to four per-tier `MissHandler`s, gated by `Content-Length` and `*_max`.
* During `write_body`, each target tier receives the chunk if still under its gate; once the running total exceeds a gate you `take()` and drop that tier's handler (no `finish()` → admission fails cleanly).
* At `finish`, you call `finish()` on the surviving handlers and return `Success` only if all surviving handlers succeeded. Durable L3 is usually always present.

## Do you need a `HandleHit` per tier or per promotion variant?

* No.
* Minimal set in your code is correct:

  * `HitFromL1PromoteL0` covers the only L1 source case with optional L0 promotion.
  * `HitFromLower` is a single generic wrapper for "source is below L1" (L2 or L3) and can fan out to L0 (sync) and L1/L2 (async) based on policy bits.

* You only introduce new wrappers if you add a truly different promotion mode (e.g., `OnFinish` with real spooling, or "background re-fetch" semantics). You do not need per-edge classes.

## Do you need a `HandleMiss` per tier or per variant?

* No.
* One `FanoutMiss` is sufficient. It multiplexes to L0/L1/L2/L3 based on gates and policy. Dropping a handler on oversize is the correct failure signal.

## Why the defaults in `Policy::default()`

* Gates: `l0_max=2 MiB`, `l1_max=64 MiB`, `l2_max=∞` align with "L0 for tiny, L1 for medium, L2/L3 for everything."
* Promotions default to `OnStreamCommitAtFinish` so hot objects pulled from cold tiers warm upper tiers immediately without adding extra RTT. You can turn individual edges off with `Never`.

## What happens when `Content-Length` is unknown

* You create the target `MissHandler` optimistically if policy says promote/admit.
* You enforce the gate in `FanoutMiss::write_body` or in the hit wrappers as chunks arrive.
* Crossing the gate drops that tier's writer mid-stream. Other lanes continue.

## Failure semantics on promotions

* Hit wrappers always:

  * Finish the source `HitHandler` first (Pingora contract).
  * Then finalize promotions. Async lanes are joined; sync `l0_miss` is finished.
  * Promotion failures are best-effort; they don't fail the client response. You should emit counters.

## Where to challenge and refine

* L3→L2 promotion: only useful if L2 isn't authoritative or you want a byte cache above S3. If L2 is "another cache," keep it. If L2 is "your index," set this to `Never`.
* Consider adding `NeverIfLenUnknown` if you want to avoid opening writers on unknown sizes.
* Add a TinyLFU admission at L0 if you see one-hit wonders evict hot keys even under byte gates.
* If you plan to support HTTP Range from cache, carry `can_seek/seek` through and only enable range service when the source or target supports it.

## Summary answer to your question

* No, you don't need `HandleHit` for every tier×promotion combination, nor separate miss types. Two hit wrappers (`HitFromL1PromoteL0` and the generic `HitFromLower`) plus one miss (`FanoutMiss`) cover all paths your `Policy` expresses. Enums + gates drive the behavior without class explosion.
