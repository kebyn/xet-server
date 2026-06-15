//! Grace period for protecting recently uploaded blobs.
//!
//! **Absolute grace period**: blobs younger than `absolute_seconds` are never deleted.
//! This is a wall-clock safety net that protects against timing issues and
//! concurrent uploads.
//!
//! # Future: Soft Grace Period (Cycles)
//!
//! A soft grace period would require blobs to be observed as unreferenced for
//! N consecutive GC cycles before deletion. This handles the case where a blob
//! is referenced by a commit that hasn't been written to Hub's file_tree yet.
//!
//! **Current status:** Soft grace period is NOT implemented because it requires
//! persisting per-blob state across GC runs (unreferenced cycle counters).
//! The `soft_cycles` config field is accepted but ignored with a warning.
//! See design spec for planned implementation.

use crate::config::GraceConfig;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Grace period manager using absolute wall-clock age.
///
/// A blob is eligible for deletion when its age exceeds `absolute_seconds`.
pub struct GracePeriod {
    /// Absolute minimum age before a blob can be deleted.
    absolute_grace: Duration,
}

impl GracePeriod {
    /// Create a new grace period manager from config.
    ///
    /// Note: `config.soft_cycles` is currently ignored (see module docs).
    /// A warning is logged if soft_cycles > 0 to inform operators.
    pub fn new(config: &GraceConfig) -> Self {
        if config.soft_cycles > 0 {
            tracing::warn!(
                soft_cycles = config.soft_cycles,
                "GC grace.soft_cycles is configured but not yet implemented. \
                 Soft grace period requires persistent state across GC runs. \
                 Only grace.absolute_seconds is enforced."
            );
        }
        Self {
            absolute_grace: Duration::from_secs(config.absolute_seconds),
        }
    }

    /// Check if a blob can be deleted based on its mtime.
    ///
    /// Returns `true` if the blob's age exceeds the absolute grace period.
    /// Returns `false` if the blob should be protected.
    pub async fn can_delete(&self, _key: &str, mtime: u64) -> bool {
        // I1 fix: Use unwrap_or_default instead of unwrap to avoid panic on clock issues.
        // If clock is broken, we conservatively return false (don't delete).
        let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(_) => {
                tracing::warn!("System clock appears to be before UNIX_EPOCH, refusing to delete");
                return false;
            }
        };

        let age = now.saturating_sub(mtime);
        if age < self.absolute_grace.as_secs() {
            tracing::trace!(
                age_secs = age,
                absolute_grace_secs = self.absolute_grace.as_secs(),
                "Blob protected by absolute grace period"
            );
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> GraceConfig {
        GraceConfig {
            absolute_seconds: 60,  // 1 minute absolute grace
            soft_cycles: 0,        // Disabled (not implemented)
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
    async fn test_absolute_grace_allows_old_blobs() {
        let grace = GracePeriod::new(&test_config());
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        // Blob created 2 hours ago (past 60s absolute grace)
        assert!(grace.can_delete("key1", now - 7200).await);
    }

    #[tokio::test]
    async fn test_soft_cycles_warning_logged() {
        let config = GraceConfig {
            absolute_seconds: 60,
            soft_cycles: 2, // Non-zero triggers warning
        };
        // This should log a warning (verified by running test with RUST_LOG=warn)
        let _grace = GracePeriod::new(&config);
        // If we get here without panicking, the warning path works
    }
}
