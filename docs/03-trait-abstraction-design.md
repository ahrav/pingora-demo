# Trait Abstraction: Pluggable BlobStore and IndexStore

## Goal

* Decouple the "durable" L3 tier into two pluggable pieces:
  * a blob/object store that speaks S3-compatible semantics
  * a fast KV/index store for manifests/metadata
* Keep `pingora_cache::Storage` as the only interface your tiers expose to Pingora
* Make L3 generic so you can swap DynamoDB→Cockroach or S3→MinIO/Ceph without touching tier logic

## Core design

* Introduce two small, capability-oriented traits:
  * `BlobStore` for immutable object I/O, optimized for streaming and multipart upload
  * `IndexStore` for small, per-key manifest rows with CAS and TTL
* Build `DurableTier<B, I>` which implements `pingora_cache::Storage` by composing `B: BlobStore` and `I: IndexStore`
* Persist a `Manifest` per cache key in `IndexStore`, pointing at a blob key in `BlobStore`, plus serialized `CacheMeta`
* Reads: `IndexStore.get` → validate TTL → stream `BlobStore.get` through a `HitHandler` wrapper
* Writes: a `MissHandler` that buffers to multipart-size and uses `BlobStore.put` streaming; on commit, persist manifest via `IndexStore.put_conditional`
* Purge: delete manifest and optionally the blob depending on `PurgeType`
* Update-meta: update manifest's serialized `CacheMeta` and TTL only

## New abstractions

```rust
// src/storage/traits.rs
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use std::ops::RangeInclusive;
use std::time::{Duration, SystemTime};

#[derive(thiserror::Error, Debug)]
pub enum DurableError {
    #[error("not found")]
    NotFound,
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("io: {0}")]
    Io(String),
    #[error("backend: {0}")]
    Backend(String),
}

pub type DurableResult<T> = Result<T, DurableError>;

#[derive(Clone, Debug)]
pub struct ObjectMeta {
    pub len: u64,
    pub etag: Option<String>,
    pub last_modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct ObjectId {
    // Bucket can be fixed per store; keep for generality
    pub bucket: Option<String>,
    pub key: String,
}

#[async_trait]
pub trait BlobWrite: Send {
    async fn write(&mut self, chunk: Bytes) -> DurableResult<()>;
    async fn complete(self: Box<Self>) -> DurableResult<ObjectMeta>;
    async fn abort(self: Box<Self>) -> DurableResult<()>;
}

#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    type Reader: Stream<Item = DurableResult<Bytes>> + Send + Unpin + 'static;

    async fn get(
        &self,
        id: &ObjectId,
        // inclusive byte range; None = full
        range: Option<RangeInclusive<u64>>,
    ) -> DurableResult<(ObjectMeta, Self::Reader)>;

    async fn head(&self, id: &ObjectId) -> DurableResult<ObjectMeta>;

    async fn put(&self, id: &ObjectId) -> DurableResult<Box<dyn BlobWrite>>;

    async fn delete(&self, id: &ObjectId) -> DurableResult<()>;
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub object: ObjectId,
    pub len: u64,
    pub etag: Option<String>,
    pub meta_blob: Vec<u8>,     // serialized CacheMeta
    pub expires_at: SystemTime, // authoritative TTL
    pub version: u64,           // for CAS
}

#[async_trait]
pub trait IndexStore: Send + Sync + 'static {
    async fn get(&self, key: &[u8]) -> DurableResult<Option<Manifest>>;
    async fn put_new(&self, key: &[u8], m: &Manifest) -> DurableResult<u64>; // returns version
    async fn cas_update(&self, key: &[u8], expect_ver: u64, m: &Manifest) -> DurableResult<u64>;
    async fn delete(&self, key: &[u8]) -> DurableResult<()>;
    async fn touch_ttl(&self, key: &[u8], new_exp: SystemTime) -> DurableResult<()>;
}
```

## Durable L3 tier using the abstractions

```rust
// src/storage/durable.rs
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use pingora_cache::{
    storage::{HandleHit, HandleMiss, MissFinishType, PurgeType},
    CacheKey, CacheMeta, HitHandler, MissHandler, Storage,
    trace::SpanHandle,
};
use crate::storage::traits::*;
use crate::policy::parse_content_length;
use std::{pin::Pin, task::{Context, Poll}, time::{Duration, SystemTime}};

fn serialize_meta(meta: &CacheMeta) -> Vec<u8> {
    // bincode or postcard; keep opaque
    bincode::serialize(meta).unwrap_or_default()
}
fn deserialize_meta(buf: &[u8]) -> CacheMeta {
    bincode::deserialize(buf).unwrap_or_default()
}
fn default_ttl_from_meta(meta: &CacheMeta) -> Duration {
    // Parse Cache-Control/Expires as you prefer; simple 1h fallback:
    Duration::from_secs(3600)
}
fn object_id_for(key: &CacheKey) -> ObjectId {
    // Stable, collision-resistant pathing; change to your house style
    // e.g. sha256 hex split 2/2/...
    ObjectId { bucket: None, key: format!("v1/{:x}", blake3::hash(key.as_slice())) }
}

pub struct DurableTier<B: BlobStore, I: IndexStore> {
    blob: &'static B,
    index: &'static I,
}

impl<B: BlobStore, I: IndexStore> DurableTier<B, I> {
    pub fn new(blob: &'static B, index: &'static I) -> Self {
        Self { blob, index }
    }
}

/* ---------- Hit: wrap a BlobStore reader into a HitHandler ---------- */

struct BlobHit<R> {
    reader: R,
    meta: CacheMeta,
    seekable: bool, // keep false initially
}

#[async_trait]
impl<R> HandleHit for BlobHit<R>
where
    R: futures_core::Stream<Item = DurableResult<Bytes>> + Send + Unpin + 'static,
{
    async fn read_body(&mut self) -> pingora_cache::Result<Option<Bytes>> {
        match self.reader.next().await {
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(anyhow::anyhow!("blob read error: {e}").into()),
            None => Ok(None),
        }
    }
    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> pingora_cache::Result<()> {
        Ok(())
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn as_any_mut(&mut self) -> &mut (dyn std::any::Any + Send + Sync) { self }
    fn can_seek(&self) -> bool { self.seekable }
    fn seek(&mut self, _s: usize, _e: Option<usize>) -> pingora_cache::Result<()> {
        Err(anyhow::anyhow!("range-seek not supported yet")).map_err(Into::into)
    }
    fn should_count_access(&self) -> bool { true }
    fn get_eviction_weight(&self) -> usize {
        // Default to content-length if present; helps Pingora's eviction heuristics
        crate::policy::parse_content_length(&self.meta).unwrap_or(0)
    }
}

/* ---------- Miss: write into multipart blob, then publish manifest ---------- */

struct BlobMiss<B: BlobStore, I: IndexStore> {
    blob: &'static B,
    index: &'static I,
    cache_key: CacheKey,
    object: ObjectId,
    meta: CacheMeta,
    writer: Box<dyn BlobWrite>,
    total: usize,
    min_part: usize,
}

#[async_trait]
impl<B: BlobStore, I: IndexStore> HandleMiss for BlobMiss<B, I> {
    async fn write_body(&mut self, data: Bytes, _eof: bool) -> pingora_cache::Result<()> {
        self.total = self.total.saturating_add(data.len());
        self.writer.write(data).await.map_err(|e| anyhow::anyhow!(e.to_string()).into())
    }

    async fn finish(self: Box<Self>) -> pingora_cache::Result<MissFinishType> {
        let meta = self.writer.complete().await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let ttl = default_ttl_from_meta(&self.meta);
        let manifest = Manifest {
            object: self.object.clone(),
            len: meta.len,
            etag: meta.etag,
            meta_blob: serialize_meta(&self.meta),
            expires_at: SystemTime::now() + ttl,
            version: 0,
        };
        // First writer wins; degrade to update if it already exists
        match self.index.put_new(self.cache_key.as_slice(), &manifest).await {
            Ok(_ver) => Ok(MissFinishType::Success),
            Err(DurableError::PreconditionFailed) => Ok(MissFinishType::Success),
            Err(e) => Err(anyhow::anyhow!("index put error: {e}").into()),
        }
    }
}

/* ---------- Storage impl ---------- */

#[async_trait]
impl<B: BlobStore, I: IndexStore> Storage for DurableTier<B, I> {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<Option<(CacheMeta, HitHandler)>> {
        let _ = trace;
        let now = SystemTime::now();
        let m = match self.index.get(key.as_slice()).await {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("index get error: {e}").into()),
        };
        if m.expires_at <= now {
            // expired; treat as miss
            return Ok(None);
        }
        let (obj_meta, reader) = self.blob.get(&m.object, None).await
            .map_err(|e| anyhow::anyhow!("blob get error: {e}"))?;
        let meta = deserialize_meta(&m.meta_blob);
        // Optional: validate len vs header; skip for speed
        let hit = BlobHit { reader, meta: meta.clone(), seekable: false };
        Ok(Some((meta, Box::new(hit))))
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<MissHandler> {
        let _ = trace;
        let object = object_id_for(key);
        let writer = self.blob.put(&object).await
            .map_err(|e| anyhow::anyhow!("blob put error: {e}"))?;
        let mh = BlobMiss::<B, I> {
            blob: self.blob,
            index: self.index,
            cache_key: key.clone(),
            object,
            meta: meta.clone(),
            writer,
            total: 0,
            min_part: 5 * 1024 * 1024,
        };
        Ok(Box::new(mh))
    }

    async fn purge(
        &'static self,
        ckey: &pingora_cache::key::CompactCacheKey,
        typ: PurgeType,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<bool> {
        let _ = trace;
        // We need the full key to delete the object; compact key is fine for index delete
        // Strategy:
        // - best-effort read manifest; if found and hard purge, delete blob
        // - delete index row
        let full_key = ckey.to_cache_key(); // helper in your codebase; or track decode
        let m = self.index.get(full_key.as_slice()).await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        if let Some(m) = m {
            if matches!(typ, PurgeType::Hard) {
                let _ = self.blob.delete(&m.object).await;
            }
        }
        let ok = self.index.delete(full_key.as_slice()).await.is_ok();
        Ok(ok)
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> pingora_cache::Result<bool> {
        let _ = trace;
        let Some(mut cur) = self.index.get(key.as_slice()).await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
        else { return Ok(false) };
        cur.meta_blob = serialize_meta(meta);
        cur.expires_at = SystemTime::now() + default_ttl_from_meta(meta);
        let _ver = self.index.cas_update(key.as_slice(), cur.version, &cur).await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(true)
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

    fn support_streaming_partial_write(&self) -> bool { false }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) { self }
}
```

## Adapters: S3-compatible `BlobStore`

```rust
// src/storage/adapters/blob_s3.rs
use aws_sdk_s3::{primitives::ByteStream, types::SdkError, Client};
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use std::ops::RangeInclusive;
use tokio::sync::mpsc;

use crate::storage::traits::*;

pub struct S3Blob {
    client: Client,
    bucket: String,
    // tune for your needs
    pub multipart_min: usize, // >= 5 MiB
    pub part_concurrency: usize,
}

impl S3Blob {
    pub fn new(client: Client, bucket: impl Into<String>) -> Self {
        Self { client, bucket: bucket.into(), multipart_min: 5*1024*1024, part_concurrency: 4 }
    }
    fn bucket(&self, id: &ObjectId) -> &str {
        id.bucket.as_deref().unwrap_or(&self.bucket)
    }
}

pub struct S3Reader {
    inner: ByteStream, // implements Stream<Item=Result<Bytes, _>>
}

impl Stream for S3Reader {
    type Item = DurableResult<Bytes>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => Poll::Ready(Some(Ok(b.into_bytes()))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(DurableError::Backend(e.to_string())))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct S3Writer {
    client: Client,
    bucket: String,
    key: String,
    upload_id: String,
    tx: mpsc::Sender<Bytes>,
    join: tokio::task::JoinHandle<DurableResult<ObjectMeta>>,
}

#[async_trait]
impl BlobWrite for S3Writer {
    async fn write(&mut self, chunk: Bytes) -> DurableResult<()> {
        self.tx.send(chunk).await.map_err(|e| DurableError::Io(e.to_string()))
    }
    async fn complete(self: Box<Self>) -> DurableResult<ObjectMeta> {
        drop(self.tx);
        self.join.await.map_err(|e| DurableError::Io(e.to_string()))?
    }
    async fn abort(self: Box<Self>) -> DurableResult<()> {
        drop(self.tx);
        // Best-effort abort; not strictly needed if writer task handles it
        Ok(())
    }
}

#[async_trait]
impl BlobStore for S3Blob {
    type Reader = S3Reader;

    async fn get(
        &self, id: &ObjectId, range: Option<RangeInclusive<u64>>,
    ) -> DurableResult<(ObjectMeta, Self::Reader)> {
        let mut req = self.client.get_object()
            .bucket(self.bucket(id))
            .key(&id.key);
        if let Some(r) = range {
            req = req.range(format!("bytes={}-{}", *r.start(), *r.end()));
        }
        let out = req.send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
        let meta = ObjectMeta {
            len: out.content_length().unwrap_or_default() as u64,
            etag: out.e_tag().map(str::to_string),
            last_modified: out.last_modified().map(|t| t.to_system_time().unwrap_or_else(|_| std::time::SystemTime::now())),
        };
        Ok((meta, S3Reader { inner: out.body }))
    }

    async fn head(&self, id: &ObjectId) -> DurableResult<ObjectMeta> {
        let out = self.client.head_object()
            .bucket(self.bucket(id))
            .key(&id.key)
            .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
        Ok(ObjectMeta {
            len: out.content_length.unwrap_or_default() as u64,
            etag: out.e_tag.map(|s| s),
            last_modified: out.last_modified.map(|t| t.to_system_time().unwrap_or_else(|_| std::time::SystemTime::now())),
        })
    }

    async fn put(&self, id: &ObjectId) -> DurableResult<Box<dyn BlobWrite>> {
        let init = self.client.create_multipart_upload()
            .bucket(self.bucket(id))
            .key(&id.key)
            .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
        let (tx, mut rx) = mpsc::channel::<Bytes>(self.part_concurrency * 2);
        let client = self.client.clone();
        let bucket = self.bucket(id).to_string();
        let key = id.key.clone();
        let upload_id = init.upload_id().unwrap_or_default().to_string();

        let join = tokio::spawn(async move {
            // naive sequential parts; parallelize if you need
            let mut part_no: i32 = 1;
            let mut parts = Vec::new();
            let mut buf = bytes::BytesMut::new();
            while let Some(mut chunk) = rx.recv().await {
                buf.extend_from_slice(&chunk);
                while buf.len() >= 5*1024*1024 {
                    let take = buf.split_to(5*1024*1024).freeze();
                    let out = client.upload_part()
                        .bucket(&bucket).key(&key).upload_id(&upload_id)
                        .part_number(part_no).body(ByteStream::from(take))
                        .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
                    parts.push(aws_sdk_s3::types::CompletedPart::builder()
                        .part_number(part_no).e_tag(out.e_tag.unwrap_or_default()).build());
                    part_no += 1;
                }
            }
            if !buf.is_empty() {
                let out = client.upload_part()
                    .bucket(&bucket).key(&key).upload_id(&upload_id)
                    .part_number(part_no).body(ByteStream::from(buf.freeze()))
                    .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
                parts.push(aws_sdk_s3::types::CompletedPart::builder()
                    .part_number(part_no).e_tag(out.e_tag.unwrap_or_default()).build());
            }
            let comp = client.complete_multipart_upload()
                .bucket(&bucket).key(&key).upload_id(&upload_id)
                .multipart_upload(aws_sdk_s3::types::CompletedMultipartUpload::builder()
                    .set_parts(Some(parts)).build())
                .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
            Ok(ObjectMeta {
                len: comp.content_length.unwrap_or_default() as u64,
                etag: comp.e_tag,
                last_modified: None,
            })
        });

        Ok(Box::new(S3Writer {
            client: self.client.clone(),
            bucket: bucket.to_string(),
            key: id.key.clone(),
            upload_id,
            tx,
            join,
        }))
    }

    async fn delete(&self, id: &ObjectId) -> DurableResult<()> {
        self.client.delete_object()
            .bucket(self.bucket(id)).key(&id.key)
            .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
        Ok(())
    }
}
```

## Adapters: DynamoDB and Cockroach `IndexStore`

```rust
// src/storage/adapters/index_dynamo.rs
use async_trait::async_trait;
use aws_sdk_dynamodb::{types::AttributeValue as AV, Client};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::storage::traits::*;

pub struct DdbIndex {
    client: Client,
    table: String,
    // attribute names: pk, ver, exp, blob, meta, len, etag, bkt, key
}

impl DdbIndex {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self { client, table: table.into() }
    }
}

#[async_trait]
impl IndexStore for DdbIndex {
    async fn get(&self, key: &[u8]) -> DurableResult<Option<Manifest>> {
        let out = self.client.get_item()
            .table_name(&self.table)
            .key("pk", AV::B(key.into()))
            .consistent_read(true)
            .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
        let Some(item) = out.item else { return Ok(None) };
        let version = item.get("ver").and_then(|v| v.as_n().ok()).and_then(|s| s.parse().ok()).unwrap_or(0);
        let exp = item.get("exp").and_then(|v| v.as_n().ok()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let expires_at = UNIX_EPOCH + std::time::Duration::from_secs(exp);
        let len = item.get("len").and_then(|v| v.as_n().ok()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let etag = item.get("etag").and_then(|v| v.as_s().ok()).cloned();
        let meta_blob = item.get("meta").and_then(|v| v.as_b().ok()).map(|b| b.clone().into_inner()).unwrap_or_default();
        let object = ObjectId {
            bucket: item.get("bkt").and_then(|v| v.as_s().ok()).cloned(),
            key: item.get("key").and_then(|v| v.as_s().ok()).cloned().unwrap_or_default(),
        };
        Ok(Some(Manifest { object, len, etag, meta_blob, expires_at, version }))
    }

    async fn put_new(&self, key: &[u8], m: &Manifest) -> DurableResult<u64> {
        let exp = m.expires_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        self.client.put_item()
            .table_name(&self.table)
            .item("pk", AV::B(key.into()))
            .item("ver", AV::N("1".into()))
            .item("exp", AV::N(exp.to_string()))
            .item("len", AV::N(m.len.to_string()))
            .item("etag", m.etag.as_ref().map(|s| AV::S(s.clone())).unwrap_or(AV::Null(true)))
            .item("meta", AV::B(m.meta_blob.clone().into()))
            .item("bkt", m.object.bucket.as_ref().map(|s| AV::S(s.clone())).unwrap_or(AV::Null(true)))
            .item("key", AV::S(m.object.key.clone()))
            .condition_expression("attribute_not_exists(pk)")
            .send().await.map_err(|e| DurableError::PreconditionFailed)?;
        Ok(1)
    }

    async fn cas_update(&self, key: &[u8], expect_ver: u64, m: &Manifest) -> DurableResult<u64> {
        let next = expect_ver + 1;
        let exp = m.expires_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        self.client.update_item()
            .table_name(&self.table)
            .key("pk", AV::B(key.into()))
            .update_expression("SET meta=:m, exp=:e, ver=:n")
            .condition_expression("ver=:v")
            .expression_attribute_values(":m", AV::B(m.meta_blob.clone().into()))
            .expression_attribute_values(":e", AV::N(exp.to_string()))
            .expression_attribute_values(":n", AV::N(next.to_string()))
            .expression_attribute_values(":v", AV::N(expect_ver.to_string()))
            .send().await.map_err(|e| DurableError::PreconditionFailed)?;
        Ok(next)
    }

    async fn delete(&self, key: &[u8]) -> DurableResult<()> {
        self.client.delete_item().table_name(&self.table)
            .key("pk", AV::B(key.into()))
            .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn touch_ttl(&self, key: &[u8], new_exp: SystemTime) -> DurableResult<()> {
        let exp = new_exp.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        self.client.update_item().table_name(&self.table)
            .key("pk", AV::B(key.into()))
            .update_expression("SET exp=:e")
            .expression_attribute_values(":e", AV::N(exp.to_string()))
            .send().await.map_err(|e| DurableError::Backend(e.to_string()))?;
        Ok(())
    }
}
```

```rust
// src/storage/adapters/index_cockroach.rs
use async_trait::async_trait;
use sqlx::{PgPool, Row};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::storage::traits::*;

pub struct CrdbIndex {
    pool: PgPool,
    // schema:
    // CREATE TABLE cache_manifest(
    //   pk BYTES PRIMARY KEY,
    //   ver INT8 NOT NULL,
    //   exp TIMESTAMPTZ NOT NULL,
    //   len INT8 NOT NULL,
    //   etag STRING NULL,
    //   bkt STRING NULL,
    //   obj STRING NOT NULL,
    //   meta BYTES NOT NULL
    // );
    // CREATE INDEX ON cache_manifest (exp);
}

impl CrdbIndex { pub fn new(pool: PgPool) -> Self { Self { pool } } }

#[async_trait]
impl IndexStore for CrdbIndex {
    async fn get(&self, key: &[u8]) -> DurableResult<Option<Manifest>> {
        let row = sqlx::query("SELECT ver, exp, len, etag, bkt, obj, meta FROM cache_manifest WHERE pk=$1")
            .bind(key).fetch_optional(&self.pool).await
            .map_err(|e| DurableError::Backend(e.to_string()))?;
        if let Some(r) = row {
            let version: i64 = r.get(0);
            let exp: chrono::DateTime<chrono::Utc> = r.get(1);
            let len: i64 = r.get(2);
            let etag: Option<String> = r.get(3);
            let bkt: Option<String> = r.get(4);
            let obj: String = r.get(5);
            let meta_blob: Vec<u8> = r.get(6);
            Ok(Some(Manifest {
                object: ObjectId { bucket: bkt, key: obj },
                len: len as u64,
                etag,
                meta_blob,
                expires_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(exp.timestamp() as u64),
                version: version as u64,
            }))
        } else { Ok(None) }
    }

    async fn put_new(&self, key: &[u8], m: &Manifest) -> DurableResult<u64> {
        let ver = 1_i64;
        let exp = chrono::DateTime::<chrono::Utc>::from(m.expires_at);
        let res = sqlx::query("INSERT INTO cache_manifest(pk, ver, exp, len, etag, bkt, obj, meta)
                               VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (pk) DO NOTHING")
            .bind(key).bind(ver).bind(exp)
            .bind(m.len as i64).bind(&m.etag).bind(&m.object.bucket)
            .bind(&m.object.key).bind(&m.meta_blob)
            .execute(&self.pool).await.map_err(|e| DurableError::Backend(e.to_string()))?;
        if res.rows_affected() == 0 { return Err(DurableError::PreconditionFailed); }
        Ok(ver as u64)
    }

    async fn cas_update(&self, key: &[u8], expect_ver: u64, m: &Manifest) -> DurableResult<u64> {
        let next = (expect_ver + 1) as i64;
        let exp = chrono::DateTime::<chrono::Utc>::from(m.expires_at);
        let res = sqlx::query("UPDATE cache_manifest
            SET meta=$1, exp=$2, ver=$3 WHERE pk=$4 AND ver=$5")
            .bind(&m.meta_blob).bind(exp).bind(next)
            .bind(key).bind(expect_ver as i64)
            .execute(&self.pool).await.map_err(|e| DurableError::Backend(e.to_string()))?;
        if res.rows_affected() == 0 { return Err(DurableError::PreconditionFailed); }
        Ok(next as u64)
    }

    async fn delete(&self, key: &[u8]) -> DurableResult<()> {
        sqlx::query("DELETE FROM cache_manifest WHERE pk=$1")
            .bind(key).execute(&self.pool).await
            .map_err(|e| DurableError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn touch_ttl(&self, key: &[u8], new_exp: SystemTime) -> DurableResult<()> {
        let exp = chrono::DateTime::<chrono::Utc>::from(new_exp);
        sqlx::query("UPDATE cache_manifest SET exp=$1 WHERE pk=$2")
            .bind(exp).bind(key).execute(&self.pool).await
            .map_err(|e| DurableError::Backend(e.to_string()))?;
        Ok(())
    }
}
```

## Wire `DurableTier` into your existing `MultiTier`

```rust
// src/bin/main.rs (excerpt)
use once_cell::sync::Lazy;
use pingora::prelude::*;
use crate::storage::multi_tier::MultiTier;
use crate::policy::Policy;

use crate::storage::durable::DurableTier;
use crate::storage::adapters::blob_s3::S3Blob;
use crate::storage::adapters::index_cockroach::CrdbIndex;
// or: use crate::storage::adapters::index_dynamo::DdbIndex;

static L3_DURABLE: Lazy<&'static DurableTier<S3Blob, CrdbIndex>> = Lazy::new(|| {
    // Build S3 client and Cockroach pool in your app init
    let s3 = S3Blob::new(/* aws sdk client */, /* bucket */);
    let idx = CrdbIndex::new(/* sqlx PgPool */);
    Box::leak(Box::new(DurableTier::new(Box::leak(Box::new(s3)), Box::leak(Box::new(idx)))))
});

type Tiers = MultiTier<crate::storage::l0_inproc::L0InProc,
                       crate::storage::l1_redis::L1Redis,
                       crate::storage::l2_mid::L2Mid,
                       DurableTier<S3Blob, CrdbIndex>>;

static TIERS: Lazy<&'static Tiers> = Lazy::new(|| {
    // Choose which tiers you want enabled today
    let l3 = L3_DURABLE;
    Box::leak(Box::new(MultiTier::new(None, None, None, l3, Policy::default())))
});
```

## Why this split works

* `BlobStore` hides S3 vs MinIO vs Ceph RGW vs local FS, focuses on streaming and multipart
* `IndexStore` hides DynamoDB vs Cockroach vs Redis, focuses on CAS, TTL, and small rows
* `DurableTier` remains small and only knows Pingora's `Storage` contract

## Required semantics and tradeoffs

* Range/seek: current hit wrapper sets `can_seek=false`. If you need Range requests, reissue `BlobStore.get` with a `RangeInclusive` on `seek`. That needs idempotent readers and small internal state.
* TTL: `default_ttl_from_meta` is a stub. Parse `Cache-Control` and `Expires` correctly to avoid hot-key churn.
* Concurrency: Put path uses a single-threaded multipart uploader for clarity. Parallelize parts and add retries with idempotency tokens for S3 5xx.
* CAS: `IndexStore` exposes `put_new` and `cas_update`. These guarantee manifest atomicity across backends. Cockroach uses `WHERE ver = ?`; Dynamo uses conditional expressions.
* Purge: `PurgeType::Hard` deletes blob and manifest. `Soft` deletes manifest only, which will naturally miss and refill.
* Content-Length unknown: Multipart uploader buffers to 5 MiB. For small objects, you could swap to single PUT if `Content-Length` is known and ≤ 5 MiB.
* Backpressure: channel sizes in S3 writer are tuneables. Tie them to your Pingora body chunk size to avoid buffering explosions.

## Drop-in migration path

* Ship `DurableTier<S3Blob, DdbIndex>` first to match today's DynamoDB+S3
* Add `CrdbIndex` and flip to `DurableTier<S3Blob, CrdbIndex>` when ready
* To run against MinIO or RGW, point `S3Blob` at a custom endpoint; no code changes

## Gaps to fill later

* Proper `Cache-Control` parsing and revalidation logic
* Range/seek support via new GET per seek
* Optional encryption and KMS headers in `BlobStore::put`
* Structured error mapping to Pingora metrics
* Batch purge in `IndexStore` for prefix invalidation

## Challenge assumptions

* If you require read-your-writes immediately across regions, S3-compatible backends and Cockroach consistency settings will influence freshness guarantees. Verify your RTO/RPO and promotion policy thresholds.
* If you need update-in-place of blobs, S3 semantics are immutable. The manifest indirection is the correct place to "update."

## What you change in your codebase

* Replace `DynamoS3Storage` with `DurableTier<B, I>`
* Keep `MultiTier` exactly as-is
* Add `traits.rs`, `durable.rs`, and the two adapters shown
* Wire feature flags for `aws-sdk-s3` and `sqlx` crates to keep compile units small

This gives you a clean separation: Pingora-facing `Storage` at the top, composable `BlobStore` and `IndexStore` at the bottom. You can now swap DynamoDB↔Cockroach or S3↔MinIO with no changes to promotion logic or multi-tier fanout.
