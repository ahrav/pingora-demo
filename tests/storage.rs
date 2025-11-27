use pingora_demo::cache::{
    CacheError, CacheKey, CacheMeta, CacheResult, HitHandler, LookupResult, MissFinishType,
    MissHandler, PurgeType, Storage,
};
use pingora_demo::storage::durable::DurableTier;
use pingora_demo::storage::memory::{InMemoryBlobStore, InMemoryIndexStore, MemoryStore};
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "multi-tier")]
use pingora_demo::policy::{Policy, Promotion};
#[cfg(feature = "multi-tier")]
use pingora_demo::storage::multi_tier::MultiTier;

async fn read_all(mut hit: Box<dyn HitHandler>) -> CacheResult<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = hit.read_body().await? {
        out.extend_from_slice(&chunk);
    }
    hit.finish().await?;
    Ok(out)
}

#[tokio::test(flavor = "current_thread")]
async fn durable_round_trip() {
    let blob = Arc::new(InMemoryBlobStore::new());
    let index = Arc::new(InMemoryIndexStore::new());
    let durable = DurableTier::new(blob, index);

    let key = CacheKey::from("hello");
    let meta = CacheMeta::new(Some(5), Duration::from_secs(60));
    let mut miss = durable.get_miss_handler(&key, &meta).await.unwrap();
    miss.write_body(b"hello".to_vec(), true).await.unwrap();
    assert_eq!(miss.finish().await.unwrap(), MissFinishType::Success);

    match durable.lookup(&key).await.unwrap() {
        LookupResult::Fresh { meta, hit } => {
            assert_eq!(meta.content_length, Some(5));
            let body = read_all(hit).await.unwrap();
            assert_eq!(body, b"hello");
        }
        _ => panic!("expected fresh hit"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn durable_respects_ttl() {
    let blob = Arc::new(InMemoryBlobStore::new());
    let index = Arc::new(InMemoryIndexStore::new());
    let durable = DurableTier::new(blob, index);

    let key = CacheKey::from("short-lived");
    let meta = CacheMeta::new(Some(3), Duration::from_secs(0));
    let mut miss = durable.get_miss_handler(&key, &meta).await.unwrap();
    miss.write_body(b"hey".to_vec(), true).await.unwrap();
    let _ = miss.finish().await.unwrap();

    match durable.lookup(&key).await.unwrap() {
        LookupResult::Stale { .. } => {
            // Expected: expired entries should be stale
        }
        LookupResult::Miss => {
            // Also acceptable: expired entries might be considered miss
        }
        LookupResult::Fresh { .. } => {
            panic!("expired entries should not be fresh");
        }
    }
}

#[cfg(feature = "multi-tier")]
#[tokio::test(flavor = "current_thread")]
async fn fanout_drops_oversize_l0() {
    let l0 = Arc::new(MemoryStore::new());
    let blob = Arc::new(InMemoryBlobStore::new());
    let index = Arc::new(InMemoryIndexStore::new());
    let l3 = Arc::new(DurableTier::new(blob, index));

    let mut policy = Policy::default();
    policy.l0_max = 3;

    let tiers = MultiTier::new(Some(l0.clone()), None, None, l3.clone(), policy);
    let key = CacheKey::from("oversize");
    let meta = CacheMeta::new(None, Duration::from_secs(30));
    let mut miss = tiers.get_miss_handler(&key, &meta).await.unwrap();
    miss.write_body(vec![1, 2], false).await.unwrap();
    miss.write_body(vec![3, 4], true).await.unwrap();
    assert_eq!(miss.finish().await.unwrap(), MissFinishType::Success);

    assert!(
        matches!(l0.lookup(&key).await.unwrap(), LookupResult::Miss),
        "L0 should be dropped once gate is exceeded"
    );

    match tiers.lookup(&key).await.unwrap() {
        LookupResult::Fresh { hit, .. } | LookupResult::Stale { hit, .. } => {
            let body = read_all(hit).await.unwrap();
            assert_eq!(body, vec![1, 2, 3, 4]);
        }
        LookupResult::Miss => panic!("expected L3 hit"),
    }
}

#[cfg(feature = "multi-tier")]
#[derive(Clone, Default)]
struct FailingStore;

#[cfg(feature = "multi-tier")]
#[async_trait::async_trait]
impl Storage for FailingStore {
    async fn lookup(&self, _key: &CacheKey) -> CacheResult<LookupResult> {
        Ok(LookupResult::Miss)
    }

    async fn get_miss_handler(
        &self,
        _key: &CacheKey,
        _meta: &CacheMeta,
    ) -> CacheResult<Box<dyn MissHandler>> {
        Ok(Box::new(FailingMiss))
    }

    async fn purge(&self, _key: &CacheKey, _typ: PurgeType) -> CacheResult<bool> {
        Ok(false)
    }

    async fn update_meta(&self, _key: &CacheKey, _meta: &CacheMeta) -> CacheResult<bool> {
        Ok(false)
    }
}

#[cfg(feature = "multi-tier")]
struct FailingMiss;

#[cfg(feature = "multi-tier")]
#[async_trait::async_trait]
impl MissHandler for FailingMiss {
    async fn write_body(&mut self, _data: Vec<u8>, _eof: bool) -> CacheResult<()> {
        Ok(())
    }

    async fn finish(self: Box<Self>) -> CacheResult<MissFinishType> {
        Err(CacheError::Backend("promotion failed".into()))
    }
}

#[cfg(feature = "multi-tier")]
#[tokio::test(flavor = "current_thread")]
async fn promotion_is_best_effort() {
    let l0 = Arc::new(FailingStore::default());
    let blob = Arc::new(InMemoryBlobStore::new());
    let index = Arc::new(InMemoryIndexStore::new());
    let l3 = Arc::new(DurableTier::new(blob, index));

    // Seed L3
    let key = CacheKey::from("promote-me");
    let meta = CacheMeta::new(Some(4), Duration::from_secs(60));
    let mut miss = l3.get_miss_handler(&key, &meta).await.unwrap();
    miss.write_body(b"ping".to_vec(), true).await.unwrap();
    let _ = miss.finish().await.unwrap();

    let mut policy = Policy::default();
    policy.promote_from_l3_to_l0 = Promotion::OnStreamCommitAtFinish;
    policy.promote_from_l3_to_l1 = Promotion::Never;
    policy.promote_from_l3_to_l2 = Promotion::Never;

    let tiers = MultiTier::new(Some(l0), None, None, l3, policy);
    match tiers.lookup(&key).await.unwrap() {
        LookupResult::Fresh { hit, .. } | LookupResult::Stale { hit, .. } => {
            let body = read_all(hit).await.unwrap();
            assert_eq!(body, b"ping");
        }
        LookupResult::Miss => panic!("expected L3 hit"),
    }
}
