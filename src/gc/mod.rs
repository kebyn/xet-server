//! Garbage collection for orphaned blobs
//!
//! GC runs as a background task that periodically:
//! 1. Scans storage for all blobs (LFS and xorbs)
//! 2. Queries Hub for all referenced hashes
//! 3. Computes orphaned set (storage - referenced)
//! 4. Applies grace period protection
//! 5. Deletes orphaned blobs (or reports in dry_run mode)

use crate::config::GcConfig;
use crate::storage::StorageBackend;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Statistics from a GC run
#[derive(Debug, Clone, Default)]
pub struct GcStats {
    pub total_lfs_blobs: usize,
    pub total_xorbs: usize,
    pub total_shards: usize,
    pub referenced_lfs_blobs: usize,
    pub referenced_xorbs: usize,
    pub orphaned_lfs_blobs: usize,
    pub orphaned_xorbs: usize,
    pub deleted_lfs_blobs: usize,
    pub deleted_xorbs: usize,
    pub grace_period_skipped: usize,
    pub errors: usize,
    pub duration_seconds: f64,
    pub dry_run: bool,
    pub last_run: Option<String>,
}

/// Garbage collector for cleaning up orphaned blobs
pub struct GarbageCollector {
    storage: Arc<Box<dyn StorageBackend>>,
    hub_client: reqwest::Client,
    config: GcConfig,
}

impl GarbageCollector {
    /// Create a new GarbageCollector
    pub fn new(storage: Arc<Box<dyn StorageBackend>>, config: GcConfig) -> Self {
        let hub_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.http_timeout_seconds))
            .build()
            .expect("Failed to build GC hub client");

        Self {
            storage,
            hub_client,
            config,
        }
    }

    /// Get GC configuration
    pub fn config(&self) -> &GcConfig {
        &self.config
    }

    /// Run a full GC cycle
    pub async fn run(&self) -> Result<GcStats, String> {
        let start = Instant::now();
        let mut stats = GcStats {
            dry_run: self.config.dry_run,
            ..Default::default()
        };

        info!("GC started (dry_run={})", self.config.dry_run);

        // Step 1: Scan storage for all blobs
        let (lfs_blobs, xorbs, shards) = self.scan_storage(&mut stats).await?;
        stats.total_lfs_blobs = lfs_blobs.len();
        stats.total_xorbs = xorbs.len();
        stats.total_shards = shards.len();

        info!(
            "GC scanned storage: {} LFS blobs, {} xorbs, {} shards",
            stats.total_lfs_blobs, stats.total_xorbs, stats.total_shards
        );

        // Step 2: Query Hub for referenced hashes
        let referenced_lfs = self.fetch_referenced_hashes().await?;
        stats.referenced_lfs_blobs = referenced_lfs.len();

        info!("GC fetched {} referenced LFS hashes from Hub", stats.referenced_lfs_blobs);

        // Step 3: Scan shards for referenced xorbs
        let referenced_xorbs = self.scan_referenced_xorbs(&shards).await?;
        stats.referenced_xorbs = referenced_xorbs.len();

        info!("GC scanned shards, found {} referenced xorbs", stats.referenced_xorbs);

        // Step 4: Compute orphaned sets
        let orphaned_lfs = self.compute_orphaned_lfs(&lfs_blobs, &referenced_lfs, &mut stats);
        let orphaned_xorbs = self.compute_orphaned_xorbs(&xorbs, &referenced_xorbs, &mut stats);
        stats.orphaned_lfs_blobs = orphaned_lfs.len();
        stats.orphaned_xorbs = orphaned_xorbs.len();

        info!(
            "GC found {} orphaned LFS blobs, {} orphaned xorbs",
            stats.orphaned_lfs_blobs, stats.orphaned_xorbs
        );

        // Step 5: Delete orphans (or report in dry_run)
        self.cleanup_orphans(&orphaned_lfs, &orphaned_xorbs, &mut stats).await?;

        stats.duration_seconds = start.elapsed().as_secs_f64();
        stats.last_run = Some(chrono::Utc::now().to_rfc3339());

        info!(
            "GC completed in {:.1}s: deleted {} LFS, {} xorbs (dry_run={}, grace_skipped={})",
            stats.duration_seconds,
            stats.deleted_lfs_blobs,
            stats.deleted_xorbs,
            stats.dry_run,
            stats.grace_period_skipped
        );

        Ok(stats)
    }

    /// Scan storage and categorize blobs by type
    /// Returns (lfs_blobs_with_mtime, xorbs_with_mtime, shard_keys)
    async fn scan_storage(
        &self,
        stats: &mut GcStats,
    ) -> Result<(Vec<(String, u64)>, Vec<(String, u64)>, Vec<String>), String> {
        let lfs_blobs = self
            .storage
            .list_objects_with_mtime("lfs/objects")
            .await
            .map_err(|e| format!("Failed to list LFS blobs: {}", e))?;

        let xorbs = self
            .storage
            .list_objects_with_mtime("xorbs")
            .await
            .map_err(|e| format!("Failed to list xorbs: {}", e))?;

        let shards = self
            .storage
            .list_objects("shards")
            .await
            .map_err(|e| format!("Failed to list shards: {}", e))?;

        // Count for stats (these are set later after computing orphans)
        let _ = stats; // stats updated by caller

        Ok((lfs_blobs, xorbs, shards))
    }

    /// Fetch all referenced LFS hashes from Hub
    /// M3 fix: Implements exponential backoff retry to handle transient network errors.
    /// Retries up to 3 times with delays: 1s, 2s, 4s.
    async fn fetch_referenced_hashes(&self) -> Result<HashSet<String>, String> {
        let url = format!("{}/internal/referenced-hashes", self.config.hub_base_url);

        const MAX_RETRIES: u32 = 3;
        let mut last_error = String::new();

        for attempt in 0..=MAX_RETRIES {
            // Exponential backoff: 1s, 2s, 4s (skip delay on first attempt)
            if attempt > 0 {
                let delay = Duration::from_secs(1 << (attempt - 1));
                tracing::info!(
                    "GC Hub request retry attempt {}/{} after {}s delay",
                    attempt, MAX_RETRIES, delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }

            let resp = match self
                .hub_client
                .get(&url)
                .header("Authorization", format!("Bearer {}", self.config.hub_internal_token))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_error = format!("Failed to query Hub: {}", e);
                    tracing::warn!("GC Hub request failed (attempt {}/{}): {}", attempt + 1, MAX_RETRIES + 1, last_error);
                    continue;
                }
            };

            if !resp.status().is_success() {
                last_error = format!("Hub returned status {}", resp.status());
                tracing::warn!("GC Hub request failed (attempt {}/{}): {}", attempt + 1, MAX_RETRIES + 1, last_error);
                // Don't retry on 4xx client errors (except 408, 429)
                let status = resp.status();
                if status.is_client_error() && status.as_u16() != 408 && status.as_u16() != 429 {
                    return Err(last_error);
                }
                continue;
            }

            let body: serde_json::Value = match resp.json().await {
                Ok(b) => b,
                Err(e) => {
                    last_error = format!("Failed to parse Hub response: {}", e);
                    tracing::warn!("GC Hub request failed (attempt {}/{}): {}", attempt + 1, MAX_RETRIES + 1, last_error);
                    continue;
                }
            };

            let hashes_array = body["hashes"]
                .as_array()
                .ok_or_else(|| "Hub response missing 'hashes' array".to_string())?;

            let hashes: HashSet<String> = hashes_array
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();

            // Success - log retry if this wasn't the first attempt
            if attempt > 0 {
                tracing::info!("GC Hub request succeeded after {} retries", attempt);
            }

            return Ok(hashes);
        }

        // All retries exhausted
        Err(format!(
            "GC Hub request failed after {} attempts: {}",
            MAX_RETRIES + 1,
            last_error
        ))
    }

    /// Scan all shards and collect referenced xorb hashes
    /// I3 fix: Use concurrent shard fetching with bounded parallelism to reduce total GC time.
    /// Processes shards in batches to avoid overwhelming the storage backend.
    async fn scan_referenced_xorbs(&self, shard_keys: &[String]) -> Result<HashSet<String>, String> {
        let mut referenced = HashSet::new();

        // I3: Process shards concurrently with bounded parallelism
        // Using chunks of 10 to balance parallelism with resource usage
        const BATCH_SIZE: usize = 10;

        for chunk in shard_keys.chunks(BATCH_SIZE) {
            let mut handles = vec![];

            for shard_key in chunk {
                let storage = self.storage.clone();
                let key = shard_key.clone();

                let handle = tokio::spawn(async move {
                    let data = storage.get(&key).await
                        .map_err(|e| format!("Failed to read shard {}: {}", key, e))?;

                    // Parse shard to extract xorb references
                    // For now, we use a simplified approach - in production, parse the shard properly
                    // using MDBShardFile::parse() and iterate xorb_entries
                    let mut xorb_hashes = HashSet::new();
                    if let Ok(shard) = crate::format::shard::MDBShardFile::parse(&data) {
                        for xorb_entry in &shard.xorb_entries {
                            xorb_hashes.insert(hex::encode(xorb_entry.xorb_hash.as_bytes()));
                        }
                    } else {
                        warn!("Failed to parse shard {}, skipping xorb extraction", key);
                    }

                    Ok::<HashSet<String>, String>(xorb_hashes)
                });

                handles.push(handle);
            }

            // Wait for all tasks in this batch to complete
            for handle in handles {
                match handle.await {
                    Ok(Ok(xorb_hashes)) => {
                        referenced.extend(xorb_hashes);
                    }
                    Ok(Err(e)) => {
                        return Err(e);
                    }
                    Err(e) => {
                        return Err(format!("Task join error: {}", e));
                    }
                }
            }
        }

        Ok(referenced)
    }

    /// Compute orphaned LFS blobs (excluding those in grace period)
    fn compute_orphaned_lfs(
        &self,
        all_blobs: &[(String, u64)],
        referenced: &HashSet<String>,
        stats: &mut GcStats,
    ) -> Vec<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let grace = self.config.grace_period_seconds;

        all_blobs
            .iter()
            .filter(|(key, mtime)| {
                // Extract OID from key
                // Supports both formats: "lfs/objects/{oid}" and "lfs/objects/{prefix}/{oid}"
                let oid = key
                    .strip_prefix("lfs/objects/")
                    .unwrap_or(key)
                    .split('/')
                    .next_back()
                    .unwrap_or(key);

                let is_orphaned = !referenced.contains(oid);
                let is_old_enough = (now.saturating_sub(*mtime)) > grace;

                if is_orphaned && !is_old_enough {
                    stats.grace_period_skipped += 1;
                }

                is_orphaned && is_old_enough
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Compute orphaned xorbs (excluding those in grace period)
    fn compute_orphaned_xorbs(
        &self,
        all_xorbs: &[(String, u64)],
        referenced: &HashSet<String>,
        stats: &mut GcStats,
    ) -> Vec<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let grace = self.config.grace_period_seconds;

        all_xorbs
            .iter()
            .filter(|(key, mtime)| {
                // Extract xorb hash from key (e.g., "xorbs/{hash}" -> "{hash}")
                let xorb_hash = key.strip_prefix("xorbs/").unwrap_or(key);

                let is_orphaned = !referenced.contains(xorb_hash);
                let is_old_enough = (now.saturating_sub(*mtime)) > grace;

                if is_orphaned && !is_old_enough {
                    stats.grace_period_skipped += 1;
                }

                is_orphaned && is_old_enough
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Delete orphaned blobs (or report in dry_run mode)
    async fn cleanup_orphans(
        &self,
        lfs_keys: &[String],
        xorb_keys: &[String],
        stats: &mut GcStats,
    ) -> Result<(), String> {
        if self.config.dry_run {
            stats.deleted_lfs_blobs = lfs_keys.len();
            stats.deleted_xorbs = xorb_keys.len();
            info!(
                "GC dry_run: would delete {} LFS blobs, {} xorbs",
                lfs_keys.len(),
                xorb_keys.len()
            );
            return Ok(());
        }

        // I5 fix: Do NOT capture `now` once outside the loop.
        // Re-fetch current time for each blob to prevent grace period bypass
        // when GC runs for a long time (many blobs to delete).
        // Previously, if GC took longer than grace_period_seconds, newly uploaded
        // blobs could be incorrectly deleted because their `age` was computed against
        // a stale `now` from before the deletion loop started.

        // Delete LFS blobs
        for key in lfs_keys {
            // I5 fix: Re-fetch current time for each blob
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Re-check mtime before deletion to prevent race condition
            // where a blob is uploaded between scan and delete phases
            match self.storage.get_mtime(key).await {
                Ok(mtime) => {
                    let age = now.saturating_sub(mtime);
                    if age <= self.config.grace_period_seconds {
                        // Blob was recently uploaded, skip deletion
                        stats.grace_period_skipped += 1;
                        continue;
                    }
                }
                Err(crate::storage::StorageError::NotFound(_)) => {
                    // Blob already deleted, skip
                    continue;
                }
                Err(e) => {
                    warn!("GC failed to check mtime for {}: {}", key, e);
                    // Proceed with deletion attempt
                }
            }

            match self.storage.delete(key).await {
                Ok(_) => stats.deleted_lfs_blobs += 1,
                Err(e) => {
                    warn!("GC failed to delete LFS blob {}: {}", key, e);
                    stats.errors += 1;
                }
            }
        }

        // Delete xorbs
        for key in xorb_keys {
            // I5 fix: Re-fetch current time for each xorb
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Re-check mtime before deletion to prevent race condition
            match self.storage.get_mtime(key).await {
                Ok(mtime) => {
                    let age = now.saturating_sub(mtime);
                    if age <= self.config.grace_period_seconds {
                        // Xorb was recently uploaded, skip deletion
                        stats.grace_period_skipped += 1;
                        continue;
                    }
                }
                Err(crate::storage::StorageError::NotFound(_)) => {
                    // Xorb already deleted, skip
                    continue;
                }
                Err(e) => {
                    warn!("GC failed to check mtime for {}: {}", key, e);
                    // Proceed with deletion attempt
                }
            }

            match self.storage.delete(key).await {
                Ok(_) => stats.deleted_xorbs += 1,
                Err(e) => {
                    warn!("GC failed to delete xorb {}: {}", key, e);
                    stats.errors += 1;
                }
            }
        }

        Ok(())
    }
}

/// Start the background GC task
pub async fn start_gc_background_task(
    gc: Arc<GarbageCollector>,
    last_stats: Arc<RwLock<Option<GcStats>>>,
) {
    if !gc.config.enabled {
        info!("GC background task disabled");
        return;
    }

    let interval = Duration::from_secs(gc.config.interval_seconds);

    info!(
        "Starting GC background task (interval={}s, dry_run={})",
        gc.config.interval_seconds, gc.config.dry_run
    );

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;

            match gc.run().await {
                Ok(stats) => {
                    *last_stats.write().await = Some(stats);
                }
                Err(e) => {
                    error!("GC background task failed: {}", e);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_stats_default() {
        let stats = GcStats::default();
        assert_eq!(stats.total_lfs_blobs, 0);
        assert!(!stats.dry_run);
    }

    #[test]
    fn test_compute_orphaned_filters_grace_period() {
        let config = GcConfig {
            grace_period_seconds: 600, // 10 minutes
            ..Default::default()
        };

        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(
            crate::storage::local::LocalStorage::new("/tmp/test-gc").unwrap()
        ));

        let gc = GarbageCollector::new(storage, config);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Blobs: (key, mtime)
        let all_blobs = vec![
            ("lfs/objects/ab/cd/old_orphan".to_string(), now - 3600), // 1 hour old
            ("lfs/objects/ab/cd/new_orphan".to_string(), now - 60),   // 1 minute old (in grace)
            ("lfs/objects/ab/cd/referenced".to_string(), now - 7200), // 2 hours old
        ];

        let referenced: HashSet<String> = vec!["referenced".to_string()].into_iter().collect();

        let mut stats = GcStats::default();
        let orphaned = gc.compute_orphaned_lfs(&all_blobs, &referenced, &mut stats);

        // Only old_orphan should be deleted (new_orphan is in grace period)
        assert_eq!(orphaned.len(), 1);
        assert!(orphaned[0].contains("old_orphan"));
        assert_eq!(stats.grace_period_skipped, 1);
    }
}
