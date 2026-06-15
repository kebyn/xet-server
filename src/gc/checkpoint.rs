//! GC Checkpoint for crash recovery and incremental scanning.
//!
//! The checkpoint records the scanner's position so GC can resume from
//! where it left off after a crash or restart, instead of re-scanning
//! the entire storage from the beginning.
//!
//! # Storage Location
//!
//! Checkpoint is stored at `.gc/checkpoint.json` in the storage backend.

use crate::gc::errors::{GcError, GcResult};
use crate::storage::StorageBackend;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Key for the checkpoint file in storage.
const CHECKPOINT_KEY: &str = ".gc/checkpoint.json";

/// Current checkpoint format version.
const CHECKPOINT_VERSION: u32 = 1;

/// Status of a GC cycle tracked by the checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CheckpointStatus {
    /// GC scan is in progress.
    InProgress,
    /// GC scan completed successfully.
    Completed,
    /// GC scan failed with an error message.
    Failed(String),
}

/// GC Checkpoint for crash recovery and incremental scanning.
///
/// The checkpoint records:
/// - The S3 pagination cursor (for resuming listing from where we left off)
/// - Counts of objects scanned so far
/// - Timestamps for cycle tracking
/// - Status of the current GC cycle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcCheckpoint {
    /// Format version for backward compatibility.
    pub version: u32,

    /// When the checkpoint was last saved.
    pub last_saved_at: DateTime<Utc>,

    /// S3 pagination cursor — the last key seen by the scanner.
    /// Pass this as `start_after` to resume listing from the next object.
    /// None means "start from the beginning".
    pub s3_cursor: Option<String>,

    /// Number of shards scanned in this cycle.
    pub shards_scanned: u64,

    /// Number of xorbs scanned in this cycle.
    pub xorbs_scanned: u64,

    /// Number of LFS blobs scanned in this cycle.
    pub lfs_blobs_scanned: u64,

    /// When the current GC cycle started.
    pub cycle_started_at: DateTime<Utc>,

    /// Current status of the GC cycle.
    pub status: CheckpointStatus,
}

impl GcCheckpoint {
    /// Create a new checkpoint starting from the beginning.
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            version: CHECKPOINT_VERSION,
            last_saved_at: now,
            s3_cursor: None,
            shards_scanned: 0,
            xorbs_scanned: 0,
            lfs_blobs_scanned: 0,
            cycle_started_at: now,
            status: CheckpointStatus::InProgress,
        }
    }

    /// Update the S3 cursor after processing a page.
    pub fn update_cursor(&mut self, cursor: Option<String>) {
        self.s3_cursor = cursor;
        self.last_saved_at = Utc::now();
    }

    /// Increment the shard counter.
    pub fn record_shard_scanned(&mut self) {
        self.shards_scanned += 1;
    }

    /// Increment the xorb counter.
    pub fn record_xorb_scanned(&mut self) {
        self.xorbs_scanned += 1;
    }

    /// Increment the LFS blob counter.
    pub fn record_lfs_blob_scanned(&mut self) {
        self.lfs_blobs_scanned += 1;
    }

    /// Mark the cycle as completed.
    pub fn mark_completed(&mut self) {
        self.status = CheckpointStatus::Completed;
        self.last_saved_at = Utc::now();
    }

    /// Mark the cycle as failed.
    pub fn mark_failed(&mut self, error: String) {
        self.status = CheckpointStatus::Failed(error);
        self.last_saved_at = Utc::now();
    }

    /// Reset the checkpoint for a new GC cycle, preserving the cursor
    /// if the previous cycle was not completed (for crash recovery).
    ///
    /// If the previous cycle was completed, reset the cursor to start fresh.
    pub fn reset_for_new_cycle(&mut self) {
        let was_completed = self.status == CheckpointStatus::Completed;

        self.shards_scanned = 0;
        self.xorbs_scanned = 0;
        self.lfs_blobs_scanned = 0;
        self.cycle_started_at = Utc::now();
        self.status = CheckpointStatus::InProgress;
        self.last_saved_at = Utc::now();

        if was_completed {
            // Previous cycle completed successfully — start from the beginning
            self.s3_cursor = None;
        }
        // If previous cycle was InProgress or Failed, keep the cursor for resume
    }

    /// Save the checkpoint to storage atomically.
    ///
    /// The checkpoint is serialized as JSON and written to `.gc/checkpoint.json`.
    ///
    /// # Atomicity
    ///
    /// Uses `put_atomic` which is crash-safe on both backends:
    /// - **S3**: PUT is already atomic.
    /// - **Local**: Uses write-to-temp + rename pattern.
    pub async fn save(&self, storage: &dyn StorageBackend) -> GcResult<()> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(GcError::Json)?;

        storage.put_atomic(CHECKPOINT_KEY, bytes::Bytes::from(json))
            .await
            .map_err(|e| GcError::Io(std::io::Error::other(
                format!("Storage error: {}", e)
            )))?;

        Ok(())
    }

    /// Load the checkpoint from storage.
    ///
    /// If the checkpoint doesn't exist or is corrupted, returns a fresh checkpoint.
    /// This allows GC to start from scratch after the first run or after corruption.
    pub async fn load(storage: &dyn StorageBackend) -> GcResult<Self> {
        let data = match storage.get(CHECKPOINT_KEY).await {
            Ok(data) => data,
            Err(crate::storage::StorageError::NotFound(_)) => {
                // No checkpoint yet — return fresh
                return Ok(Self::new());
            }
            Err(e) => return Err(GcError::Io(std::io::Error::other(
                format!("Storage error: {}", e)
            ))),
        };

        match serde_json::from_slice::<GcCheckpoint>(&data) {
            Ok(checkpoint) => {
                // Version check
                if checkpoint.version != CHECKPOINT_VERSION {
                    tracing::warn!(
                        "GC checkpoint version mismatch: expected {}, got {}. Starting fresh.",
                        CHECKPOINT_VERSION,
                        checkpoint.version
                    );
                    return Ok(Self::new());
                }
                Ok(checkpoint)
            }
            Err(e) => {
                tracing::warn!("GC checkpoint corrupted: {}. Starting fresh.", e);
                Ok(Self::new())
            }
        }
    }
}

impl Default for GcCheckpoint {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_checkpoint() {
        let cp = GcCheckpoint::new();
        assert_eq!(cp.version, CHECKPOINT_VERSION);
        assert!(cp.s3_cursor.is_none());
        assert_eq!(cp.shards_scanned, 0);
        assert_eq!(cp.status, CheckpointStatus::InProgress);
    }

    #[test]
    fn test_update_cursor() {
        let mut cp = GcCheckpoint::new();
        cp.update_cursor(Some("shards/abc123".to_string()));
        assert_eq!(cp.s3_cursor, Some("shards/abc123".to_string()));

        cp.update_cursor(None);
        assert!(cp.s3_cursor.is_none());
    }

    #[test]
    fn test_counters() {
        let mut cp = GcCheckpoint::new();
        cp.record_shard_scanned();
        cp.record_shard_scanned();
        cp.record_xorb_scanned();
        cp.record_lfs_blob_scanned();

        assert_eq!(cp.shards_scanned, 2);
        assert_eq!(cp.xorbs_scanned, 1);
        assert_eq!(cp.lfs_blobs_scanned, 1);
    }

    #[test]
    fn test_status_transitions() {
        let mut cp = GcCheckpoint::new();
        assert_eq!(cp.status, CheckpointStatus::InProgress);

        cp.mark_completed();
        assert_eq!(cp.status, CheckpointStatus::Completed);

        cp.mark_failed("timeout".to_string());
        assert_eq!(cp.status, CheckpointStatus::Failed("timeout".to_string()));
    }

    #[test]
    fn test_reset_after_completed() {
        let mut cp = GcCheckpoint::new();
        cp.update_cursor(Some("shards/xyz".to_string()));
        cp.shards_scanned = 100;
        cp.mark_completed();

        cp.reset_for_new_cycle();

        // Cursor should be reset since previous cycle was completed
        assert!(cp.s3_cursor.is_none());
        assert_eq!(cp.shards_scanned, 0);
        assert_eq!(cp.status, CheckpointStatus::InProgress);
    }

    #[test]
    fn test_reset_after_failure_preserves_cursor() {
        let mut cp = GcCheckpoint::new();
        cp.update_cursor(Some("shards/resume_here".to_string()));
        cp.shards_scanned = 50;
        cp.mark_failed("crash".to_string());

        cp.reset_for_new_cycle();

        // Cursor should be preserved for crash recovery
        assert_eq!(cp.s3_cursor, Some("shards/resume_here".to_string()));
        assert_eq!(cp.shards_scanned, 0); // Counters reset
        assert_eq!(cp.status, CheckpointStatus::InProgress);
    }

    #[test]
    fn test_checkpoint_json_roundtrip() {
        let mut cp = GcCheckpoint::new();
        cp.update_cursor(Some("shards/abc".to_string()));
        cp.shards_scanned = 42;
        cp.mark_completed();

        let json = serde_json::to_vec(&cp).unwrap();
        let loaded: GcCheckpoint = serde_json::from_slice(&json).unwrap();

        assert_eq!(loaded.version, cp.version);
        assert_eq!(loaded.s3_cursor, cp.s3_cursor);
        assert_eq!(loaded.shards_scanned, 42);
        assert_eq!(loaded.status, CheckpointStatus::Completed);
    }
}
