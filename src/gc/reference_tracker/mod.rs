//! Reference tracker for recording which chunks/xorbs are referenced by each shard.
//!
//! The reference tracker maintains a mapping from shard hashes to their referenced
//! xorb and LFS blob hashes. This information is used by the incremental GC scanner
//! to populate the bloom filter protected set.
//!
//! # Two Modes
//!
//! - **Sidecar** (S3): JSON files at `shard_refs/{hash}.refs.json`
//! - **Local cache** (P1): SQLite database for local storage backends

pub mod s3;

use crate::gc::errors::GcResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Set of references extracted from a single shard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceSet {
    /// Version for backward compatibility.
    pub version: u32,
    /// Hash of the shard these references belong to.
    pub shard_hash: String,
    /// LFS blob hashes referenced by this shard (file hashes).
    pub lfs_refs: Vec<String>,
    /// Xorb hashes referenced by this shard.
    pub xorb_refs: Vec<String>,
    /// When this reference set was created.
    pub created_at: DateTime<Utc>,
}

impl ReferenceSet {
    /// Create a new empty reference set for a shard.
    pub fn new(shard_hash: String) -> Self {
        Self {
            version: 1,
            shard_hash,
            lfs_refs: Vec::new(),
            xorb_refs: Vec::new(),
            created_at: Utc::now(),
        }
    }

    /// Total number of references in this set.
    pub fn total_refs(&self) -> usize {
        self.lfs_refs.len() + self.xorb_refs.len()
    }
}

/// Trait for tracking references between shards and their referenced xorbs/LFS blobs.
///
/// Implementations:
/// - `SidecarReferenceTracker`: stores references as JSON sidecar files in S3
/// - `LocalReferenceTracker` (P1): stores references in a local SQLite database
#[async_trait]
pub trait ReferenceTracker: Send + Sync {
    /// Record the references extracted from a shard.
    ///
    /// Called when a shard is uploaded (via conversion pipeline or direct upload).
    /// The implementation should persist the reference set so GC can read it later.
    async fn record_references(
        &self,
        shard_hash: &str,
        lfs_refs: &[String],
        xorb_refs: &[String],
    ) -> GcResult<()>;

    /// Remove the reference set for a shard.
    ///
    /// Called when a shard is deleted by GC (cascade cleanup).
    async fn remove_references(&self, shard_hash: &str) -> GcResult<()>;

    /// Get the reference set for a specific shard.
    ///
    /// Returns None if no sidecar exists for this shard.
    async fn get_references(&self, shard_hash: &str) -> GcResult<Option<ReferenceSet>>;

    /// List all reference sets (for rebuilding the bloom filter from scratch).
    ///
    /// For large deployments, this may be expensive. The incremental scanner
    /// prefers to read sidecars as it encounters shards during scanning.
    async fn list_all_references(&self) -> GcResult<Vec<ReferenceSet>>;

    /// Health check — verify the reference tracker is operational.
    async fn health_check(&self) -> GcResult<()>;
}
