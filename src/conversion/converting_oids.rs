use std::collections::HashSet;

use parking_lot::RwLock;

/// Tracks OIDs currently being converted (prevents duplicate concurrent conversions).
/// In-memory only — resets on restart (acceptable: reconversion is idempotent).
///
/// Uses a `parking_lot::RwLock` (like the metadata index and readiness state):
/// parking_lot locks are not poisoned, so the former std-lock poison-recovery
/// boilerplate is unnecessary.
pub struct ConvertingOids {
    inner: RwLock<HashSet<String>>,
}

impl ConvertingOids {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashSet::new()),
        }
    }

    /// Try to mark an OID as converting. Returns true if successfully marked
    /// (not already being converted by another task).
    pub fn try_acquire(&self, oid: &str) -> bool {
        self.inner.write().insert(oid.to_string())
    }

    /// Release the conversion lock for an OID.
    pub fn release(&self, oid: &str) {
        self.inner.write().remove(oid);
    }
}

impl Default for ConvertingOids {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_converting_oids_acquire_release() {
        let oids = ConvertingOids::new();

        // First acquire should succeed
        assert!(oids.try_acquire("abc123"));

        // Second acquire of same OID should fail (already converting)
        assert!(!oids.try_acquire("abc123"));

        // Different OID should succeed
        assert!(oids.try_acquire("def456"));

        // Release and re-acquire should work
        oids.release("abc123");
        assert!(oids.try_acquire("abc123"));

        // Other OID still held
        assert!(!oids.try_acquire("def456"));
    }
}
