use crate::cache::{
    CacheError, CacheKey, CacheMeta, CacheResult, HitHandler, LookupResult, MissFinishType,
    MissHandler, PurgeType, Storage,
};
use crate::storage::traits::{
    BlobRead, BlobStore, BlobWrite, DurableError, IndexStore, Manifest, ObjectId,
};
use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn map_durable_err(err: DurableError) -> CacheError {
    match err {
        DurableError::PreconditionFailed => CacheError::Conflict,
        DurableError::NotFound => CacheError::NotFound,
        DurableError::Backend(msg) => CacheError::Backend(msg),
    }
}

// TODO: Why not provide a const for the capacity?
// Is there a further optimization of using a buffer pool?
fn serialize_meta(meta: &CacheMeta) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 8 + 8);
    match meta.content_length {
        Some(len) => {
            buf.push(1);
            buf.extend_from_slice(&(len as u64).to_be_bytes());
        }
        None => {
            buf.push(0);
            buf.extend_from_slice(&0u64.to_be_bytes());
        }
    }
    buf.extend_from_slice(&meta.ttl.as_secs().to_be_bytes());
    buf
}

// TODO: Can we add a const with a decriptive name for 17.
// Is this the most performant way of filling len_bytes?
// Is there a more idiomatic way for constructing the CacheMeta?
fn deserialize_meta(buf: &[u8]) -> CacheMeta {
    if buf.len() < 17 {
        return CacheMeta::default();
    }
    let flag = buf[0];
    let mut len_bytes = [0u8; 8];
    len_bytes.copy_from_slice(&buf[1..9]);
    let mut ttl_bytes = [0u8; 8];
    ttl_bytes.copy_from_slice(&buf[9..17]);

    let mut meta = CacheMeta::default();
    meta.content_length = (flag == 1).then_some(u64::from_be_bytes(len_bytes) as usize);
    meta.ttl = Duration::from_secs(u64::from_be_bytes(ttl_bytes));
    meta
}

// TODO: What is the underlying hasher?
// Do we need to contruct a new hasher for every call,
// is there a perf penatly for consturcitng the hasher,
// can the hasher be reused?
fn object_id_for(key: &CacheKey) -> ObjectId {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    let hashed = hasher.finish();
    ObjectId {
        bucket: None,
        key: format!("obj-{hashed:x}"),
    }
}

pub struct DurableTier<B: BlobStore, I: IndexStore> {
    blob_store: Arc<B>,
    index_store: Arc<I>,
}

impl<B: BlobStore, I: IndexStore> DurableTier<B, I> {
    pub fn new(blob_store: Arc<B>, index_store: Arc<I>) -> Self {
        Self {
            blob_store,
            index_store,
        }
    }
}

struct BlobHit {
    reader: Box<dyn BlobRead>,
}

#[async_trait]
impl HitHandler for BlobHit {
    async fn read_body(&mut self) -> CacheResult<Option<Vec<u8>>> {
        self.reader.next_chunk().await.map_err(map_durable_err)
    }

    async fn finish(self: Box<Self>) -> CacheResult<()> {
        Ok(())
    }
}

struct BlobMiss<B: BlobStore, I: IndexStore> {
    index: Arc<I>,
    cache_key: CacheKey,
    object: ObjectId,
    meta: CacheMeta,
    writer: Box<dyn BlobWrite>,
    phantom: PhantomData<B>,
}

#[async_trait]
impl<B: BlobStore, I: IndexStore> MissHandler for BlobMiss<B, I> {
    async fn write_body(&mut self, data: Vec<u8>, _eof: bool) -> CacheResult<()> {
        self.writer.write(data).await.map_err(map_durable_err)
    }

    async fn finish(self: Box<Self>) -> CacheResult<MissFinishType> {
        let object_meta = self.writer.complete().await.map_err(map_durable_err)?;
        let manifest = Manifest {
            object: self.object.clone(),
            len: object_meta.len,
            etag: object_meta.etag,
            meta_blob: serialize_meta(&self.meta),
            expires_at: SystemTime::now() + self.meta.ttl,
            version: 0,
        };
        match self
            .index
            .put_new(self.cache_key.as_slice(), &manifest)
            .await
        {
            Ok(_) => Ok(MissFinishType::Success),
            Err(DurableError::PreconditionFailed) => Ok(MissFinishType::Success),
            Err(e) => Err(map_durable_err(e)),
        }
    }
}

#[async_trait]
impl<B: BlobStore + 'static, I: IndexStore + 'static> Storage for DurableTier<B, I> {
    async fn lookup(&self, key: &CacheKey) -> CacheResult<LookupResult> {
        let manifest = match self.index_store.get(key.as_slice()).await {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(LookupResult::Miss),
            Err(e) => return Err(map_durable_err(e)),
        };

        let now = SystemTime::now();
        let (obj_meta, reader) = self
            .blob_store
            .get(&manifest.object)
            .await
            .map_err(map_durable_err)?;

        let mut meta = deserialize_meta(&manifest.meta_blob);
        if meta.content_length.is_none() {
            meta.content_length = Some(obj_meta.len as usize);
        }

        let hit: Box<dyn HitHandler> = Box::new(BlobHit { reader });

        if manifest.expires_at > now {
            Ok(LookupResult::Fresh { meta, hit })
        } else {
            Ok(LookupResult::Stale { meta, hit })
        }
    }

    async fn get_miss_handler(
        &self,
        key: &CacheKey,
        meta: &CacheMeta,
    ) -> CacheResult<Box<dyn MissHandler>> {
        let id = object_id_for(key);
        let writer = self.blob_store.put(&id).await.map_err(map_durable_err)?;
        Ok(Box::new(BlobMiss::<B, I> {
            index: self.index_store.clone(),
            cache_key: key.clone(),
            object: id,
            meta: meta.clone(),
            writer,
            phantom: PhantomData,
        }))
    }

    async fn purge(&self, key: &CacheKey, typ: PurgeType) -> CacheResult<bool> {
        let manifest = self
            .index_store
            .get(key.as_slice())
            .await
            .map_err(map_durable_err)?;
        if let Some(m) = manifest.clone() {
            if matches!(typ, PurgeType::Hard) {
                let _ = self.blob_store.delete(&m.object).await;
            }
            self.index_store
                .delete(key.as_slice())
                .await
                .map_err(map_durable_err)?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn update_meta(&self, key: &CacheKey, meta: &CacheMeta) -> CacheResult<bool> {
        let Some(mut manifest) = self
            .index_store
            .get(key.as_slice())
            .await
            .map_err(map_durable_err)?
        else {
            return Ok(false);
        };
        manifest.meta_blob = serialize_meta(meta);
        manifest.expires_at = SystemTime::now() + meta.ttl;
        match self
            .index_store
            .cas_update(key.as_slice(), manifest.version, &manifest)
            .await
        {
            Ok(_) => Ok(true),
            Err(DurableError::PreconditionFailed) => Ok(false),
            Err(e) => Err(map_durable_err(e)),
        }
    }

    async fn touch(&self, key: &CacheKey, new_expiry: u64) -> CacheResult<bool> {
        let new_expiry_time = SystemTime::UNIX_EPOCH + Duration::from_secs(new_expiry);
        self.index_store
            .touch_ttl(key.as_slice(), new_expiry_time)
            .await
            .map_err(map_durable_err)?;
        Ok(true)
    }
}
