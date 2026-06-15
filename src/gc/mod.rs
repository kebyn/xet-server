//! Garbage collection for orphaned blobs
//!
//! # Incremental GC (v2)
//!
//! The incremental GC system uses a 5-phase approach:
//!
//! 1. **Acquire lease** — multi-node coordination via S3 conditional PUT
//! 2. **Load checkpoint + bloom filter** — crash recovery, resume from cursor
//! 3. **Incremental scan** — populate bloom filter from shard references
//! 4. **Compute candidates** — storage - bloom protected set, apply grace period
//! 5. **Batch delete** — delete orphans, save bloom + checkpoint, release lease
//!
//! Key components:
//! - `BloomFilterProtectedSet` — O(1) probabilistic reference tracking
//! - `GcCheckpoint` — crash recovery via S3 pagination cursor
//! - `IncrementalScanner` — paged scanning with 3-layer defense
//! - `GcCoordinator` — multi-node lease management
//! - `GracePeriod` — two-tier protection (absolute + soft cycles)
//! - `SidecarReferenceTracker` — reference tracking via JSON sidecar files

pub mod errors;
pub mod bloom;
pub mod checkpoint;
pub mod reference_tracker;
pub mod scanner;
pub mod coordinator;
pub mod grace;


// ============================================================================
// Incremental GC (v2) — replaces the legacy full-scan GC above
// ============================================================================

use crate::gc::bloom::BloomFilterProtectedSet;
use crate::gc::checkpoint::GcCheckpoint;
use crate::gc::coordinator::GcCoordinator;
use crate::gc::grace::GracePeriod;
use crate::gc::reference_tracker::ReferenceTracker;
use crate::gc::scanner::IncrementalScanner;
use crate::config::GcConfig;
use crate::storage::StorageBackend;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Statistics from an incremental GC run.
#[derive(Debug, Clone, Default)]
pub struct IncrementalGcStats {
    /// Whether this was a dry run (no actual deletions).
    pub dry_run: bool,
    /// Whether the lease was acquired.
    pub lease_acquired: bool,
    /// Number of shards scanned.
    pub shards_scanned: u64,
    /// Number of references inserted into the bloom filter.
    pub refs_inserted: u64,
    /// Number of candidate objects for deletion (before grace period).
    pub candidates: u64,
    /// Number of objects actually deleted.
    pub deleted_lfs_blobs: usize,
    pub deleted_xorbs: usize,
    /// Note: shards are NEVER deleted by incremental GC (C2 fix).
    /// Shard references are tracked by Hub's file_tree, which we cannot query.
    /// Objects skipped by grace period.
    pub grace_period_skipped: usize,
    /// Objects skipped because they're in the bloom filter (referenced).
    pub bloom_protected: usize,
    /// Errors encountered during deletion.
    pub errors: usize,
    /// Sidecars that were missing (fell back to shard parsing).
    pub sidecar_missing: u64,
    /// Total duration in seconds.
    pub duration_seconds: f64,
    /// When this run completed.
    pub last_run: Option<chrono::DateTime<chrono::Utc>>,
    /// Bloom filter stats.
    pub bloom_items: u64,
    pub bloom_rebuild_count: u32,
    /// Whether the scan completed (vs. timed out).
    pub scan_completed: bool,
}

/// Incremental garbage collector.
///
/// Replaces the legacy full-scan GC with a bloom-filter-based incremental
/// approach. The 5-phase run flow:
///
/// 1. Acquire lease (multi-node coordination)
/// 2. Load checkpoint + bloom filter (crash recovery)
/// 3. Incremental scan → populate bloom filter
/// 4. Compute candidates = storage - bloom protected set → apply grace period
/// 5. Batch delete → save bloom + checkpoint → release lease
pub struct IncrementalGarbageCollector {
    storage: Arc<Box<dyn StorageBackend>>,
    ref_tracker: Arc<dyn ReferenceTracker>,
    config: GcConfig,
    node_id: String,
}

impl IncrementalGarbageCollector {
    /// Create a new incremental GC.
    pub fn new(
        storage: Arc<Box<dyn StorageBackend>>,
        ref_tracker: Arc<dyn ReferenceTracker>,
        config: GcConfig,
    ) -> Result<Self, String> {
        let node_id = uuid::Uuid::new_v4().to_string();
        Ok(Self {
            storage,
            ref_tracker,
            config,
            node_id,
        })
    }

    /// Get the GC configuration.
    pub fn config(&self) -> &GcConfig {
        &self.config
    }

    /// Run a full incremental GC cycle.
    pub async fn run(&self) -> Result<IncrementalGcStats, String> {
        let start = Instant::now();
        let mut stats = IncrementalGcStats {
            dry_run: self.config.dry_run,
            ..Default::default()
        };

        info!("Incremental GC started (dry_run={}, node_id={})", self.config.dry_run, self.node_id);

        // ── Phase 1: Acquire Lease ─────────────────────────────────────────
        let coordinator = Arc::new(GcCoordinator::new(
            self.storage.clone(),
            self.node_id.clone(),
            self.config.lease.clone(),
        ));

        let lease_guard = match coordinator.try_acquire_lease().await {
            Ok(Some(guard)) => {
                stats.lease_acquired = true;
                info!("Acquired GC lease");
                guard
            }
            Ok(None) => {
                info!("Could not acquire GC lease, skipping this cycle");
                stats.duration_seconds = start.elapsed().as_secs_f64();
                return Ok(stats);
            }
            Err(e) => {
                return Err(format!("Failed to acquire lease: {}", e));
            }
        };

        // ── Phase 2: Load Checkpoint + Bloom Filter ────────────────────────
        let mut checkpoint = match GcCheckpoint::load(&**self.storage).await {
            Ok(cp) => {
                info!(
                    cursor = ?cp.s3_cursor,
                    status = ?cp.status,
                    "Loaded GC checkpoint"
                );
                cp
            }
            Err(e) => {
                warn!("Failed to load checkpoint: {}, starting fresh", e);
                GcCheckpoint::new()
            }
        };

        // Reset for new cycle (preserves cursor if previous cycle was incomplete)
        checkpoint.reset_for_new_cycle();

        let mut bloom = match self.load_bloom_filter().await {
            Ok(b) => {
                info!(
                    items = b.stats().items_inserted,
                    "Loaded bloom filter"
                );
                b
            }
            Err(e) => {
                warn!("Failed to load bloom filter: {}, creating new one", e);
                BloomFilterProtectedSet::new(self.config.bloom.clone())
            }
        };

        // ── Phase 3: Incremental Scan ──────────────────────────────────────
        let scanner = IncrementalScanner::new(
            self.storage.clone(),
            self.ref_tracker.clone(),
            self.config.scanner.clone(),
        );

        match scanner.scan(&mut bloom, &mut checkpoint).await {
            Ok(scan_result) => {
                stats.shards_scanned = scan_result.shards_scanned;
                stats.refs_inserted = scan_result.refs_inserted;
                stats.sidecar_missing = scan_result.sidecar_missing;
                stats.scan_completed = scan_result.scan_completed;
                info!(
                    shards = scan_result.shards_scanned,
                    refs = scan_result.refs_inserted,
                    completed = scan_result.scan_completed,
                    "Incremental scan phase completed"
                );

                // C1 fix: If scan is incomplete (timeout), skip deletion phase.
                // The bloom filter only contains a subset of references, so computing
                // deletion candidates would risk deleting referenced data.
                // Save checkpoint to resume next cycle from where we left off.
                if !scan_result.scan_completed {
                    warn!(
                        "Scan incomplete (timeout or error). Skipping deletion phase. \
                         Checkpoint saved — next GC cycle will resume from cursor."
                    );
                    // Save bloom and checkpoint, then return without deleting
                    if let Err(e) = self.save_bloom_filter(&bloom).await {
                        warn!("Failed to save bloom filter: {}", e);
                    }
                    if let Err(e) = checkpoint.save(&**self.storage).await {
                        warn!("Failed to save checkpoint: {}", e);
                    }
                    stats.duration_seconds = start.elapsed().as_secs_f64();
                    stats.last_run = Some(chrono::Utc::now());
                    return Ok(stats);
                }
            }
            Err(e) => {
                error!("Incremental scan failed: {}", e);
                checkpoint.mark_failed(e.to_string());
                let _ = checkpoint.save(&**self.storage).await;
                return Err(format!("Scan failed: {}", e));
            }
        }

        // ── Phase 4: Compute Candidates ────────────────────────────────────
        let grace = GracePeriod::new(&self.config.grace);

        // List all objects and compute deletion candidates.
        // C2 fix: Shards are not candidates for deletion.
        let (lfs_candidates, xorb_candidates) =
            self.compute_candidates(&bloom, &grace, &mut stats).await?;

        info!(
            lfs_candidates = lfs_candidates.len(),
            xorb_candidates = xorb_candidates.len(),
            bloom_protected = stats.bloom_protected,
            grace_skipped = stats.grace_period_skipped,
            "Computed deletion candidates"
        );

        // ── Phase 5: Delete + Save ─────────────────────────────────────────
        if !self.config.dry_run {
            self.delete_candidates(&lfs_candidates, &xorb_candidates, &mut stats).await?;
        } else {
            stats.deleted_lfs_blobs = lfs_candidates.len();
            stats.deleted_xorbs = xorb_candidates.len();
            info!(
                "Dry run: would delete {} LFS, {} xorbs",
                lfs_candidates.len(),
                xorb_candidates.len(),
            );
        }

        // Save bloom filter and checkpoint
        if let Err(e) = self.save_bloom_filter(&bloom).await {
            warn!("Failed to save bloom filter: {}", e);
        }
        if let Err(e) = checkpoint.save(&**self.storage).await {
            warn!("Failed to save checkpoint: {}", e);
        }

        // Update bloom stats
        stats.bloom_items = bloom.stats().items_inserted;
        stats.bloom_rebuild_count = bloom.stats().rebuild_count;

        // Drop the lease guard (releases lease)
        drop(lease_guard);

        stats.duration_seconds = start.elapsed().as_secs_f64();
        stats.last_run = Some(chrono::Utc::now());

        info!(
            "Incremental GC completed in {:.1}s: deleted {} LFS, {} xorbs \
             (dry_run={}, bloom_items={}, scan_completed={})",
            stats.duration_seconds,
            stats.deleted_lfs_blobs,
            stats.deleted_xorbs,
            stats.dry_run,
            stats.bloom_items,
            stats.scan_completed,
        );

        Ok(stats)
    }

    /// Load the bloom filter from storage.
    async fn load_bloom_filter(&self) -> Result<BloomFilterProtectedSet, String> {
        let bloom_key = ".gc/bloom.bin";
        match self.storage.get(bloom_key).await {
            Ok(data) => {
                let mut cursor = std::io::Cursor::new(data.to_vec());
                BloomFilterProtectedSet::load(&mut cursor, self.config.bloom.clone())
                    .map_err(|e| format!("Failed to load bloom filter: {}", e))
            }
            Err(crate::storage::StorageError::NotFound(_)) => {
                Ok(BloomFilterProtectedSet::new(self.config.bloom.clone()))
            }
            Err(e) => Err(format!("Failed to read bloom filter: {}", e)),
        }
    }

    /// Save the bloom filter to storage.
    async fn save_bloom_filter(&self, bloom: &BloomFilterProtectedSet) -> Result<(), String> {
        let bloom_key = ".gc/bloom.bin";
        let mut buf = Vec::new();
        bloom.save(&mut buf)
            .map_err(|e| format!("Failed to serialize bloom filter: {}", e))?;

        self.storage.put(bloom_key, bytes::Bytes::from(buf))
            .await
            .map_err(|e| format!("Failed to save bloom filter: {}", e))?;

        Ok(())
    }

    /// Compute deletion candidates by comparing storage contents against the bloom filter.
    ///
    /// C2 fix: Shards are NOT deleted by incremental GC because we have no way to verify
    /// whether they're still referenced by Hub's file_tree. Shard lifecycle requires
    /// a separate mechanism (e.g., Hub webhook, manual cleanup, or future reference tracking).
    async fn compute_candidates(
        &self,
        bloom: &BloomFilterProtectedSet,
        grace: &GracePeriod,
        stats: &mut IncrementalGcStats,
    ) -> Result<(Vec<(String, u64)>, Vec<(String, u64)>), String> {
        let mut lfs_candidates = Vec::new();
        let mut xorb_candidates = Vec::new();

        // Scan LFS blobs
        let lfs_blobs = self.storage.list_objects_with_mtime("lfs/objects").await
            .map_err(|e| format!("Failed to list LFS blobs: {}", e))?;

        for (key, mtime) in lfs_blobs {
            let oid = key.strip_prefix("lfs/objects/").unwrap_or(&key)
                .split('/').next_back().unwrap_or(&key);

            if bloom.contains(oid.as_bytes()) {
                stats.bloom_protected += 1;
                continue;
            }

            if !grace.can_delete(&key, mtime).await {
                stats.grace_period_skipped += 1;
                continue;
            }

            lfs_candidates.push((key, mtime));
        }

        // Scan xorbs
        let xorbs = self.storage.list_objects_with_mtime("xorbs").await
            .map_err(|e| format!("Failed to list xorbs: {}", e))?;

        for (key, mtime) in xorbs {
            let xorb_hash = key.strip_prefix("xorbs/").unwrap_or(&key);

            if bloom.contains(xorb_hash.as_bytes()) {
                stats.bloom_protected += 1;
                continue;
            }

            if !grace.can_delete(&key, mtime).await {
                stats.grace_period_skipped += 1;
                continue;
            }

            xorb_candidates.push((key, mtime));
        }

        // C2 fix: Shards are scanned for reference extraction (in Phase 3) but NEVER deleted.
        // Shard references are tracked by Hub's file_tree, which the incremental GC cannot query.
        // Deleting shards based on mtime alone would cause cascading data loss:
        //   1. Shard is deleted (age > grace period)
        //   2. Next GC: shard can't be scanned → its xorb/LFS refs not in bloom filter
        //   3. Those xorbs/LFS blobs are then falsely identified as orphans and deleted

        stats.candidates = (lfs_candidates.len() + xorb_candidates.len()) as u64;

        Ok((lfs_candidates, xorb_candidates))
    }

    /// Delete candidate objects in batches.
    ///
    /// C2 fix: Only deletes LFS blobs and xorbs. Shards are NOT deleted by incremental GC
    /// because we cannot verify whether they're still referenced by Hub's file_tree.
    async fn delete_candidates(
        &self,
        lfs_keys: &[(String, u64)],
        xorb_keys: &[(String, u64)],
        stats: &mut IncrementalGcStats,
    ) -> Result<(), String> {
        let batch_size = self.config.delete_batch_size;

        // Delete LFS blobs
        for chunk in lfs_keys.chunks(batch_size) {
            for (key, _) in chunk {
                match self.storage.delete(key).await {
                    Ok(_) => stats.deleted_lfs_blobs += 1,
                    Err(e) => {
                        warn!("Failed to delete LFS blob {}: {}", key, e);
                        stats.errors += 1;
                    }
                }
            }
        }

        // Delete xorbs
        for chunk in xorb_keys.chunks(batch_size) {
            for (key, _) in chunk {
                match self.storage.delete(key).await {
                    Ok(_) => stats.deleted_xorbs += 1,
                    Err(e) => {
                        warn!("Failed to delete xorb {}: {}", key, e);
                        stats.errors += 1;
                    }
                }
            }
        }

        // C2 fix: Shards are NOT deleted. See compute_candidates for rationale.

        Ok(())
    }
}

/// Start the background incremental GC task.
pub async fn start_incremental_gc_background_task(
    gc: Arc<IncrementalGarbageCollector>,
    last_stats: Arc<RwLock<Option<IncrementalGcStats>>>,
) {
    if !gc.config.enabled {
        info!("Incremental GC background task disabled");
        return;
    }

    let interval = Duration::from_secs(gc.config.interval_seconds);

    info!(
        "Starting incremental GC background task (interval={}s, dry_run={})",
        gc.config.interval_seconds, gc.config.dry_run
    );

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;

            match gc.run().await {
                Ok(run_stats) => {
                    *last_stats.write().await = Some(run_stats);
                }
                Err(e) => {
                    error!("Incremental GC background task failed: {}", e);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    // M4 fix: Verify that xorb hash hex encoding matches storage key format.
    #[test]
    fn test_xorb_hash_hex_encoding_consistency() {
        use crate::types::MerkleHash;

        let hash_bytes: [u8; 32] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let hash = MerkleHash::from(hash_bytes);

        assert_eq!(
            hash.to_hex(),
            hex::encode(hash.as_bytes()),
            "MerkleHash::to_hex() must equal hex::encode(as_bytes()) for xorb key consistency"
        );

        let storage_key = format!("xorbs/{}", hash.to_hex());
        let extracted_hash = storage_key.strip_prefix("xorbs/").unwrap();
        assert_eq!(extracted_hash, hash.to_hex());
    }
}
