use async_trait::async_trait;
use std::time::SystemTime;

#[derive(thiserror::Error, Debug)]
pub enum DurableError {
    #[error("not found")]
    NotFound,
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("backend: {0}")]
    Backend(String),
}

pub type DurableResult<T> = Result<T, DurableError>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectId {
    pub bucket: Option<String>,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    pub len: u64,
    pub etag: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub object: ObjectId,
    pub len: u64,
    pub etag: Option<String>,
    pub meta_blob: Vec<u8>,
    pub expires_at: SystemTime,
    pub version: u64,
}

#[async_trait]
pub trait BlobRead: Send {
    async fn next_chunk(&mut self) -> DurableResult<Option<Vec<u8>>>;
}

#[async_trait]
pub trait BlobWrite: Send {
    async fn write(&mut self, chunk: Vec<u8>) -> DurableResult<()>;
    async fn complete(self: Box<Self>) -> DurableResult<ObjectMeta>;
    async fn abort(self: Box<Self>) -> DurableResult<()>;
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn get(&self, id: &ObjectId) -> DurableResult<(ObjectMeta, Box<dyn BlobRead>)>;
    async fn head(&self, id: &ObjectId) -> DurableResult<ObjectMeta>;
    async fn put(&self, id: &ObjectId) -> DurableResult<Box<dyn BlobWrite>>;
    async fn delete(&self, id: &ObjectId) -> DurableResult<()>;
}

#[async_trait]
pub trait IndexStore: Send + Sync {
    async fn get(&self, key: &[u8]) -> DurableResult<Option<Manifest>>;
    async fn put_new(&self, key: &[u8], manifest: &Manifest) -> DurableResult<u64>;
    async fn cas_update(
        &self,
        key: &[u8],
        expect_ver: u64,
        manifest: &Manifest,
    ) -> DurableResult<u64>;
    async fn delete(&self, key: &[u8]) -> DurableResult<()>;
    async fn touch_ttl(&self, key: &[u8], new_expiry: SystemTime) -> DurableResult<()>;
}
