# Pingora Multi-Tier Cache Documentation

This directory contains comprehensive documentation for the Pingora-based multi-tier caching system with pluggable storage backends.

## 📚 Core Documentation

### Design Documents

1. **[Policy and Promotions](01-policy-and-promotions.md)**
   - Core mapping of Policy → behavior
   - Promotion strategies (sync vs async)
   - Byte gate admission control
   - Failure semantics and tuning recommendations

2. **[Multi-Tier Implementation](02-multi-tier-implementation.md)**
   - Complete Rust implementation
   - `MultiTier<L0, L1, L2, L3>` storage
   - Hit/Miss handler wrappers
   - Fanout miss logic with oversize protection

3. **[Trait Abstraction Design](03-trait-abstraction-design.md)**
   - Pluggable `BlobStore` and `IndexStore` traits
   - `DurableTier<B, I>` composition
   - Adapter implementations (S3, DynamoDB, CockroachDB)
   - Migration paths and deployment options

4. **[System Invariants](04-invariants.md)**
   - Safety properties that must always hold
   - Memory, correctness, concurrency guarantees
   - Data integrity constraints
   - Enforcement mechanisms

5. **[Agent Session Guide](05-session-guide.md)**
   - Session startup and end protocols
   - Codebase navigation patterns
   - Task tracking workflow
   - Context recovery procedures

### Task Tracking

- **[tasks/todo.md](../tasks/todo.md)** - Current work items, backlog, and session reviews

## 📊 Visual Diagrams

All diagrams are in Mermaid format (`.mmd` files) and can be viewed with:
- [Mermaid Live Editor](https://mermaid.live)
- VS Code with Mermaid extension
- GitHub (renders Mermaid natively)

### Architecture Diagrams

| # | Diagram | Description |
|---|---------|-------------|
| 01 | [Architecture Overview](diagrams/01-architecture-overview.mmd) | Complete system showing L0-L3 tiers with trait abstraction |
| 02 | [Cache Hit Flow](diagrams/02-cache-hit-flow.mmd) | Lookup logic with policy-driven promotions |
| 03 | [Cache Miss Flow](diagrams/03-cache-miss-flow.mmd) | Fanout admission with byte gates and oversize handling |
| 04 | [Sync vs Async Promotions](diagrams/04-sync-async-promotions.mmd) | Sequence diagram showing promotion strategies |
| 05 | [Policy Decision Tree](diagrams/05-policy-decision.mmd) | How byte gates and promotion policies control behavior |

### Data Flow Diagrams

| # | Diagram | Description |
|---|---------|-------------|
| 06 | [Cold Cache MISS](diagrams/06-cold-cache-miss.mmd) | Complete miss from all tiers to origin |
| 07 | [L3 HIT with Promotions](diagrams/07-l3-hit-promotions.mmd) | L3 hit warming upper tiers |
| 08 | [L0 HIT Hot Path](diagrams/08-l0-hit-hotpath.mmd) | Fastest path with no promotions |

### Trait Abstraction Diagrams

| # | Diagram | Description |
|---|---------|-------------|
| 09 | [Trait Abstraction Layer](diagrams/09-trait-abstraction.mmd) | BlobStore and IndexStore trait hierarchy |
| 10 | [L3 Internal Flow](diagrams/10-l3-internal-flow.mmd) | DurableTier lookup and miss operations |
| 11 | [L3 Sequence: Miss→Hit](diagrams/11-l3-sequence-miss-hit.mmd) | Full sequence of write then read |
| 12 | [Deployment Options](diagrams/12-deployment-options.mmd) | Real-world configurations (AWS, on-prem, hybrid) |
| 13 | [Wiring Example](diagrams/13-wiring-example.mmd) | Type-safe configuration in `main.rs` |
| 14 | [Complete System Flow](diagrams/14-complete-system-flow.mmd) | End-to-end request flow with all layers |
| 15 | [Manifest Data Model](diagrams/15-manifest-data-model.mmd) | Manifest structure and CRUD operations |

## 🎯 Key Concepts

### Four-Tier Hierarchy

- **L0**: In-process cache (2MB gate) - TinyLFU/LRU, fastest
- **L1**: Distributed cache (64MB gate) - Redis/Memcached, fast
- **L2**: Mid-latency cache (no gate) - Optional tier
- **L3**: Durable storage (no gate) - `DurableTier<BlobStore, IndexStore>`

### Promotion Strategies

- **L0 promotions**: Always synchronous (in-process, fast)
- **L1/L2 promotions**: Always asynchronous (lazy workers, bounded channels)
- **Policy-driven**: `Promotion::{Never, OnStreamCommitAtFinish}`

### Trait Abstraction Benefits

```rust
// Production: AWS native
DurableTier<S3Blob, DdbIndex>

// Global: Strong consistency
DurableTier<S3Blob, CrdbIndex>

// On-premise: Self-hosted
DurableTier<CephBlob, CrdbIndex>

// Development: Local
DurableTier<MinIOBlob, RedisIndex>
```

**Zero code changes** between deployments!

## 🚀 Quick Start

1. **Understand the system**
   - Read [Policy and Promotions](01-policy-and-promotions.md)
   - Review [Architecture Overview](diagrams/01-architecture-overview.mmd)

2. **Explore the implementation**
   - Study [Multi-Tier Implementation](02-multi-tier-implementation.md)
   - Trace flows in [Cache Hit](diagrams/02-cache-hit-flow.mmd) and [Cache Miss](diagrams/03-cache-miss-flow.mmd)

3. **Understand trait abstraction**
   - Read [Trait Abstraction Design](03-trait-abstraction-design.md)
   - Review [Trait Abstraction Layer](diagrams/09-trait-abstraction.mmd)
   - See deployment options in [Deployment Options](diagrams/12-deployment-options.mmd)

## 🔑 Design Principles

1. **Policy-Driven Behavior**: All promotion and admission logic controlled by `Policy` struct
2. **Zero-Cost Abstractions**: Fully typed, no dynamic dispatch overhead
3. **Graceful Degradation**: Best-effort promotions never fail client requests
4. **Oversize-Safe**: Byte gates + dynamic dropping prevent memory exhaustion
5. **Client-Optimized**: Async promotions never block TTFB
6. **Type-Safe Composition**: Cannot mix incompatible implementations

## 🛠️ Implementation Notes

### Two Hit Wrappers Cover All Cases

- `HitFromL1PromoteL0`: L1 → L0 promotion (sync only)
- `HitFromLower`: L2/L3 → L0/L1/L2 promotions (sync + async)

### One Miss Handler for All Tiers

- `FanoutMiss`: Multiplexes to L0/L1/L2/L3 based on byte gates
- Drops handlers mid-stream if size exceeds gate

### Lazy Async Workers

- Spawned on first chunk only
- Zero overhead for `Never` policy
- Bounded channels with drop-on-full backpressure

## 📈 Performance Characteristics

| Tier | Latency | Capacity | Use Case |
|------|---------|----------|----------|
| L0 | ~μs | Small (2MB) | Hot tiny objects |
| L1 | ~1-5ms | Medium (64MB) | Hot medium objects |
| L2 | ~10-50ms | Large | Optional mid-tier |
| L3 | ~50-500ms | Unlimited | All objects (durable) |

## 🔬 Testing & Development

- Use `MinIOBlob` + `RedisIndex` for local development
- Enable verbose metrics for promotion drop counters
- Test with `Content-Length` known and unknown scenarios
- Validate CAS behavior under concurrent writes

## 📖 Further Reading

- [Pingora Documentation](https://github.com/cloudflare/pingora)
- [Cache-Control Directives](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Cache-Control)
- [S3 Multipart Upload](https://docs.aws.amazon.com/AmazonS3/latest/userguide/mpuoverview.html)
- [DynamoDB Conditional Writes](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/WorkingWithItems.html#WorkingWithItems.ConditionalUpdate)
- [CockroachDB Transactions](https://www.cockroachlabs.com/docs/stable/transactions.html)

---

Generated: 2024
Project: Pingora Multi-Tier Cache
