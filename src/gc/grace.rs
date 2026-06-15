//! Two-tier grace period for protecting recently uploaded blobs.
//!
//! Two complementary mechanisms prevent premature deletion:
//!
//! 1. **Absolute grace period**: blobs younger than `absolute_seconds` are never deleted.
//!    This is a wall-clock safety net that protects against timing issues.
//!
//! 2. **Soft grace period (cycles)**: blobs must be observed as unreferenced for
//!    `soft_cycles` consecutive GC cycles before becoming eligible for deletion.
//!    This handles the case where a blob is referenced by a commit that hasn't
//!    been written to Hub's file_tree yet.
//!
//! # Combined Behavior
//!
//! A blob is eligible for deletion only when BOTH conditions are met:
//! - Age > `absolute_seconds` (wall clock)
//! - Observed unreferenced for >= `soft_cycles` consecutive cycles

use crate::config::GraceConfig;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

/// Two-tier grace period manager.
///
/// Tracks how many consecutive GC cycles each blob has been observed as
/// unreferenced, and combines this with wall-clock age to determine
/// deletion eligibility.
pub struct GracePeriod {
    /// Absolute minimum age before a blob can be deleted.
    absolute_grace: Duration,
    /// Number of consecutive unreferenced cycles before deletion.
    soft_grace_cycles: u32,
    /// Tracks consecutive unreferenced count per blob key.
    /// Key: storage object key, Value: number of consecutive unreferenced cycles.
    unreferenced_tracker: RwLock<HashMap<String, u32>>,
}

impl GracePeriod {
    /// Create a new grace period manager from config.
    pub fn new(config: &GraceConfig) -> Self {
        Self {
            absolute_grace: Duration::from_secs(config.absolute_seconds),
            soft_grace_cycles: config.soft_cycles,
            unreferenced_tracker: RwLock::new(HashMap::new()),
        }
    }

    /// Check if a blob can be deleted based on its mtime and reference status.
    ///
    /// This method:
    /// 1. Checks absolute grace period (wall clock age)
    /// 2. Increments the unreferenced counter for this blob
    /// 3. Checks soft grace period (consecutive cycles)
    ///
    /// Returns `true` if the blob can be deleted (both grace periods satisfied).
    /// Returns `false` if the blob should be protected.
    pub async fn can_delete(&self, key: &str, mtime: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // 1. Absolute grace period check
        let age = now.saturating_sub(mtime);
        if age < self.absolute_grace.as_secs() {
            tracing::trace!(
                key = %key,
                age_secs = age,
                absolute_grace_secs = self.absolute_grace.as_secs(),
                "Blob protected by absolute grace period"
            );
            return false;
        }

        // 2. Soft grace period check — increment unreferenced counter
        let mut tracker = self.unreferenced_tracker.write().await;
        let count = tracker.entry(key.to_string()).or_insert(0);
        *count += 1;

        if *count < self.soft_grace_cycles {
            tracing::trace!(
                key = %key,
                unreferenced_cycles = *count,
                required_cycles = self.soft_grace_cycles,
                "Blob protected by soft grace period"
            );
            return false;
        }

        // Both grace periods satisfied — eligible for deletion
        // Reset the counter (blob will be deleted or re-encountered next cycle)
        tracker.remove(key);
        true
    }

    /// Record that a blob was observed as referenced in this cycle.
    ///
    /// Resets the unreferenced counter for this blob to zero.
    /// Call this for every blob that IS referenced during the scan.
    pub async fn record_referenced(&self, key: &str) {
        let mut tracker = self.unreferenced_tracker.write().await;
        tracker.remove(key);
    }

    /// Record that a blob was observed as unreferenced in this cycle.
    ///
    /// Increments the unreferenced counter. Call this for blobs that
    /// are NOT in the bloom filter protected set.
    pub async fn record_unreferenced(&self, key: &str) {
        let mut tracker = self.unreferenced_tracker.write().await;
        let count = tracker.entry(key.to_string()).or_insert(0);
        *count += 1;
    }

    /// Clear all tracking state (call at the start of each GC cycle).
    ///
    /// Note: This is NOT called automatically — the caller decides when
    /// to clear state. In the standard GC flow, state persists across
    /// cycles to accumulate the soft grace counter.
    pub async fn clear(&self) {
        self.unreferenced_tracker.write().await.clear();
    }

    /// Get the current unreferenced count for a key (for testing/monitoring).
    pub async fn unreferenced_count(&self, key: &str) -> u32 {
        self.unreferenced_tracker.read().await.get(key).copied().unwrap_or(0)
    }

    /// Get the total number of keys being tracked.
    pub async fn tracked_count(&self) -> usize {
        self.unreferenced_tracker.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> GraceConfig {
        GraceConfig {
            absolute_seconds: 60,  // 1 minute absolute grace
            soft_cycles: 2,         // Must be unreferenced for 2 cycles
        }
    }

    #[tokio::test]
    async fn test_absolute_grace_blocks_deletion() {
        let grace = GracePeriod::new(&test_config());
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        // Blob created 30 seconds ago (within 60s absolute grace)
        assert!(!grace.can_delete("key1", now - 30).await);
    }

    #[tokio::test]
    async fn test_soft_grace_requires_multiple_cycles() {
        let grace = GracePeriod::new(&test_config());
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        // Blob created 2 hours ago (past absolute grace)
        let old_mtime = now - 7200;

        // First cycle: should be blocked by soft grace
        assert!(!grace.can_delete("key1", old_mtime).await);
        assert_eq!(grace.unreferenced_count("key1").await, 1);

        // Second cycle: soft grace satisfied
        assert!(grace.can_delete("key1", old_mtime).await);
    }

    #[tokio::test]
    async fn test_referenced_resets_counter() {
        let grace = GracePeriod::new(&test_config());

        grace.record_unreferenced("key1").await;
        assert_eq!(grace.unreferenced_count("key1").await, 1);

        grace.record_referenced("key1").await;
        assert_eq!(grace.unreferenced_count("key1").await, 0);
    }

    #[tokio::test]
    async fn test_clear_resets_all() {
        let grace = GracePeriod::new(&test_config());

        grace.record_unreferenced("key1").await;
        grace.record_unreferenced("key2").await;
        assert_eq!(grace.tracked_count().await, 2);

        grace.clear().await;
        assert_eq!(grace.tracked_count().await, 0);
    }
}
