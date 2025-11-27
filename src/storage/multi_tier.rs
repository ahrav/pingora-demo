use crate::cache::{
    CacheKey, CacheMeta, CacheResult, HitHandler, MissFinishType, MissHandler, PurgeType, Storage,
};
use crate::policy::{Policy, Promotion, parse_content_length};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Multi-tier storage orchestrator with policy-driven promotions.
pub struct MultiTier {
    pub l0: Option<Arc<dyn Storage>>,
    pub l1: Option<Arc<dyn Storage>>,
    pub l2: Option<Arc<dyn Storage>>,
    pub l3: Arc<dyn Storage>,
    pub policy: Policy,
}

impl MultiTier {
    pub fn new(
        l0: Option<Arc<dyn Storage>>,
        l1: Option<Arc<dyn Storage>>,
        l2: Option<Arc<dyn Storage>>,
        l3: Arc<dyn Storage>,
        policy: Policy,
    ) -> Self {
        Self {
            l0,
            l1,
            l2,
            l3,
            policy,
        }
    }
}

struct HitFromL1PromoteL0 {
    inner: Box<dyn HitHandler>,
    l0_miss: Option<Box<dyn MissHandler>>,
    mode: Promotion,
}

#[async_trait]
impl HitHandler for HitFromL1PromoteL0 {
    async fn read_body(&mut self) -> CacheResult<Option<Vec<u8>>> {
        if let Some(chunk) = self.inner.read_body().await? {
            if self.mode == Promotion::OnStreamCommitAtFinish {
                if let Some(ref mut miss) = self.l0_miss {
                    let _ = miss.write_body(chunk.clone(), false).await;
                }
            }
            return Ok(Some(chunk));
        }
        Ok(None)
    }

    async fn finish(self: Box<Self>) -> CacheResult<()> {
        let mut this = self;
        this.inner.finish().await?;

        if this.mode == Promotion::OnStreamCommitAtFinish {
            if let Some(miss) = this.l0_miss.take() {
                // Promotion is best-effort; ignore failures so the client path stays clean.
                let _ = miss.finish().await;
            }
        }
        Ok(())
    }
}

struct AsyncPromo {
    tx: mpsc::Sender<Vec<u8>>,
    join: tokio::task::JoinHandle<CacheResult<MissFinishType>>,
}

struct HitFromLower {
    inner: Box<dyn HitHandler>,

    l0_mode: Promotion,
    l0_miss: Option<Box<dyn MissHandler>>,

    l1_mode: Promotion,
    l1_async: Option<AsyncPromo>,
    l1_lazy_target: Option<(Arc<dyn Storage>, CacheKey, CacheMeta)>,

    l2_mode: Promotion,
    l2_async: Option<AsyncPromo>,
    l2_lazy_target: Option<(Arc<dyn Storage>, CacheKey, CacheMeta)>,
}

impl HitFromLower {
    async fn ensure_async_worker(
        lazy: &mut Option<(Arc<dyn Storage>, CacheKey, CacheMeta)>,
        slot: &mut Option<AsyncPromo>,
    ) -> CacheResult<()> {
        if slot.is_some() {
            return Ok(());
        }
        if let Some((storage, key, meta)) = lazy.take() {
            let miss = storage.get_miss_handler(&key, &meta).await?;
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
            let join = tokio::spawn(async move {
                let mut handler = miss;
                while let Some(chunk) = rx.recv().await {
                    handler.write_body(chunk, false).await?;
                }
                handler.finish().await
            });
            *slot = Some(AsyncPromo { tx, join });
        }
        Ok(())
    }
}

#[async_trait]
impl HitHandler for HitFromLower {
    async fn read_body(&mut self) -> CacheResult<Option<Vec<u8>>> {
        if let Some(chunk) = self.inner.read_body().await? {
            if self.l0_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(ref mut miss) = self.l0_miss {
                    let _ = miss.write_body(chunk.clone(), false).await;
                }
            }

            if self.l1_mode == Promotion::OnStreamCommitAtFinish {
                if self.l1_async.is_none() {
                    Self::ensure_async_worker(&mut self.l1_lazy_target, &mut self.l1_async).await?;
                }
                if let Some(ap) = self.l1_async.as_ref() {
                    if ap.tx.try_send(chunk.clone()).is_err() {
                        // drop on full
                    }
                }
            }

            if self.l2_mode == Promotion::OnStreamCommitAtFinish {
                if self.l2_async.is_none() {
                    Self::ensure_async_worker(&mut self.l2_lazy_target, &mut self.l2_async).await?;
                }
                if let Some(ap) = self.l2_async.as_ref() {
                    if ap.tx.try_send(chunk.clone()).is_err() {
                        // drop on full
                    }
                }
            }

            return Ok(Some(chunk));
        }
        Ok(None)
    }

    async fn finish(self: Box<Self>) -> CacheResult<()> {
        let mut this = self;
        this.inner.finish().await?;

        if let Some(ap) = this.l1_async.take() {
            drop(ap.tx);
            let _ = ap.join.await;
        }
        if let Some(ap) = this.l2_async.take() {
            drop(ap.tx);
            let _ = ap.join.await;
        }

        if this.l0_mode == Promotion::OnStreamCommitAtFinish {
            if let Some(miss) = this.l0_miss.take() {
                let _ = miss.finish().await;
            }
        }
        Ok(())
    }
}

struct FanoutMiss {
    l0_max: usize,
    l1_max: usize,
    l2_max: usize,
    total: usize,
    l0: Option<Box<dyn MissHandler>>,
    l1: Option<Box<dyn MissHandler>>,
    l2: Option<Box<dyn MissHandler>>,
    l3: Option<Box<dyn MissHandler>>,
}

#[async_trait]
impl MissHandler for FanoutMiss {
    async fn write_body(&mut self, data: Vec<u8>, eof: bool) -> CacheResult<()> {
        self.total = self.total.saturating_add(data.len());

        if let Some(handler) = self.l0.as_mut() {
            if self.total <= self.l0_max {
                handler.write_body(data.clone(), eof).await?;
            } else {
                self.l0.take();
            }
        }
        if let Some(handler) = self.l1.as_mut() {
            if self.total <= self.l1_max {
                handler.write_body(data.clone(), eof).await?;
            } else {
                self.l1.take();
            }
        }
        if let Some(handler) = self.l2.as_mut() {
            if self.total <= self.l2_max {
                handler.write_body(data.clone(), eof).await?;
            } else {
                self.l2.take();
            }
        }
        if let Some(handler) = self.l3.as_mut() {
            handler.write_body(data, eof).await?;
        }

        Ok(())
    }

    async fn finish(self: Box<Self>) -> CacheResult<MissFinishType> {
        let mut ok = true;

        if let Some(handler) = self.l0 {
            ok &= matches!(handler.finish().await, Ok(MissFinishType::Success));
        }
        if let Some(handler) = self.l1 {
            ok &= matches!(handler.finish().await, Ok(MissFinishType::Success));
        }
        if let Some(handler) = self.l2 {
            ok &= matches!(handler.finish().await, Ok(MissFinishType::Success));
        }
        if let Some(handler) = self.l3 {
            ok &= matches!(handler.finish().await, Ok(MissFinishType::Success));
        }

        Ok(if ok {
            MissFinishType::Success
        } else {
            MissFinishType::Failed
        })
    }
}

#[async_trait]
impl Storage for MultiTier {
    async fn lookup(
        &self,
        key: &CacheKey,
    ) -> CacheResult<Option<(CacheMeta, Box<dyn HitHandler>)>> {
        if let Some(l0) = &self.l0 {
            if let Some(hit) = l0.lookup(key).await? {
                return Ok(Some(hit));
            }
        }

        if let Some(l1) = &self.l1 {
            if let Some((meta, inner)) = l1.lookup(key).await? {
                let len = parse_content_length(&meta);
                let mut l0_miss = None;
                if self.policy.promote_from_l1_to_l0 == Promotion::OnStreamCommitAtFinish {
                    if let Some(l0) = &self.l0 {
                        let allow = len.map(|n| n <= self.policy.l0_max).unwrap_or(true);
                        if allow {
                            l0_miss = Some(l0.get_miss_handler(key, &meta).await?);
                        }
                    }
                }
                let wrapper = HitFromL1PromoteL0 {
                    inner,
                    l0_miss,
                    mode: self.policy.promote_from_l1_to_l0,
                };
                return Ok(Some((meta, Box::new(wrapper))));
            }
        }

        if let Some(l2) = &self.l2 {
            if let Some((meta, inner)) = l2.lookup(key).await? {
                let len = parse_content_length(&meta);
                let mut wrapper = HitFromLower {
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

                if wrapper.l0_mode == Promotion::OnStreamCommitAtFinish {
                    if let Some(l0) = &self.l0 {
                        let allow = len.map(|n| n <= self.policy.l0_max).unwrap_or(true);
                        if allow {
                            wrapper.l0_miss = Some(l0.get_miss_handler(key, &meta).await?);
                        }
                    }
                }

                if wrapper.l1_mode == Promotion::OnStreamCommitAtFinish {
                    if let Some(l1) = &self.l1 {
                        let allow = len.map(|n| n <= self.policy.l1_max).unwrap_or(true);
                        if allow {
                            wrapper.l1_lazy_target = Some((l1.clone(), key.clone(), meta.clone()));
                        }
                    }
                }

                return Ok(Some((meta, Box::new(wrapper))));
            }
        }

        if let Some((meta, inner)) = self.l3.lookup(key).await? {
            let len = parse_content_length(&meta);
            let mut wrapper = HitFromLower {
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

            if wrapper.l0_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(l0) = &self.l0 {
                    let allow = len.map(|n| n <= self.policy.l0_max).unwrap_or(true);
                    if allow {
                        wrapper.l0_miss = Some(l0.get_miss_handler(key, &meta).await?);
                    }
                }
            }

            if wrapper.l1_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(l1) = &self.l1 {
                    let allow = len.map(|n| n <= self.policy.l1_max).unwrap_or(true);
                    if allow {
                        wrapper.l1_lazy_target = Some((l1.clone(), key.clone(), meta.clone()));
                    }
                }
            }

            if wrapper.l2_mode == Promotion::OnStreamCommitAtFinish {
                if let Some(l2) = &self.l2 {
                    let allow = len.map(|n| n <= self.policy.l2_max).unwrap_or(true);
                    if allow {
                        wrapper.l2_lazy_target = Some((l2.clone(), key.clone(), meta.clone()));
                    }
                }
            }

            return Ok(Some((meta, Box::new(wrapper))));
        }

        Ok(None)
    }

    async fn get_miss_handler(
        &self,
        key: &CacheKey,
        meta: &CacheMeta,
    ) -> CacheResult<Box<dyn MissHandler>> {
        let len = parse_content_length(meta);

        let l0 = if let Some(l0) = &self.l0 {
            if len.map(|n| n <= self.policy.l0_max).unwrap_or(true) {
                Some(l0.get_miss_handler(key, meta).await?)
            } else {
                None
            }
        } else {
            None
        };

        let l1 = if let Some(l1) = &self.l1 {
            if len.map(|n| n <= self.policy.l1_max).unwrap_or(true) {
                Some(l1.get_miss_handler(key, meta).await?)
            } else {
                None
            }
        } else {
            None
        };

        let l2 = if let Some(l2) = &self.l2 {
            if len.map(|n| n <= self.policy.l2_max).unwrap_or(true) {
                Some(l2.get_miss_handler(key, meta).await?)
            } else {
                None
            }
        } else {
            None
        };

        let l3 = Some(self.l3.get_miss_handler(key, meta).await?);

        Ok(Box::new(FanoutMiss {
            l0_max: self.policy.l0_max,
            l1_max: self.policy.l1_max,
            l2_max: self.policy.l2_max,
            total: 0,
            l0,
            l1,
            l2,
            l3,
        }))
    }

    async fn purge(&self, key: &CacheKey, typ: PurgeType) -> CacheResult<bool> {
        let mut any = false;
        if let Some(l0) = &self.l0 {
            any |= l0.purge(key, typ).await.unwrap_or(false);
        }
        if let Some(l1) = &self.l1 {
            any |= l1.purge(key, typ).await.unwrap_or(false);
        }
        if let Some(l2) = &self.l2 {
            any |= l2.purge(key, typ).await.unwrap_or(false);
        }
        any |= self.l3.purge(key, typ).await.unwrap_or(false);
        Ok(any)
    }

    async fn update_meta(&self, key: &CacheKey, meta: &CacheMeta) -> CacheResult<bool> {
        let mut any = false;
        if let Some(l0) = &self.l0 {
            any |= l0.update_meta(key, meta).await.unwrap_or(false);
        }
        if let Some(l1) = &self.l1 {
            any |= l1.update_meta(key, meta).await.unwrap_or(false);
        }
        if let Some(l2) = &self.l2 {
            any |= l2.update_meta(key, meta).await.unwrap_or(false);
        }
        any |= self.l3.update_meta(key, meta).await.unwrap_or(false);
        Ok(any)
    }
}
