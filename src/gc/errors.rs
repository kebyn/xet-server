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

    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Object not found: {0}")]
    NotFound(String),

    #[error("Condition failed (optimistic locking)")]
    ConditionFailed,

    #[error("S3 error: {0}")]
    S3(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),
}

pub type GcResult<T> = Result<T, GcError>;
