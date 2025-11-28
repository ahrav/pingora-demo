pub mod cache;
pub mod policy;
pub mod proxy;
pub mod storage;

pub use cache::{
    CacheError, CacheKey, CacheMeta, CacheResult, HitHandler, MissFinishType, MissHandler,
    PurgeType, Storage,
};

// Always export
pub use policy::{CachePolicy, NoOpPolicy, parse_content_length};
pub use proxy::{CacheControlDirectives, CacheDecision, parse_cache_control};

// Only with multi-tier feature
#[cfg(feature = "multi-tier")]
pub use policy::{Policy, Promotion};

#[cfg(feature = "multi-tier")]
pub use storage::multi_tier::MultiTier;
