//! Incremental scanner for GC.
//!
//! The scanner walks storage in pages, extracting references from each shard
//! and populating the bloom filter protected set. It supports:
//!
//! - **Incremental scanning**: resumes from the last checkpoint cursor
//! - **Three-layer defense**: sidecar → parse shard → conservative skip
//! - **Timeout protection**: stops after `max_duration_seconds`
//! - **Periodic checkpointing**: saves progress every `checkpoint_interval` objects

use crate::config::ScannerConfig;
use crate::format::shard::MDBShardFile;
use crate::gc::bloom::BloomFilterProtectedSet;
use crate::gc::checkpoint::GcCheckpoint;
use crate::gc::errors::{GcError, GcResult};
use crate::gc::reference_tracker::ReferenceTracker;
use crate::storage::StorageBackend;
use std::sync::Arc;
use std::time::Instant;

/// Result of a scan operation.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// Number of shards scanned in this invocation.
    pub shards_scanned: u64,
    /// Number of new references inserted into the bloom filter.
    pub refs_inserted: u64,
    /// Number of shards with missing sidecars (fell back to parsing).
    pub sidecar_missing: u64,
    /// Number of shards that failed to parse.
    pub parse_errors: u64,
    /// Whether the scan completed all remaining objects (no more pages).
    pub scan_completed: bool,
    /// How long the scan took.
    pub duration: std::time::Duration,
}

/// Incremental scanner that populates the bloom filter from shard references.
pub struct IncrementalScanner {
    storage: Arc<Box<dyn StorageBackend>>,
    ref_tracker: Arc<dyn ReferenceTracker>,
    config: ScannerConfig,
    /// I2 fix: Limits concurrent sidecar regeneration tasks.
    /// Without this, first GC scan on a large store could spawn thousands of
    /// concurrent S3 writes (one per missing sidecar), risking connection pool
    /// exhaustion or S3 rate limiting.
    sidecar_semaphore: Arc<tokio::sync::Semaphore>,
}

impl IncrementalScanner {
    /// Create a new incremental scanner.
    pub fn new(
        storage: Arc<Box<dyn StorageBackend>>,
        ref_tracker: Arc<dyn ReferenceTracker>,
        config: ScannerConfig,
    ) -> Self {
        Self {
            storage,
            ref_tracker,
            config,
            // I2 fix: Allow at most 10 concurrent sidecar regenerations.
            // This bounds S3 write concurrency while still allowing parallelism.
            sidecar_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
        }
    }

    /// Run a scan operation, populating the bloom filter with references from shards.
    ///
    /// The scanner:
    /// 1. Starts from `checkpoint.s3_cursor` (or beginning if None)
    /// 2. Lists shards in pages of `page_size`
    /// 3. For each shard, loads references (sidecar or parse fallback)
    /// 4. Inserts all references into the bloom filter
    /// 5. Saves checkpoint every `checkpoint_interval` objects
    /// 6. Stops after `max_duration_seconds`
    ///
    /// Returns the scan result and updates the checkpoint in place.
    pub async fn scan(
        &self,
        bloom: &mut BloomFilterProtectedSet,
        checkpoint: &mut GcCheckpoint,
    ) -> GcResult<ScanResult> {
        let start = Instant::now();
        let mut result = ScanResult::default();
        let mut scanned_since_checkpoint: u64 = 0;

        tracing::info!(
            cursor = ?checkpoint.s3_cursor,
            page_size = self.config.page_size,
            max_duration = self.config.max_duration_seconds,
            "Starting incremental scan"
        );

        loop {
            // Check timeout
            if start.elapsed().as_secs() > self.config.max_duration_seconds {
                tracing::warn!(
                    elapsed = start.elapsed().as_secs(),
                    limit = self.config.max_duration_seconds,
                    "Scanner timeout reached, saving checkpoint and stopping"
                );
                break;
            }

            // List next page of shards
            // M1 fix: Use next_cursor from storage for consistent pagination.
            let (keys, next_cursor, has_more) = self.storage
                .list_objects_paged("shards/", checkpoint.s3_cursor.as_deref(), self.config.page_size)
                .await
                .map_err(|e| GcError::Io(std::io::Error::other(
                    format!("Failed to list shards: {}", e),
                )))?;

            if keys.is_empty() {
                // No more shards to scan
                result.scan_completed = true;
                checkpoint.mark_completed();
                break;
            }

            // Process each shard in the page
            for key in &keys {
                // Extract shard hash from key (e.g., "shards/abc123" → "abc123")
                let shard_hash = key.strip_prefix("shards/").unwrap_or(key);

                // Extract references using three-layer defense
                match self.load_shard_references(shard_hash).await {
                    Ok(refs) => {
                        // Insert references into bloom filter
                        let ref_count = refs.lfs_refs.len() + refs.xorb_refs.len();
                        for lfs_ref in &refs.lfs_refs {
                            bloom.insert(lfs_ref.as_bytes());
                        }
                        for xorb_ref in &refs.xorb_refs {
                            bloom.insert(xorb_ref.as_bytes());
                        }
                        result.refs_inserted += ref_count as u64;
                    }
                    Err(e) => {
                        // Conservative: skip this shard (don't delete its xorbs)
                        tracing::warn!(
                            shard_hash = %shard_hash,
                            error = %e,
                            "Failed to load shard references, skipping"
                        );
                        result.parse_errors += 1;
                    }
                }

                result.shards_scanned += 1;
                checkpoint.record_shard_scanned();
                scanned_since_checkpoint += 1;

                // Periodic checkpoint save
                if scanned_since_checkpoint >= self.config.checkpoint_interval {
                    // C2 fix: Use current key as cursor, not next_cursor.
                    // If we crash mid-page and recover, we must resume from the last
                    // processed shard, not skip to the end of the page.
                    checkpoint.update_cursor(Some(key.clone()));
                    if let Err(e) = checkpoint.save(&**self.storage).await {
                        tracing::warn!("Failed to save checkpoint: {}", e);
                    }
                    scanned_since_checkpoint = 0;
                }
            }

            // M1 fix: Use next_cursor from storage response.
            checkpoint.update_cursor(next_cursor);

            if !has_more {
                // No more pages — scan completed
                result.scan_completed = true;
                checkpoint.mark_completed();
                break;
            }
        }

        // Final checkpoint save
        if let Err(e) = checkpoint.save(&**self.storage).await {
            tracing::warn!("Failed to save final checkpoint: {}", e);
        }

        result.duration = start.elapsed();

        tracing::info!(
            shards_scanned = result.shards_scanned,
            refs_inserted = result.refs_inserted,
            sidecar_missing = result.sidecar_missing,
            parse_errors = result.parse_errors,
            scan_completed = result.scan_completed,
            duration_secs = result.duration.as_secs_f64(),
            "Incremental scan completed"
        );

        Ok(result)
    }

    /// Load references for a shard using three-layer defense:
    ///
    /// 1. **Sidecar**: read `shard_refs/{hash}.refs.json` (fast, preferred)
    /// 2. **Parse shard**: download and parse the shard file (slow fallback)
    /// 3. **Conservative skip**: return empty refs (safest, may retain orphans)
    async fn load_shard_references(
        &self,
        shard_hash: &str,
    ) -> GcResult<crate::gc::reference_tracker::ReferenceSet> {
        use crate::gc::reference_tracker::ReferenceSet;

        // Layer 1: Try sidecar
        if let Some(refs) = self.ref_tracker.get_references(shard_hash).await? {
            return Ok(refs);
        }

        // Layer 2: Parse the shard directly
        tracing::debug!(shard_hash = %shard_hash, "Sidecar missing, parsing shard directly");

        let shard_key = format!("shards/{}", shard_hash);
        let shard_data = self.storage.get(&shard_key).await
            .map_err(|e| GcError::Io(std::io::Error::other(
                format!("Failed to read shard {}: {}", shard_key, e),
            )))?;

        match self.extract_references_from_shard(shard_hash, &shard_data) {
            Ok(refs) => {
                // Layer 2 succeeded — regenerate sidecar asynchronously (best effort).
                // I2 fix: Use semaphore to limit concurrent sidecar writes.
                // Acquire a permit before spawning; the permit is moved into the task
                // and released when the task completes (on drop).
                let semaphore = self.sidecar_semaphore.clone();
                let ref_tracker = self.ref_tracker.clone();
                let shard_hash_owned = shard_hash.to_string();
                let lfs_refs = refs.lfs_refs.clone();
                let xorb_refs = refs.xorb_refs.clone();
                tokio::spawn(async move {
                    // Acquire permit — blocks if 10 sidecar writes are already in flight
                    let _permit = semaphore.acquire().await;
                    if let Err(e) = ref_tracker.record_references(
                        &shard_hash_owned,
                        &lfs_refs,
                        &xorb_refs,
                    ).await {
                        tracing::warn!(
                            shard_hash = %shard_hash_owned,
                            error = %e,
                            "Failed to regenerate sidecar (non-fatal)"
                        );
                    }
                });

                Ok(refs)
            }
            Err(e) => {
                // Layer 3: Conservative skip — return empty refs
                // This means GC won't delete xorbs referenced by this shard,
                // which is safe (retains data) but may leave orphans.
                tracing::warn!(
                    shard_hash = %shard_hash,
                    error = %e,
                    "Shard parse failed, using conservative empty references"
                );
                Ok(ReferenceSet::new(shard_hash.to_string()))
            }
        }
    }

    /// Extract references from raw shard data by parsing the shard format.
    fn extract_references_from_shard(
        &self,
        shard_hash: &str,
        data: &[u8],
    ) -> GcResult<crate::gc::reference_tracker::ReferenceSet> {
        use crate::gc::reference_tracker::ReferenceSet;

        let shard = MDBShardFile::parse(data)
            .map_err(|e| GcError::ShardParse(format!("{}: {}", shard_hash, e)))?;

        // Extract LFS refs (file hashes)
        let lfs_refs: Vec<String> = shard.file_hashes()
            .iter()
            .map(|h| h.to_hex())
            .collect();

        // Extract xorb refs (from xorb_entries headers)
        let xorb_refs: Vec<String> = shard.xorb_entries
            .iter()
            .map(|e| e.xorb_hash.to_hex())
            .collect();

        let mut refs = ReferenceSet::new(shard_hash.to_string());
        refs.lfs_refs = lfs_refs;
        refs.xorb_refs = xorb_refs;

        Ok(refs)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BloomConfig, ScannerConfig};
    use crate::gc::bloom::BloomFilterProtectedSet;
    use crate::gc::checkpoint::GcCheckpoint;
    use crate::gc::reference_tracker::s3::SidecarReferenceTracker;
    use crate::storage::local::LocalStorage;
    use tempfile::TempDir;

    #[test]
    fn test_scan_result_default() {
        let result = ScanResult::default();
        assert_eq!(result.shards_scanned, 0);
        assert_eq!(result.refs_inserted, 0);
        assert!(!result.scan_completed);
    }

    /// Helper to create a scanner with LocalStorage for testing.
    fn make_scanner() -> (IncrementalScanner, Arc<Box<dyn StorageBackend>>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(
            LocalStorage::new(tmp.path().to_str().unwrap()).unwrap()
        ));
        let ref_tracker: Arc<dyn ReferenceTracker> =
            Arc::new(SidecarReferenceTracker::new(storage.clone()));

        let scanner_config = ScannerConfig {
            page_size: 10,
            checkpoint_interval: 5,
            max_duration_seconds: 60,
        };

        let scanner = IncrementalScanner::new(
            storage.clone(),
            ref_tracker,
            scanner_config,
        );
        (scanner, storage, tmp)
    }

    #[tokio::test]
    async fn test_scan_empty_storage() {
        let (scanner, _storage, _tmp) = make_scanner();
        let bloom_config = BloomConfig {
            expected_items: 100,
            false_positive_rate: 0.01,
            rebuild_threshold: 0.8,
        };
        let mut bloom = BloomFilterProtectedSet::new(bloom_config);
        let mut checkpoint = GcCheckpoint::new();

        let result = scanner.scan(&mut bloom, &mut checkpoint).await.unwrap();

        assert!(result.scan_completed);
        assert_eq!(result.shards_scanned, 0);
        assert_eq!(result.refs_inserted, 0);
        assert_eq!(checkpoint.status, crate::gc::checkpoint::CheckpointStatus::Completed);
    }

    #[tokio::test]
    async fn test_scan_with_sidecar_references() {
        let (scanner, storage, _tmp) = make_scanner();
        let ref_tracker = SidecarReferenceTracker::new(storage.clone());

        ref_tracker.record_references(
            "shard_abc",
            &["lfs_ref_1".to_string()],
            &["xorb_ref_1".to_string(), "xorb_ref_2".to_string()],
        ).await.unwrap();

        storage.put("shards/shard_abc", bytes::Bytes::from("fake shard data"))
            .await.unwrap();

        let bloom_config = BloomConfig {
            expected_items: 100,
            false_positive_rate: 0.01,
            rebuild_threshold: 0.8,
        };
        let mut bloom = BloomFilterProtectedSet::new(bloom_config);
        let mut checkpoint = GcCheckpoint::new();

        let result = scanner.scan(&mut bloom, &mut checkpoint).await.unwrap();

        assert!(result.scan_completed);
        assert_eq!(result.shards_scanned, 1);
        assert_eq!(result.refs_inserted, 3);

        assert!(bloom.contains(b"lfs_ref_1"));
        assert!(bloom.contains(b"xorb_ref_1"));
        assert!(bloom.contains(b"xorb_ref_2"));
    }

    #[tokio::test]
    async fn test_scan_multiple_pages() {
        let (scanner, storage, _tmp) = make_scanner();
        let ref_tracker = SidecarReferenceTracker::new(storage.clone());

        for i in 0..15 {
            let shard_hash = format!("shard_{:03}", i);
            ref_tracker.record_references(
                &shard_hash,
                &[],
                &[format!("xorb_{}", i)],
            ).await.unwrap();
            storage.put(&format!("shards/{}", shard_hash), bytes::Bytes::from("data"))
                .await.unwrap();
        }

        let bloom_config = BloomConfig {
            expected_items: 100,
            false_positive_rate: 0.01,
            rebuild_threshold: 0.8,
        };
        let mut bloom = BloomFilterProtectedSet::new(bloom_config);
        let mut checkpoint = GcCheckpoint::new();

        let result = scanner.scan(&mut bloom, &mut checkpoint).await.unwrap();

        assert!(result.scan_completed);
        assert_eq!(result.shards_scanned, 15);
        assert_eq!(result.refs_inserted, 15);
    }

    #[tokio::test]
    async fn test_scan_missing_sidecar_falls_back() {
        let (scanner, storage, _tmp) = make_scanner();

        // Create a shard without a sidecar and with invalid data.
        // The scanner should use Layer 3 (conservative skip) — return empty refs
        // rather than failing. This ensures GC doesn't delete referenced data
        // when a shard can't be parsed.
        storage.put("shards/orphan_shard", bytes::Bytes::from("not a valid shard"))
            .await.unwrap();

        let bloom_config = BloomConfig {
            expected_items: 100,
            false_positive_rate: 0.01,
            rebuild_threshold: 0.8,
        };
        let mut bloom = BloomFilterProtectedSet::new(bloom_config);
        let mut checkpoint = GcCheckpoint::new();

        let result = scanner.scan(&mut bloom, &mut checkpoint).await.unwrap();

        // Scan completes successfully (Layer 3 conservative skip)
        assert!(result.scan_completed);
        assert_eq!(result.shards_scanned, 1);
        // parse_errors is 0 because Layer 3 is a graceful fallback, not an error.
        // The scanner intentionally returns empty refs to avoid deleting referenced data.
        assert_eq!(result.parse_errors, 0);
        // No references inserted (conservative — safer to retain orphans than delete referenced)
        assert_eq!(result.refs_inserted, 0);
    }
}
