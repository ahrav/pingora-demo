use crate::cache::CacheMeta;

/// Generic cache admission policy trait.
///
/// Implementations decide whether content should be admitted to a cache
/// based on metadata like content length, TTL, or other criteria.
pub trait CachePolicy: Send + Sync {
    /// Returns true if the content described by `meta` should be cached.
    fn should_cache(&self, meta: &CacheMeta) -> bool;

    /// Returns the maximum cacheable size in bytes, if any.
    /// Returns `None` if there is no size limit (unlimited).
    fn max_size(&self) -> Option<usize>;
}

/// A no-op policy that caches everything.
///
/// This is the default policy for single-tier usage where all content
/// is admitted regardless of size.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOpPolicy;

impl CachePolicy for NoOpPolicy {
    fn should_cache(&self, _meta: &CacheMeta) -> bool {
        true
    }

    fn max_size(&self) -> Option<usize> {
        None
    }
}

#[cfg(feature = "multi-tier")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Promotion {
    Never,
    OnStreamCommitAtFinish,
}

#[cfg(feature = "multi-tier")]
#[derive(Clone, Debug)]
pub struct Policy {
    pub l0_max: usize,
    pub l1_max: usize,
    pub l2_max: usize,

    pub promote_from_l1_to_l0: Promotion,
    pub promote_from_l2_to_l0: Promotion,
    pub promote_from_l2_to_l1: Promotion,
    pub promote_from_l3_to_l0: Promotion,
    pub promote_from_l3_to_l1: Promotion,
    pub promote_from_l3_to_l2: Promotion,
}

#[cfg(feature = "multi-tier")]
impl Default for Policy {
    fn default() -> Self {
        Self {
            l0_max: 2 * 1024 * 1024,
            l1_max: 64 * 1024 * 1024,
            l2_max: usize::MAX,
            promote_from_l1_to_l0: Promotion::OnStreamCommitAtFinish,
            promote_from_l2_to_l0: Promotion::OnStreamCommitAtFinish,
            promote_from_l2_to_l1: Promotion::OnStreamCommitAtFinish,
            promote_from_l3_to_l0: Promotion::OnStreamCommitAtFinish,
            promote_from_l3_to_l1: Promotion::OnStreamCommitAtFinish,
            promote_from_l3_to_l2: Promotion::OnStreamCommitAtFinish,
        }
    }
}

pub fn parse_content_length(meta: &CacheMeta) -> Option<usize> {
    meta.content_length
}
