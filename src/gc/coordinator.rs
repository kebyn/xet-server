//! Multi-node GC coordinator using S3-based lease management.
//!
//! When multiple CAS nodes run GC concurrently, only one node should
//! perform GC at a time to avoid race conditions. The coordinator uses
//! a lease file in storage (`.gc/lease.json`) with conditional PUT for
//! atomic acquisition.
//!
//! # Lease Lifecycle
//!
//! 1. Node reads `.gc/lease.json` (may not exist)
//! 2. If lease is expired or absent, attempt conditional PUT
//! 3. On success: node holds the lease, starts renewal task
//! 4. On failure: another node holds the lease, skip this GC cycle
//! 5. Periodically renew the lease (extend expiry)
//! 6. On GC completion or drop: release the lease

use crate::config::LeaseConfig;
use crate::gc::errors::{GcError, GcResult};
use crate::storage::StorageBackend;
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Key for the lease file in storage.
const LEASE_KEY: &str = ".gc/lease.json";

/// GC lease data stored in `.gc/lease.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcLease {
    /// Unique identifier of the node holding the lease.
    pub holder_node_id: String,
    /// When the lease expires.
    pub expires_at: DateTime<Utc>,
    /// When the lease was acquired.
    pub acquired_at: DateTime<Utc>,
    /// Current ETag of the lease file (for conditional operations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl GcLease {
    /// Check if this lease has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    /// Check if this lease is held by the given node.
    pub fn is_held_by(&self, node_id: &str) -> bool {
        self.holder_node_id == node_id
    }
}

/// RAII guard for a held GC lease.
///
/// When dropped, the lease is released (best-effort). The renewal task
/// is also cancelled on drop.
pub struct GcLeaseGuard {
    coordinator: Arc<GcCoordinator>,
    lease: GcLease,
    renewal_handle: Option<tokio::task::JoinHandle<()>>,
}

impl GcLeaseGuard {
    /// Get the current lease data.
    pub fn lease(&self) -> &GcLease {
        &self.lease
    }

    /// Cancel the renewal task (called on drop).
    fn cancel_renewal(&mut self) {
        if let Some(handle) = self.renewal_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for GcLeaseGuard {
    /// Release the lease on drop.
    ///
    /// # M5 fix: Runtime Shutdown Consideration
    ///
    /// Since `Drop` is synchronous and cannot perform async work directly,
    /// we spawn a task to release the lease. This has limitations:
    ///
    /// - **Normal drop**: The spawned task runs and releases the lease promptly.
    /// - **Runtime shutdown**: `tokio::spawn` may not execute if the runtime is
    ///   shutting down. In this case, the lease will expire naturally after
    ///   `ttl_seconds` (default: 1 hour).
    ///
    /// This is acceptable because:
    /// 1. Lease TTL is relatively short (1 hour default)
    /// 2. Other nodes will retry after the TTL expires
    /// 3. Graceful shutdown can call `release_lease()` explicitly before dropping
    ///
    /// For stronger guarantees, callers should explicitly release the lease
    /// before dropping the guard during shutdown sequences.
    fn drop(&mut self) {
        self.cancel_renewal();

        // Best-effort lease release via spawned task.
        // If runtime is shutting down, lease will expire via TTL.
        let coordinator = self.coordinator.clone();
        let node_id = self.lease.holder_node_id.clone();
        let etag = self.lease.etag.clone();

        // Use spawn_blocking as a fallback indication that we're in drop context
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // I1 fix: Pass etag for conditional delete to prevent race condition
                if let Err(e) = coordinator.release_lease(&node_id, etag.as_deref()).await {
                    tracing::warn!("Failed to release GC lease on drop: {}", e);
                }
            });
        } else {
            // Runtime is gone — lease will expire via TTL
            tracing::debug!(
                node_id = %node_id,
                "Tokio runtime unavailable during drop; lease will expire via TTL"
            );
        }
    }
}

/// Multi-node GC coordinator.
///
/// Manages lease acquisition, renewal, and release for coordinating
/// concurrent GC runs across multiple CAS nodes.
pub struct GcCoordinator {
    storage: Arc<Box<dyn StorageBackend>>,
    node_id: String,
    config: LeaseConfig,
}

impl GcCoordinator {
    /// Create a new GC coordinator.
    pub fn new(
        storage: Arc<Box<dyn StorageBackend>>,
        node_id: String,
        config: LeaseConfig,
    ) -> Self {
        Self {
            storage,
            node_id,
            config,
        }
    }

    /// Get the node ID.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Try to acquire the GC lease.
    ///
    /// Returns `Ok(Some(guard))` if the lease was acquired.
    /// Returns `Ok(None)` if another node holds a valid lease.
    /// Returns `Err` on storage errors.
    pub async fn try_acquire_lease(&self) -> GcResult<Option<GcLeaseGuard>> {
        // 1. Read existing lease (if any)
        let existing_lease = self.read_lease().await?;
        let existing_etag = self.storage.get_etag(LEASE_KEY).await
            .map_err(|e| GcError::Io(std::io::Error::other(
                format!("Failed to get lease etag: {}", e),
            )))?;

        // 2. Check if lease is still valid and held by another node
        if let Some(ref lease) = existing_lease
            && !lease.is_expired() && !lease.is_held_by(&self.node_id) {
                tracing::info!(
                    holder = %lease.holder_node_id,
                    expires_at = %lease.expires_at,
                    "GC lease held by another node, skipping this cycle"
                );
                return Ok(None);
            }

        // 3. Create new lease
        let now = Utc::now();
        let new_lease = GcLease {
            holder_node_id: self.node_id.clone(),
            expires_at: now + Duration::seconds(self.config.ttl_seconds as i64),
            acquired_at: now,
            etag: None,
        };

        let lease_json = serde_json::to_vec_pretty(&new_lease)
            .map_err(GcError::Json)?;

        // 4. Conditional PUT: write only if absent or expired (etag matches)
        let result = self.storage.put_if_absent_or_expired(
            LEASE_KEY,
            Bytes::from(lease_json),
            existing_etag.as_deref(),
        ).await;

        match result {
            Ok(new_etag) => {
                tracing::info!(
                    node_id = %self.node_id,
                    ttl_seconds = self.config.ttl_seconds,
                    "Acquired GC lease"
                );

                let mut lease = new_lease;
                lease.etag = Some(new_etag);

                // 5. Start renewal task
                let coordinator = Arc::new(GcCoordinator {
                    storage: self.storage.clone(),
                    node_id: self.node_id.clone(),
                    config: self.config.clone(),
                });

                let renewal_handle = Self::start_renewal_task(
                    coordinator.clone(),
                    lease.clone(),
                );

                Ok(Some(GcLeaseGuard {
                    coordinator,
                    lease,
                    renewal_handle: Some(renewal_handle),
                }))
            }
            Err(crate::storage::StorageError::ConditionFailed) => {
                // Another node acquired the lease between our check and PUT
                tracing::info!("GC lease condition failed (another node acquired it)");
                Ok(None)
            }
            Err(e) => {
                Err(GcError::Io(std::io::Error::other(
                    format!("Failed to acquire lease: {}", e),
                )))
            }
        }
    }

    /// Renew an existing lease by extending its expiry.
    pub async fn renew_lease(&self, lease: &mut GcLease) -> GcResult<()> {
        let now = Utc::now();
        lease.expires_at = now + Duration::seconds(self.config.ttl_seconds as i64);

        let lease_json = serde_json::to_vec_pretty(&lease)
            .map_err(GcError::Json)?;

        let result = self.storage.put_if_absent_or_expired(
            LEASE_KEY,
            Bytes::from(lease_json),
            lease.etag.as_deref(),
        ).await;

        match result {
            Ok(new_etag) => {
                lease.etag = Some(new_etag);
                tracing::debug!(
                    node_id = %self.node_id,
                    new_expires_at = %lease.expires_at,
                    "Renewed GC lease"
                );
                Ok(())
            }
            Err(crate::storage::StorageError::ConditionFailed) => {
                Err(GcError::LeaseExpired)
            }
            Err(e) => {
                Err(GcError::Io(std::io::Error::other(
                    format!("Failed to renew lease: {}", e),
                )))
            }
        }
    }

    /// Release the GC lease (only if we still hold it).
    ///
    /// I1 fix: Uses conditional delete with etag to prevent race condition
    /// where one node deletes another node's lease.
    async fn release_lease(&self, node_id: &str, expected_etag: Option<&str>) -> GcResult<()> {
        // Read current lease
        let current = match self.read_lease().await? {
            Some(lease) => lease,
            None => return Ok(()), // No lease to release
        };

        // Only release if we hold it
        if !current.is_held_by(node_id) {
            return Ok(());
        }

        // I1 fix: Use conditional delete with etag to prevent race condition.
        // If etag is provided and doesn't match, another node has acquired the lease.
        if let Some(etag) = expected_etag {
            match self.storage.delete_if_match(LEASE_KEY, etag).await {
                Ok(()) => {
                    tracing::info!(node_id = %node_id, "Released GC lease (conditional)");
                    return Ok(());
                }
                Err(crate::storage::StorageError::ConditionFailed) => {
                    // Another node has the lease — don't delete
                    tracing::debug!(
                        node_id = %node_id,
                        "Lease release skipped: etag mismatch (another node holds lease)"
                    );
                    return Ok(());
                }
                Err(crate::storage::StorageError::Internal(_)) => {
                    // Backend doesn't support conditional delete — fall through to regular delete
                    tracing::debug!(
                        "Backend doesn't support conditional delete, falling back to regular delete"
                    );
                }
                Err(e) => {
                    return Err(GcError::Io(std::io::Error::other(
                        format!("Failed to delete lease: {}", e),
                    )));
                }
            }
        }

        // Fallback: regular delete (has race condition but better than nothing)
        self.storage.delete(LEASE_KEY).await
            .map_err(|e| GcError::Io(std::io::Error::other(
                format!("Failed to delete lease: {}", e),
            )))?;

        tracing::info!(node_id = %node_id, "Released GC lease");
        Ok(())
    }

    /// Read the current lease from storage.
    async fn read_lease(&self) -> GcResult<Option<GcLease>> {
        match self.storage.get(LEASE_KEY).await {
            Ok(data) => {
                let lease: GcLease = serde_json::from_slice(&data)
                    .map_err(GcError::Json)?;
                Ok(Some(lease))
            }
            Err(crate::storage::StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(GcError::Io(std::io::Error::other(
                format!("Failed to read lease: {}", e),
            ))),
        }
    }

    /// Start a background task that periodically renews the lease.
    fn start_renewal_task(
        coordinator: Arc<GcCoordinator>,
        initial_lease: GcLease,
    ) -> tokio::task::JoinHandle<()> {
        let renew_interval = std::time::Duration::from_secs(coordinator.config.renew_interval_seconds);

        tokio::spawn(async move {
            let mut lease = initial_lease;
            let mut interval = tokio::time::interval(renew_interval);

            loop {
                interval.tick().await;

                match coordinator.renew_lease(&mut lease).await {
                    Ok(()) => {
                        tracing::debug!(
                            expires_at = %lease.expires_at,
                            "GC lease renewed"
                        );
                    }
                    Err(GcError::LeaseExpired) => {
                        tracing::error!("GC lease expired during renewal, stopping renewal task");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("GC lease renewal failed: {}, will retry", e);
                        // Continue retrying — transient errors shouldn't stop renewal
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lease_is_expired() {
        let lease = GcLease {
            holder_node_id: "node-1".to_string(),
            expires_at: Utc::now() - Duration::seconds(10), // expired 10s ago
            acquired_at: Utc::now() - Duration::seconds(3610),
            etag: None,
        };
        assert!(lease.is_expired());
    }

    #[test]
    fn test_lease_not_expired() {
        let lease = GcLease {
            holder_node_id: "node-1".to_string(),
            expires_at: Utc::now() + Duration::seconds(3600), // expires in 1h
            acquired_at: Utc::now(),
            etag: None,
        };
        assert!(!lease.is_expired());
    }

    #[test]
    fn test_lease_is_held_by() {
        let lease = GcLease {
            holder_node_id: "node-1".to_string(),
            expires_at: Utc::now() + Duration::seconds(3600),
            acquired_at: Utc::now(),
            etag: None,
        };
        assert!(lease.is_held_by("node-1"));
        assert!(!lease.is_held_by("node-2"));
    }

    #[test]
    fn test_lease_json_roundtrip() {
        let lease = GcLease {
            holder_node_id: "node-abc".to_string(),
            expires_at: Utc::now() + Duration::seconds(3600),
            acquired_at: Utc::now(),
            etag: Some("\"12345\"".to_string()),
        };

        let json = serde_json::to_vec(&lease).unwrap();
        let loaded: GcLease = serde_json::from_slice(&json).unwrap();

        assert_eq!(loaded.holder_node_id, "node-abc");
        assert_eq!(loaded.etag, Some("\"12345\"".to_string()));
    }
}
