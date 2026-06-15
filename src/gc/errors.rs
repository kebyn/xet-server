//! GC error types.
//!
//! I7 fix: Uses `crate::storage::StorageError` instead of a duplicate type.
//! This eliminates confusion between the two StorageError variants and allows
//! using `?` operator to convert storage errors to GcError.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GcError {
    #[error("Bloom Filter corrupted: expected CRC32 {expected}, got {actual}")]
    BloomFilterCorrupted { expected: u32, actual: u32 },

    #[error("Checkpoint corrupted: expected CRC32 {expected}, got {actual}")]
    CheckpointCorrupted { expected: u32, actual: u32 },

    #[error("Sidecar missing for shard: {0}")]
    SidecarMissing(String),

    #[error("Lease expired")]
    LeaseExpired,

    #[error("Lease held by another node: {0}")]
    LeaseHeldByOther(String),

    #[error("Shard parse error: {0}")]
    ShardParse(String),

    #[error("Reference count mismatch: expected {expected}, got {actual}")]
    ReferenceCountMismatch { expected: usize, actual: usize },

    #[error("Scan timeout after {0} seconds")]
    ScanTimeout(u64),

    /// I7 fix: Uses storage::StorageError directly (no duplicate type).
    #[error("Storage error: {0}")]
    Storage(#[from] crate::storage::StorageError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type GcResult<T> = Result<T, GcError>;
