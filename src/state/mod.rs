//! State management for tracking blob storage type.
//!
//! This module provides the [`StorageStateManager`] trait for tracking whether
//! each blob is stored as raw bytes ([`StorageState::RawOnly`]) or as xet chunks
//! ([`StorageState::XetOnly`]).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod sqlite;

// Re-export the SQLite implementation for convenience
pub use sqlite::SqliteStateManager;

/// The storage state of a blob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StorageState {
    /// Blob is stored as raw bytes only.
    RawOnly,
    /// Blob is stored as xet chunks only.
    XetOnly,
}

/// State information for a stored file/blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    /// The storage state of the file.
    pub state: StorageState,
    /// The xet file ID (if converted to xet format).
    pub xet_file_id: Option<String>,
    /// Size of the file in bytes.
    pub size: u64,
    /// SHA256 hash of the file (same as the oid).
    pub sha256: String,
    /// Unix timestamp when the file was created.
    pub created_at: u64,
    /// Unix timestamp when the file was converted to xet format.
    pub converted_at: Option<u64>,
}

/// Trait for managing blob storage state.
///
/// This trait abstracts the storage backend, allowing SQLite to be swapped
/// for PostgreSQL later.
#[async_trait]
pub trait StorageStateManager: Send + Sync {
    /// Get the state of a blob by its OID (SHA256 hash).
    async fn get_state(&self, oid: &str) -> Result<Option<FileState>, StateError>;

    /// Register a new raw blob (not yet converted to xet).
    ///
    /// This is idempotent - calling it multiple times with the same OID
    /// will not error.
    async fn register_raw_blob(&self, oid: &str, size: u64) -> Result<(), StateError>;

    /// Register a blob as xet-only (already in xet format).
    ///
    /// This is used when a file is uploaded directly in xet format.
    async fn register_xet_only(&self, oid: &str, file_id: &str, size: u64) -> Result<(), StateError>;

    /// Mark a raw blob as converted to xet format.
    ///
    /// Updates the state to XetOnly and sets the xet_file_id and converted_at.
    /// Returns an error if the OID is not found.
    async fn mark_converted(&self, oid: &str, file_id: &str) -> Result<(), StateError>;

    /// Get the state of multiple blobs by their OIDs.
    ///
    /// Returns a vector of (oid, Option<FileState>) tuples.
    /// If an OID is not found, the corresponding FileState will be None.
    async fn get_states(&self, oids: &[String]) -> Result<Vec<(String, Option<FileState>)>, StateError>;
}

/// Errors that can occur during state management operations.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Database error occurred.
    #[error("Database error: {0}")]
    Database(String),
    /// Internal error occurred.
    #[error("Internal error: {0}")]
    Internal(String),
}