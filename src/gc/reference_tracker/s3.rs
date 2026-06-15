//! S3 Sidecar-based reference tracker.
//!
//! Stores reference sets as JSON files at `shard_refs/{shard_hash}.refs.json`.
//! This approach works with any S3-compatible storage and doesn't require
//! external databases or services.
//!
//! # Storage Layout
//!
//! ```text
//! s3://bucket/
//! ├── shards/
//! │   └── {shard_hash}
//! ├── shard_refs/
//! │   └── {shard_hash}.refs.json    ← ReferenceSet JSON
//! ├── xorbs/
//! │   └── {xorb_hash}
//! └── .gc/
//!     ├── checkpoint.json
//!     └── bloom.bin
//! ```

use crate::gc::errors::{GcError, GcResult};
use crate::gc::reference_tracker::{ReferenceSet, ReferenceTracker};
use crate::storage::StorageBackend;
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

/// S3 sidecar-based reference tracker.
///
/// Each shard has an associated `.refs.json` sidecar file containing the
/// list of xorb and LFS references extracted from the shard.
pub struct SidecarReferenceTracker {
    storage: Arc<Box<dyn StorageBackend>>,
}

impl SidecarReferenceTracker {
    /// Create a new sidecar reference tracker.
    pub fn new(storage: Arc<Box<dyn StorageBackend>>) -> Self {
        Self { storage }
    }

    /// Construct the sidecar key for a given shard hash.
    fn sidecar_key(shard_hash: &str) -> String {
        format!("shard_refs/{}.refs.json", shard_hash)
    }
}

#[async_trait]
impl ReferenceTracker for SidecarReferenceTracker {
    async fn record_references(
        &self,
        shard_hash: &str,
        lfs_refs: &[String],
        xorb_refs: &[String],
    ) -> GcResult<()> {
        let refs = ReferenceSet {
            version: 1,
            shard_hash: shard_hash.to_string(),
            lfs_refs: lfs_refs.to_vec(),
            xorb_refs: xorb_refs.to_vec(),
            created_at: chrono::Utc::now(),
        };

        let json = serde_json::to_vec_pretty(&refs)
            .map_err(|e| GcError::Json(e))?;

        let key = Self::sidecar_key(shard_hash);
        self.storage.put(&key, Bytes::from(json))
            .await
            .map_err(|e| GcError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to write sidecar {}: {}", key, e),
            )))?;

        tracing::debug!(
            shard_hash = %shard_hash,
            lfs_refs = lfs_refs.len(),
            xorb_refs = xorb_refs.len(),
            "Recorded shard references in sidecar"
        );

        Ok(())
    }

    async fn remove_references(&self, shard_hash: &str) -> GcResult<()> {
        let key = Self::sidecar_key(shard_hash);
        // Ignore NotFound — sidecar may not exist if shard was never tracked
        match self.storage.delete(&key).await {
            Ok(()) => Ok(()),
            Err(crate::storage::StorageError::NotFound(_)) => Ok(()),
            Err(e) => Err(GcError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to delete sidecar {}: {}", key, e),
            ))),
        }
    }

    async fn get_references(&self, shard_hash: &str) -> GcResult<Option<ReferenceSet>> {
        let key = Self::sidecar_key(shard_hash);
        match self.storage.get(&key).await {
            Ok(data) => {
                let refs: ReferenceSet = serde_json::from_slice(&data)
                    .map_err(|e| GcError::Json(e))?;
                Ok(Some(refs))
            }
            Err(crate::storage::StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(GcError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to read sidecar {}: {}", key, e),
            ))),
        }
    }

    async fn list_all_references(&self) -> GcResult<Vec<ReferenceSet>> {
        let keys = self.storage.list_objects("shard_refs/")
            .await
            .map_err(|e| GcError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to list sidecars: {}", e),
            )))?;

        let mut refs = Vec::with_capacity(keys.len());
        for key in keys {
            // Skip non-JSON files (shouldn't happen, but be defensive)
            if !key.ends_with(".refs.json") {
                continue;
            }

            match self.storage.get(&key).await {
                Ok(data) => {
                    match serde_json::from_slice::<ReferenceSet>(&data) {
                        Ok(ref_set) => refs.push(ref_set),
                        Err(e) => {
                            tracing::warn!("Skipping corrupted sidecar {}: {}", key, e);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Skipping unreadable sidecar {}: {}", key, e);
                }
            }
        }

        Ok(refs)
    }

    async fn health_check(&self) -> GcResult<()> {
        // Simple health check: verify we can list the shard_refs prefix
        let _ = self.storage.list_objects("shard_refs/")
            .await
            .map_err(|e| GcError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Health check failed: {}", e),
            )))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::local::LocalStorage;
    use tempfile::TempDir;

    fn make_tracker() -> (SidecarReferenceTracker, TempDir) {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(
            LocalStorage::new(tmp.path().to_str().unwrap()).unwrap()
        ));
        (SidecarReferenceTracker::new(storage), tmp)
    }

    #[tokio::test]
    async fn test_record_and_get_references() {
        let (tracker, _tmp) = make_tracker();

        let lfs_refs = vec!["lfs_hash_1".to_string(), "lfs_hash_2".to_string()];
        let xorb_refs = vec!["xorb_hash_a".to_string(), "xorb_hash_b".to_string(), "xorb_hash_c".to_string()];

        tracker.record_references("shard_abc", &lfs_refs, &xorb_refs).await.unwrap();

        let refs = tracker.get_references("shard_abc").await.unwrap().unwrap();
        assert_eq!(refs.shard_hash, "shard_abc");
        assert_eq!(refs.lfs_refs, lfs_refs);
        assert_eq!(refs.xorb_refs, xorb_refs);
        assert_eq!(refs.total_refs(), 5);
    }

    #[tokio::test]
    async fn test_get_references_not_found() {
        let (tracker, _tmp) = make_tracker();

        let refs = tracker.get_references("nonexistent").await.unwrap();
        assert!(refs.is_none());
    }

    #[tokio::test]
    async fn test_remove_references() {
        let (tracker, _tmp) = make_tracker();

        tracker.record_references("shard_xyz", &["lfs1".to_string()], &["xorb1".to_string()]).await.unwrap();
        assert!(tracker.get_references("shard_xyz").await.unwrap().is_some());

        tracker.remove_references("shard_xyz").await.unwrap();
        assert!(tracker.get_references("shard_xyz").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_remove_nonexistent_is_ok() {
        let (tracker, _tmp) = make_tracker();

        // Removing a non-existent sidecar should succeed
        tracker.remove_references("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn test_list_all_references() {
        let (tracker, _tmp) = make_tracker();

        tracker.record_references("shard_1", &["lfs1".to_string()], &["xorb1".to_string()]).await.unwrap();
        tracker.record_references("shard_2", &["lfs2".to_string()], &["xorb2".to_string()]).await.unwrap();
        tracker.record_references("shard_3", &[], &["xorb3".to_string()]).await.unwrap();

        let all = tracker.list_all_references().await.unwrap();
        assert_eq!(all.len(), 3);
    }
}
