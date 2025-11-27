# Multi-Tier Storage Implementation

Complete implementation of the multi-tier cache storage system with promotion support.

```rust
// src/storage/multi_tier.rs
use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use pingora_cache::{
    CacheKey, CacheMeta, Storage, HitHandler, MissHandler,
    storage::{HandleHit, HandleMiss, MissFinishType, PurgeType},
    trace::SpanHandle,
};

use crate::policy::{Policy, Promotion, parse_content_length};

// Simple metric stub
fn incr_counter(_name: &str) {
    // TODO: hook to your metrics
}

/// Fully-typed tier stack. No trait-object tiers.
pub struct MultiTier<L0, L1, L2, L3> {
    pub l0: Option<&'static L0>, // in-proc
    pub l1: Option<&'static L1>, // distributed
    pub l2: Option<&'static L2>, // mid-latency
    pub l3: &'static L3,         // durable (DynamoDB+S3)
    pub policy: Policy,
}

impl<L0, L1, L2, L3> MultiTier<L0, L1, L2, L3> {
    pub fn new(
        l0: Option<&'static L0>,
        l1: Option<&'static L1>,
        l2: Option<&'static L2>,
        l3: &'static L3,
        policy: Policy,
    ) -> Self {
        Self { l0, l1, l2, l3, policy }
    }
}

/* ---------- Hit wrappers ---------- */

struct HitFromL1PromoteL0 {
    inner: HitHandler,
    l0_miss: Option<MissHandler>,
    mode: Promotion, // Never | OnStreamCommitAtFinish
}

#[async_trait]
impl HandleHit for HitFromL1PromoteL0 {
    async fn read_body(&mut self) -> pingora_cache::Result<Option<Bytes>> {
        if let Some(chunk) = self.inner.read_body().await? {
            if self.mode == Promotion::OnStreamCommitAtFinish {
                if let Some(ref mut h) = self.l0_miss {
                    let _ = h.write_body(chunk.clone(), false).await;
                }
            }
            return Ok(Some(chunk));
        }
        Ok(None)
    }

    async fn finish(
        self: Box<Self>,
        storage: &'static (dyn Storage + Sync),
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<()> {
        let mut this = self;

        // Finish the source hit first (Pingora contract)
        this.inner.finish(storage, key, trace).await?;

        // Then commit the promotion
        if self.mode == Promotion::OnStreamCommitAtFinish {
            if let Some(h) = this.l0_miss.take() {
                let _ = h.finish().await?;
            }
        }
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn as_any_mut(&mut self) -> &mut (dyn std::any::Any + Send + Sync) { self }
    fn can_seek(&self) -> bool { self.inner.can_seek() }
    fn seek(&mut self, s: usize, e: Option<usize>) -> pingora_cache::Result<()> { self.inner.seek(s, e) }
    fn should_count_access(&self) -> bool { self.inner.should_count_access() }
    fn get_eviction_weight(&self) -> usize { self.inner.get_eviction_weight() }
}

struct AsyncPromo {
    tx: mpsc::Sender<Bytes>,
    join: tokio::task::JoinHandle<pingora_cache::Result<MissFinishType>>,
}

/// One wrapper for "lower" tiers (L2/L3) promoting up to L1/L0 (and L3→L2).
struct HitFromLower {
    inner: HitHandler,

    // L0 sync
    l0_mode: Promotion,
    l0_miss: Option<MissHandler>,

    // L1 async (lazy)
    l1_mode: Promotion,
    l1_async: Option<AsyncPromo>,
    l1_lazy_target: Option<(&'static (dyn Storage + Sync), CacheKey, CacheMeta, SpanHandle)>,

    // L2 async (lazy) when source is L3
    l2_mode: Promotion,
    l2_async: Option<AsyncPromo>,
    l2_lazy_target: Option<(&'static (dyn Storage + Sync), CacheKey, CacheMeta, SpanHandle)>,
}

impl HitFromLower {
    async fn ensure_async_worker(
        lazy: &mut Option<(&'static (dyn Storage + Sync), CacheKey, CacheMeta, SpanHandle)>,
        slot: &mut Option<AsyncPromo>,
    ) -> pingora_cache::Result<()> {
        if slot.is_some() { return Ok(()); }
        if let Some((storage, key, meta, trace)) = lazy.take() {
            let miss = storage.get_miss_handler(&key, &meta, &trace).await?;
            let (tx, mut rx) = mpsc::channel::<Bytes>(64);
            let join = tokio::spawn(async move {
                let mut m = miss;
                while let Some(chunk) = rx.recv().await {
                    m.write_body(chunk, false).await?;
                }
                m.finish().await
            });
            *slot = Some(AsyncPromo { tx, join });
        }
        Ok(())
    }
}

#[async_trait]
impl HandleHit for HitFromLower {
    async fn read_body(&mut self) -> pingora_cache::Result<Option<Bytes>> {
        if let Some(chunk) = self.inner.read_body().await? {
            // L0 sync (best-effort)
            if self.l0_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(ref mut h) = self.l0_miss {
                    let _ = h.write_body(chunk.clone(), false).await;
                }
            }

            // L1 async (lazy init)
            if self.l1_mode == Promotion::OnStreamCommitAtFinish {
                if self.l1_async.is_none() {
                    Self::ensure_async_worker(&mut self.l1_lazy_target, &mut self.l1_async).await?;
                }
                if let Some(ap) = self.l1_async.as_ref() {
                    if ap.tx.try_send(chunk.clone()).is_err() {
                        incr_counter("promo.l1.drop_full");
                    }
                }
            }

            // L2 async (lazy init)
            if self.l2_mode == Promotion::OnStreamCommitAtFinish {
                if self.l2_async.is_none() {
                    Self::ensure_async_worker(&mut self.l2_lazy_target, &mut self.l2_async).await?;
                }
                if let Some(ap) = self.l2_async.as_ref() {
                    if ap.tx.try_send(chunk.clone()).is_err() {
                        incr_counter("promo.l2.drop_full");
                    }
                }
            }

            return Ok(Some(chunk));
        }
        Ok(None)
    }

    async fn finish(
        self: Box<Self>,
        storage: &'static (dyn Storage + Sync),
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<()> {
        let mut this = self;

        // Finish the source hit first
        this.inner.finish(storage, key, trace).await?;

        // Close async lanes by dropping senders, then await joins
        if let Some(ap) = this.l1_async.take() {
            drop(ap.tx);
            let _ = ap.join.await;
        }
        if let Some(ap) = this.l2_async.take() {
            drop(ap.tx);
            let _ = ap.join.await;
        }

        // Commit L0 sync promotion
        if self.l0_mode == Promotion::OnStreamCommitAtFinish {
            if let Some(h) = this.l0_miss.take() {
                let _ = h.finish().await?;
            }
        }
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn as_any_mut(&mut self) -> &mut (dyn std::any::Any + Send + Sync) { self }
    fn can_seek(&self) -> bool { self.inner.can_seek() }
    fn seek(&mut self, s: usize, e: Option<usize>) -> pingora_cache::Result<()> { self.inner.seek(s, e) }
    fn should_count_access(&self) -> bool { self.inner.should_count_access() }
    fn get_eviction_weight(&self) -> usize { self.inner.get_eviction_weight() }
}

/* ---------- Miss fanout (oversize-safe) ---------- */

struct FanoutMiss {
    l0_max: usize,
    l1_max: usize,
    l2_max: usize,
    total: usize,

    l0: Option<MissHandler>,
    l1: Option<MissHandler>,
    l2: Option<MissHandler>,
    l3: Option<MissHandler>, // durable; usually present
}

#[async_trait]
impl HandleMiss for FanoutMiss {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> pingora_cache::Result<()> {
        self.total = self.total.saturating_add(data.len());

        if let Some(h) = self.l0.as_mut() {
            if self.total <= self.l0_max {
                let _ = h.write_body(data.clone(), eof).await?;
            } else {
                drop(self.l0.take()); // oversize => fail L0 admission
            }
        }
        if let Some(h) = self.l1.as_mut() {
            if self.total <= self.l1_max {
                let _ = h.write_body(data.clone(), eof).await?;
            } else {
                drop(self.l1.take());
            }
        }
        if let Some(h) = self.l2.as_mut() {
            if self.total <= self.l2_max {
                let _ = h.write_body(data.clone(), eof).await?;
            } else {
                drop(self.l2.take());
            }
        }
        if let Some(h) = self.l3.as_mut() {
            let _ = h.write_body(data, eof).await?;
        }
        Ok(())
    }

    async fn finish(self: Box<Self>) -> pingora_cache::Result<MissFinishType> {
        let mut ok = true;
        if let Some(h) = self.l0 { ok &= matches!(h.finish().await, Ok(MissFinishType::Success)); }
        if let Some(h) = self.l1 { ok &= matches!(h.finish().await, Ok(MissFinishType::Success)); }
        if let Some(h) = self.l2 { ok &= matches!(h.finish().await, Ok(MissFinishType::Success)); }
        if let Some(h) = self.l3 { ok &= matches!(h.finish().await, Ok(MissFinishType::Success)); }
        Ok(if ok { MissFinishType::Success } else { MissFinishType::Failed })
    }
}

/* ---------- Storage impl (typed tiers; OR semantics) ---------- */

#[async_trait]
impl<L0, L1, L2, L3> Storage for MultiTier<L0, L1, L2, L3>
where
    L0: Storage + Sync + 'static,
    L1: Storage + Sync + 'static,
    L2: Storage + Sync + 'static,
    L3: Storage + Sync + 'static,
{
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<Option<(CacheMeta, HitHandler)>> {
        // L0
        if let Some(l0) = self.l0 {
            if let Some(pair) = l0.lookup(key, trace).await? {
                return Ok(Some(pair));
            }
        }

        // L1
        if let Some(l1) = self.l1 {
            if let Some((meta, inner)) = l1.lookup(key, trace).await? {
                let len = parse_content_length(&meta);
                let mut l0_miss = None;

                // L1 -> L0 promotion (sync)
                let l0_mode = self.policy.promote_from_l1_to_l0;
                if l0_mode == Promotion::OnStreamCommitAtFinish {
                    if let Some(l0) = self.l0 {
                        let allow = len.map(|n| n <= self.policy.l0_max).unwrap_or(true);
                        if allow { l0_miss = Some(l0.get_miss_handler(key, &meta, trace).await?); }
                    }
                }

                let w = HitFromL1PromoteL0 { inner, l0_miss, mode: l0_mode };
                return Ok(Some((meta, Box::new(w))));
            }
        }

        // L2
        if let Some(l2) = self.l2 {
            if let Some((meta, inner)) = l2.lookup(key, trace).await? {
                let len = parse_content_length(&meta);

                // Prepare wrapper
                let mut w = HitFromLower {
                    inner,
                    l0_mode: self.policy.promote_from_l2_to_l0,
                    l0_miss: None,
                    l1_mode: self.policy.promote_from_l2_to_l1,
                    l1_async: None,
                    l1_lazy_target: None,
                    l2_mode: Promotion::Never,
                    l2_async: None,
                    l2_lazy_target: None,
                };

                // L2 -> L0 (sync)
                if w.l0_mode == Promotion::OnStreamCommitAtFinish {
                    if let Some(l0) = self.l0 {
                        let allow = len.map(|n| n <= self.policy.l0_max).unwrap_or(true);
                        if allow { w.l0_miss = Some(l0.get_miss_handler(key, &meta, trace).await?); }
                    }
                }

                // L2 -> L1 (async, lazy)
                if w.l1_mode == Promotion::OnStreamCommitAtFinish {
                    if let Some(l1) = self.l1 {
                        let allow = len.map(|n| n <= self.policy.l1_max).unwrap_or(true);
                        if allow {
                            w.l1_lazy_target = Some((l1 as &'static (dyn Storage + Sync), key.clone(), meta.clone(), trace.clone()));
                        }
                    }
                }

                return Ok(Some((meta, Box::new(w))));
            }
        }

        // L3
        if let Some((meta, inner)) = self.l3.lookup(key, trace).await? {
            let len = parse_content_length(&meta);

            let mut w = HitFromLower {
                inner,
                l0_mode: self.policy.promote_from_l3_to_l0,
                l0_miss: None,
                l1_mode: self.policy.promote_from_l3_to_l1,
                l1_async: None,
                l1_lazy_target: None,
                l2_mode: self.policy.promote_from_l3_to_l2,
                l2_async: None,
                l2_lazy_target: None,
            };

            // L3 -> L0 (sync)
            if w.l0_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(l0) = self.l0 {
                    let allow = len.map(|n| n <= self.policy.l0_max).unwrap_or(true);
                    if allow { w.l0_miss = Some(l0.get_miss_handler(key, &meta, trace).await?); }
                }
            }

            // L3 -> L1 (async, lazy)
            if w.l1_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(l1) = self.l1 {
                    let allow = len.map(|n| n <= self.policy.l1_max).unwrap_or(true);
                    if allow {
                        w.l1_lazy_target = Some((l1 as &'static (dyn Storage + Sync), key.clone(), meta.clone(), trace.clone()));
                    }
                }
            }

            // L3 -> L2 (async, lazy)
            if w.l2_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(l2) = self.l2 {
                    let allow = len.map(|n| n <= self.policy.l2_max).unwrap_or(true);
                    if allow {
                        w.l2_lazy_target = Some((l2 as &'static (dyn Storage + Sync), key.clone(), meta.clone(), trace.clone()));
                    }
                }
            }

            return Ok(Some((meta, Box::new(w))));
        }

        Ok(None)
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<MissHandler> {
        let len = parse_content_length(meta);

        let l0 = if let Some(l0) = self.l0 {
            if len.map(|n| n <= self.policy.l0_max).unwrap_or(true) {
                Some(l0.get_miss_handler(key, meta, trace).await?)
            } else { None }
        } else { None };

        let l1 = if let Some(l1) = self.l1 {
            if len.map(|n| n <= self.policy.l1_max).unwrap_or(true) {
                Some(l1.get_miss_handler(key, meta, trace).await?)
            } else { None }
        } else { None };

        let l2 = if let Some(l2) = self.l2 {
            if len.map(|n| n <= self.policy.l2_max).unwrap_or(true) {
                Some(l2.get_miss_handler(key, meta, trace).await?)
            } else { None }
        } else { None };

        let l3 = Some(self.l3.get_miss_handler(key, meta, trace).await?);

        Ok(Box::new(FanoutMiss {
            l0_max: self.policy.l0_max,
            l1_max: self.policy.l1_max,
            l2_max: self.policy.l2_max,
            total: 0,
            l0, l1, l2, l3,
        }))
    }

    async fn purge(
        &'static self,
        ckey: &pingora_cache::key::CompactCacheKey,
        typ: PurgeType,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<bool> {
        let mut any = false;
        if let Some(l0) = self.l0 { any |= l0.purge(ckey, typ, trace).await.unwrap_or(false); }
        if let Some(l1) = self.l1 { any |= l1.purge(ckey, typ, trace).await.unwrap_or(false); }
        if let Some(l2) = self.l2 { any |= l2.purge(ckey, typ, trace).await.unwrap_or(false); }
        any |= self.l3.purge(ckey, typ, trace).await.unwrap_or(false);
        Ok(any)
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<bool> {
        let mut any = false;
        if let Some(l0) = self.l0 { any |= l0.update_meta(key, meta, trace).await.unwrap_or(false); }
        if let Some(l1) = self.l1 { any |= l1.update_meta(key, meta, trace).await.unwrap_or(false); }
        if let Some(l2) = self.l2 { any |= l2.update_meta(key, meta, trace).await.unwrap_or(false); }
        any |= self.l3.update_meta(key, meta, trace).await.unwrap_or(false);
        Ok(any)
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        tag: Option<&[u8]>,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<Option<(CacheMeta, HitHandler)>> {
        let _ = tag;
        self.lookup(key, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        // Keep false for now; set to true only when you add real support end-to-end.
        false
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) { self }
}
```
