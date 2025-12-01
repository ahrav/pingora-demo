#![allow(clippy::type_complexity)]

use crate::cache::{
    CacheError, CacheKey, CacheMeta, CacheResult, HitHandler, LookupResult, MissFinishType,
    MissHandler, PurgeType, Storage,
};
use crate::storage::traits::{
    BlobRead, BlobStore, BlobWrite, DurableError, DurableResult, IndexStore, Manifest, ObjectId,
    ObjectMeta,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const DEFAULT_CHUNK: usize = 1024;

/* ---------- Generic in-memory cache tier for L0/L1/L2 ---------- */

#[derive(Clone, Default)]
pub struct MemoryStore {
    pub(crate) inner: Arc<Mutex<HashMap<CacheKey, (CacheMeta, Vec<u8>)>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

struct MemoryHit {
    data: Arc<Vec<u8>>,
    cursor: usize,
}

#[async_trait]
impl HitHandler for MemoryHit {
    async fn read_body(&mut self) -> CacheResult<Option<Vec<u8>>> {
        if self.cursor >= self.data.len() {
            return Ok(None);
        }
        let end = (self.cursor + DEFAULT_CHUNK).min(self.data.len());
        let chunk = self.data[self.cursor..end].to_vec();
        self.cursor = end;
        Ok(Some(chunk))
    }

    async fn finish(self: Box<Self>) -> CacheResult<()> {
        Ok(())
    }
}

struct MemoryMiss {
    key: CacheKey,
    meta: CacheMeta,
    buf: Vec<u8>,
    inner: Arc<Mutex<HashMap<CacheKey, (CacheMeta, Vec<u8>)>>>,
}

#[async_trait]
impl MissHandler for MemoryMiss {
    async fn write_body(&mut self, data: Vec<u8>, _eof: bool) -> CacheResult<()> {
        self.buf.extend_from_slice(&data);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> CacheResult<MissFinishType> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CacheError::Backend("mutex poisoned".into()))?;
        guard.insert(self.key, (self.meta, self.buf));
        Ok(MissFinishType::Success)
    }
}

#[async_trait]
impl Storage for MemoryStore {
    async fn lookup(&self, key: &CacheKey) -> CacheResult<LookupResult> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| CacheError::Backend("mutex poisoned".into()))?;
        if let Some((meta, data)) = guard.get(key) {
            let hit = MemoryHit {
                data: Arc::new(data.clone()),
                cursor: 0,
            };
            let hit_boxed: Box<dyn HitHandler> = Box::new(hit);

            if meta.is_fresh() {
                return Ok(LookupResult::Fresh {
                    meta: meta.clone(),
                    hit: hit_boxed,
                });
            } else {
                return Ok(LookupResult::Stale {
                    meta: meta.clone(),
                    hit: hit_boxed,
                });
            }
        }
        Ok(LookupResult::Miss)
    }

    async fn get_miss_handler(
        &self,
        key: &CacheKey,
        meta: &CacheMeta,
    ) -> CacheResult<Box<dyn MissHandler>> {
        Ok(Box::new(MemoryMiss {
            key: key.clone(),
            meta: meta.clone(),
            buf: Vec::new(),
            inner: self.inner.clone(),
        }))
    }

    async fn purge(&self, key: &CacheKey, _typ: PurgeType) -> CacheResult<bool> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CacheError::Backend("mutex poisoned".into()))?;
        Ok(guard.remove(key).is_some())
    }

    async fn update_meta(&self, key: &CacheKey, meta: &CacheMeta) -> CacheResult<bool> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CacheError::Backend("mutex poisoned".into()))?;
        if let Some(entry) = guard.get_mut(key) {
            entry.0 = meta.clone();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn touch(&self, key: &CacheKey, new_expiry: u64) -> CacheResult<bool> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CacheError::Backend("mutex poisoned".into()))?;
        if let Some(entry) = guard.get_mut(key) {
            entry.0.expires_at = Some(new_expiry);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/* ---------- In-memory blob and index stores for DurableTier ---------- */

#[derive(Clone, Default)]
pub struct InMemoryBlobStore {
    inner: Arc<Mutex<HashMap<String, (ObjectMeta, Vec<u8>)>>>,
}

impl InMemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(&self, id: &ObjectId) -> String {
        format!("{}:{}", id.bucket.clone().unwrap_or_default(), id.key)
    }
}

struct InMemoryBlobReader {
    data: Arc<Vec<u8>>,
    cursor: usize,
}

#[async_trait]
impl BlobRead for InMemoryBlobReader {
    async fn next_chunk(&mut self) -> DurableResult<Option<Vec<u8>>> {
        if self.cursor >= self.data.len() {
            return Ok(None);
        }
        let end = (self.cursor + DEFAULT_CHUNK).min(self.data.len());
        let chunk = self.data[self.cursor..end].to_vec();
        self.cursor = end;
        Ok(Some(chunk))
    }
}

struct InMemoryBlobWriter {
    store_key: String,
    inner: Arc<Mutex<HashMap<String, (ObjectMeta, Vec<u8>)>>>,
    buf: Vec<u8>,
}

#[async_trait]
impl BlobWrite for InMemoryBlobWriter {
    async fn write(&mut self, chunk: Vec<u8>) -> DurableResult<()> {
        self.buf.extend_from_slice(&chunk);
        Ok(())
    }

    async fn complete(self: Box<Self>) -> DurableResult<ObjectMeta> {
        let meta = ObjectMeta {
            len: self.buf.len() as u64,
            etag: None,
        };
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        guard.insert(self.store_key.clone(), (meta.clone(), self.buf));
        Ok(meta)
    }

    async fn abort(self: Box<Self>) -> DurableResult<()> {
        // No-op for in-memory writer
        Ok(())
    }
}

#[async_trait]
impl BlobStore for InMemoryBlobStore {
    async fn get(&self, id: &ObjectId) -> DurableResult<(ObjectMeta, Box<dyn BlobRead>)> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        let key = self.key(id);
        let Some((meta, data)) = guard.get(&key) else {
            return Err(DurableError::NotFound);
        };
        let reader = InMemoryBlobReader {
            data: Arc::new(data.clone()),
            cursor: 0,
        };
        Ok((meta.clone(), Box::new(reader)))
    }

    async fn head(&self, id: &ObjectId) -> DurableResult<ObjectMeta> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        let key = self.key(id);
        guard
            .get(&key)
            .map(|(m, _)| m.clone())
            .ok_or(DurableError::NotFound)
    }

    async fn put(&self, id: &ObjectId) -> DurableResult<Box<dyn BlobWrite>> {
        let store_key = self.key(id);
        Ok(Box::new(InMemoryBlobWriter {
            store_key,
            inner: self.inner.clone(),
            buf: Vec::new(),
        }))
    }

    async fn delete(&self, id: &ObjectId) -> DurableResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        let key = self.key(id);
        guard.remove(&key);
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct InMemoryIndexStore {
    inner: Arc<Mutex<HashMap<Vec<u8>, Manifest>>>,
}

impl InMemoryIndexStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl IndexStore for InMemoryIndexStore {
    async fn get(&self, key: &[u8]) -> DurableResult<Option<Manifest>> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        Ok(guard.get(key).cloned())
    }

    async fn put_new(&self, key: &[u8], manifest: &Manifest) -> DurableResult<u64> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        if guard.contains_key(key) {
            return Err(DurableError::PreconditionFailed);
        }
        guard.insert(key.to_vec(), manifest.clone());
        Ok(1)
    }

    async fn cas_update(
        &self,
        key: &[u8],
        expect_ver: u64,
        manifest: &Manifest,
    ) -> DurableResult<u64> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        let Some(current) = guard.get_mut(key) else {
            return Err(DurableError::NotFound);
        };
        if current.version != expect_ver {
            return Err(DurableError::PreconditionFailed);
        }
        current.version = expect_ver + 1;
        current.meta_blob = manifest.meta_blob.clone();
        current.expires_at = manifest.expires_at;
        Ok(current.version)
    }

    async fn delete(&self, key: &[u8]) -> DurableResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        guard.remove(key);
        Ok(())
    }

    async fn touch_ttl(&self, key: &[u8], new_expiry: SystemTime) -> DurableResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| DurableError::Backend("mutex poisoned".into()))?;
        if let Some(m) = guard.get_mut(key) {
            m.expires_at = new_expiry;
        }
        Ok(())
    }
}
