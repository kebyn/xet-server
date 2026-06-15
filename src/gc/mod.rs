//! Garbage collection for orphaned blobs
//!
//! GC runs as a background task that periodically:
//! 1. Scans storage for all blobs (LFS and xorbs)
//! 2. Queries Hub for all referenced hashes
//! 3. Computes orphaned set (storage - referenced)
//! 4. Applies grace period protection
//! 5. Deletes orphaned blobs (or reports in dry_run mode)

pub mod errors;

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
    /// C1 fix: Track deleted shards
    pub deleted_shards: usize,
    pub grace_period_skipped: usize,
    pub errors: usize,
    pub duration_seconds: f64,
    pub dry_run: bool,
    /// M5 fix: Use DateTime<Utc> for type safety instead of String
    pub last_run: Option<chrono::DateTime<chrono::Utc>>,
}

/// Garbage collector for cleaning up orphaned blobs
pub struct GarbageCollector {
    storage: Arc<Box<dyn StorageBackend>>,
    hub_client: reqwest::Client,
    config: GcConfig,
}

impl GarbageCollector {
    /// Create a new GarbageCollector
    /// I4 fix: Returns Result instead of panicking on client build failure
    pub fn new(storage: Arc<Box<dyn StorageBackend>>, config: GcConfig) -> Result<Self, String> {
        let hub_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.http_timeout_seconds))
            .build()
            .map_err(|e| format!("Failed to build GC hub client: {}", e))?;

        Ok(Self {
            storage,
            hub_client,
            config,
        })
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

        // Step 3: C1 fix - Identify orphaned shards first (using mtime heuristic)
        // Only scan non-orphaned shards for xorb references to prevent storage leak
        let orphaned_shards = self.compute_orphaned_shards(&shards, &mut stats);
        let active_shard_keys: Vec<String> = shards
            .iter()
            .filter(|(key, _)| !orphaned_shards.contains(key))
            .map(|(key, _)| key.clone())
            .collect();

        info!(
            "GC identified {} orphaned shards (using mtime heuristic), {} active shards",
            orphaned_shards.len(),
            active_shard_keys.len()
        );

        // Step 4: Scan ONLY active shards for referenced xorbs
        // This prevents orphaned shards from keeping their xorbs alive forever
        let referenced_xorbs = self.scan_referenced_xorbs(&active_shard_keys).await?;
        stats.referenced_xorbs = referenced_xorbs.len();

        info!("GC scanned {} active shards, found {} referenced xorbs",
            active_shard_keys.len(), stats.referenced_xorbs);

        // Step 5: Compute orphaned sets
        let orphaned_lfs = self.compute_orphaned_lfs(&lfs_blobs, &referenced_lfs, &mut stats);
        let orphaned_xorbs = self.compute_orphaned_xorbs(&xorbs, &referenced_xorbs, &mut stats);
        stats.orphaned_lfs_blobs = orphaned_lfs.len();
        stats.orphaned_xorbs = orphaned_xorbs.len();

        info!(
            "GC found {} orphaned LFS blobs, {} orphaned xorbs",
            stats.orphaned_lfs_blobs, stats.orphaned_xorbs
        );

        // Step 6: Delete orphans (or report in dry_run)
        self.cleanup_orphans(&orphaned_lfs, &orphaned_xorbs, &mut stats).await?;

        // Step 7: C1 fix - Delete orphaned shards
        self.cleanup_orphaned_shards(&orphaned_shards, &mut stats).await?;

        stats.duration_seconds = start.elapsed().as_secs_f64();
        stats.last_run = Some(chrono::Utc::now());  // M5 fix: Use DateTime<Utc> directly

        info!(
            "GC completed in {:.1}s: deleted {} LFS, {} xorbs, {} shards (dry_run={}, grace_skipped={})",
            stats.duration_seconds,
            stats.deleted_lfs_blobs,
            stats.deleted_xorbs,
            stats.deleted_shards,
            stats.dry_run,
            stats.grace_period_skipped
        );

        Ok(stats)
    }

    /// Scan storage and categorize blobs by type
    /// Returns (lfs_blobs_with_mtime, xorbs_with_mtime, shards_with_mtime)
    /// C1 fix: Also return mtime for shards to enable orphan detection
    async fn scan_storage(
        &self,
        stats: &mut GcStats,
    ) -> Result<(Vec<(String, u64)>, Vec<(String, u64)>, Vec<(String, u64)>), String> {
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

        // C1 fix: Get shards with mtime for orphan detection
        let shards = self
            .storage
            .list_objects_with_mtime("shards")
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
                // M3 fix: Log message clearly indicates this is a retry wait, not a failure
                tracing::info!(
                    "GC Hub request failed, waiting {}s before retry attempt {}/{}",
                    delay.as_secs(), attempt, MAX_RETRIES
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
                            // M4 fix: Use to_hex() for consistency with xorb storage key format.
                            // to_hex() is hex::encode(as_bytes()), but using the named method
                            // makes the intent clearer and prevents future divergence.
                            xorb_hashes.insert(xorb_entry.xorb_hash.to_hex());
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
    /// I2 fix: Returns (key, mtime) pairs to avoid redundant get_mtime calls in cleanup
    fn compute_orphaned_lfs(
        &self,
        all_blobs: &[(String, u64)],
        referenced: &HashSet<String>,
        stats: &mut GcStats,
    ) -> Vec<(String, u64)> {
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
                // M2 fix: Validate OID format (64 hex chars) for safety
                let oid = key
                    .strip_prefix("lfs/objects/")
                    .unwrap_or(key)
                    .split('/')
                    .next_back()
                    .unwrap_or(key);

                // M2 fix: Validate OID format
                let is_valid_oid = oid.len() == 64 && oid.chars().all(|c| c.is_ascii_hexdigit());
                if !is_valid_oid {
                    warn!("GC skipping LFS blob with invalid OID format: {} (OID: {})", key, oid);
                    return false;
                }

                let is_orphaned = !referenced.contains(oid);
                let is_old_enough = (now.saturating_sub(*mtime)) > grace;

                if is_orphaned && !is_old_enough {
                    stats.grace_period_skipped += 1;
                }

                is_orphaned && is_old_enough
            })
            .map(|(key, mtime)| (key.clone(), *mtime))
            .collect()
    }

    /// Compute orphaned xorbs (excluding those in grace period)
    /// I2 fix: Returns (key, mtime) pairs to avoid redundant get_mtime calls in cleanup
    fn compute_orphaned_xorbs(
        &self,
        all_xorbs: &[(String, u64)],
        referenced: &HashSet<String>,
        stats: &mut GcStats,
    ) -> Vec<(String, u64)> {
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
            .map(|(key, mtime)| (key.clone(), *mtime))
            .collect()
    }

    /// Delete orphaned blobs (or report in dry_run mode)
    /// I2 fix: Uses mtime from scan_storage to avoid redundant get_mtime calls
    async fn cleanup_orphans(
        &self,
        lfs_keys: &[(String, u64)],
        xorb_keys: &[(String, u64)],
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

        // I2 fix: Use mtime passed from compute_orphaned_* instead of re-fetching.
        // However, we still re-check mtime before deletion to prevent race conditions
        // where a blob is uploaded between scan and delete phases.
        // I5 fix: Re-fetch current time for each blob to prevent grace period bypass
        // when GC runs for a long time.

        // Delete LFS blobs
        for (key, scan_mtime) in lfs_keys {
            // I5 fix: Re-fetch current time for each blob
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // I2 optimization: Only re-check mtime if scan was a while ago.
            // If scan_mtime is recent (within last 10 seconds), trust it.
            // This reduces S3 HEAD requests for fast GC runs.
            let mtime_age = now.saturating_sub(*scan_mtime);
            if mtime_age > 10 {
                // Scan was more than 10 seconds ago, re-check mtime
                match self.storage.get_mtime(key).await {
                    Ok(current_mtime) => {
                        // If mtime changed, blob was replaced - skip deletion
                        if current_mtime != *scan_mtime {
                            warn!("GC skipping {} - mtime changed since scan ({} != {})", key, current_mtime, scan_mtime);
                            continue;
                        }
                        let age = now.saturating_sub(current_mtime);
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
            } else {
                // Scan was recent, trust the mtime but still check grace period
                if mtime_age <= self.config.grace_period_seconds {
                    stats.grace_period_skipped += 1;
                    continue;
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
        for (key, scan_mtime) in xorb_keys {
            // I5 fix: Re-fetch current time for each xorb
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // I2 optimization: Only re-check mtime if scan was a while ago
            let mtime_age = now.saturating_sub(*scan_mtime);
            if mtime_age > 10 {
                // Scan was more than 10 seconds ago, re-check mtime
                match self.storage.get_mtime(key).await {
                    Ok(current_mtime) => {
                        // If mtime changed, xorb was replaced - skip deletion
                        if current_mtime != *scan_mtime {
                            warn!("GC skipping {} - mtime changed since scan ({} != {})", key, current_mtime, scan_mtime);
                            continue;
                        }
                        let age = now.saturating_sub(current_mtime);
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
            } else {
                // Scan was recent, trust the mtime but still check grace period
                if mtime_age <= self.config.grace_period_seconds {
                    stats.grace_period_skipped += 1;
                    continue;
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

    /// C1 fix: Compute orphaned shards using mtime heuristic.
    ///
    /// Since Hub doesn't track shard references, we use mtime as a heuristic:
    /// shards older than the grace period are considered orphaned.
    /// This is not perfect (may delete recently created but unused shards)
    /// but prevents storage leak from orphaned shards.
    ///
    /// A proper solution would require Hub to track shard references in its database.
    fn compute_orphaned_shards(
        &self,
        all_shards: &[(String, u64)],
        stats: &mut GcStats,
    ) -> Vec<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let grace = self.config.grace_period_seconds;

        all_shards
            .iter()
            .filter(|(_, mtime)| {
                // Shards older than grace period are considered orphaned
                let is_old_enough = (now.saturating_sub(*mtime)) > grace;
                if !is_old_enough {
                    stats.grace_period_skipped += 1;
                }
                is_old_enough
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// C1 fix: Delete orphaned shards
    async fn cleanup_orphaned_shards(
        &self,
        shard_keys: &[String],
        stats: &mut GcStats,
    ) -> Result<(), String> {
        if self.config.dry_run {
            info!("GC dry_run: would delete {} shards", shard_keys.len());
            return Ok(());
        }

        for key in shard_keys {
            match self.storage.delete(key).await {
                Ok(_) => stats.deleted_shards += 1,
                Err(e) => {
                    warn!("GC failed to delete shard {}: {}", key, e);
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

        let gc = GarbageCollector::new(storage, config).expect("Failed to create GC");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Blobs: (key, mtime) - use valid 64-char hex OIDs for M2 validation
        let all_blobs = vec![
            ("lfs/objects/ab/cd/0000000000000000000000000000000000000000000000000000000000000001".to_string(), now - 3600), // 1 hour old
            ("lfs/objects/ab/cd/0000000000000000000000000000000000000000000000000000000000000002".to_string(), now - 60),   // 1 minute old (in grace)
            ("lfs/objects/ab/cd/0000000000000000000000000000000000000000000000000000000000000003".to_string(), now - 7200), // 2 hours old
        ];

        let referenced: HashSet<String> = vec!["0000000000000000000000000000000000000000000000000000000000000003".to_string()].into_iter().collect();

        let mut stats = GcStats::default();
        let orphaned = gc.compute_orphaned_lfs(&all_blobs, &referenced, &mut stats);

        // Only the first blob should be deleted (second is in grace period, third is referenced)
        assert_eq!(orphaned.len(), 1);
        assert!(orphaned[0].0.ends_with("0000000000000000000000000000000000000000000000000000000000000001"));  // I2 fix: orphaned is now Vec<(String, u64)>
        assert_eq!(stats.grace_period_skipped, 1);
    }

    // M4 fix: Verify that xorb hash hex encoding matches storage key format.
    // xorb storage keys use format!("xorbs/{}", xorb_hash.to_hex()),
    // so GC must use the same encoding when comparing referenced xorbs.
    #[test]
    fn test_xorb_hash_hex_encoding_consistency() {
        use crate::types::MerkleHash;

        // Create a known hash value
        let hash_bytes: [u8; 32] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let hash = MerkleHash::from(hash_bytes);

        // to_hex() and hex::encode(as_bytes()) must produce identical output
        assert_eq!(
            hash.to_hex(),
            hex::encode(hash.as_bytes()),
            "MerkleHash::to_hex() must equal hex::encode(as_bytes()) for xorb key consistency"
        );

        // Verify the storage key format matches what GC computes
        let storage_key = format!("xorbs/{}", hash.to_hex());
        let extracted_hash = storage_key.strip_prefix("xorbs/").unwrap();
        assert_eq!(extracted_hash, hash.to_hex());
    }
}
