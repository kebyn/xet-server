pub mod sqlite;

pub use sqlite::SqliteMetadataStore;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Repository type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoType {
    Model,
    Dataset,
    Space,
}

impl std::fmt::Display for RepoType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoType::Model => write!(f, "model"),
            RepoType::Dataset => write!(f, "dataset"),
            RepoType::Space => write!(f, "space"),
        }
    }
}

impl std::str::FromStr for RepoType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "model" => Ok(RepoType::Model),
            "dataset" => Ok(RepoType::Dataset),
            "space" => Ok(RepoType::Space),
            _ => Err(format!("Invalid repo type: {}", s)),
        }
    }
}

/// Repository metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub namespace: String,
    pub repo_type: RepoType,
    pub sha: Option<String>,
    pub private: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Revision (commit) metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub commit_id: String,
    pub repo_id: i64,
    pub parent: Option<String>,
    pub message: String,
    pub author: String,
    pub created_at: i64,
}

/// File entry in the tree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub repo_id: i64,
    pub commit_id: String,
    pub size: u64,
    pub cas_hash: String,
    pub is_lfs: bool,
}

/// Metadata store error
#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("Repository not found: {0}")]
    RepoNotFound(String),
    #[error("Repository already exists: {0}")]
    RepoAlreadyExists(String),
    #[error("Revision not found: {0}")]
    RevisionNotFound(String),
    #[error("File not found: {0}")]
    FileNotFound(String),
    #[error("Database error: {0}")]
    DatabaseError(String),
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),
}

/// Trait for metadata storage operations
#[async_trait]
pub trait MetadataStore: Send + Sync {
    /// Create a new repository
    async fn create_repo(
        &self,
        namespace: &str,
        name: &str,
        repo_type: RepoType,
        private: bool,
    ) -> Result<Repo, MetadataError>;

    /// Get a repository by namespace and name
    async fn get_repo(
        &self,
        namespace: &str,
        name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, MetadataError>;

    /// Delete a repository
    async fn delete_repo(&self, repo_id: i64) -> Result<(), MetadataError>;

    /// Add a revision (commit)
    async fn add_revision(&self, revision: Revision) -> Result<(), MetadataError>;

    /// Get a specific revision
    async fn get_revision(
        &self,
        repo_id: i64,
        commit_id: &str,
    ) -> Result<Revision, MetadataError>;

    /// Get the HEAD commit for a repository
    async fn get_head(&self, repo_id: i64) -> Result<Option<String>, MetadataError>;

    /// Set the HEAD commit for a repository
    async fn set_head(&self, repo_id: i64, commit_id: &str) -> Result<(), MetadataError>;

    /// Get the commit log (history from HEAD)
    async fn get_commit_log(
        &self,
        repo_id: i64,
        limit: Option<usize>,
    ) -> Result<Vec<Revision>, MetadataError>;

    /// Add file entries for a commit
    async fn add_file_entries(&self, entries: Vec<FileEntry>) -> Result<(), MetadataError>;

    /// Get the file tree for a commit
    async fn get_file_tree(
        &self,
        repo_id: i64,
        commit_id: &str,
    ) -> Result<Vec<FileEntry>, MetadataError>;

    /// Get file entries with a path prefix
    async fn get_file_tree_prefix(
        &self,
        repo_id: i64,
        commit_id: &str,
        prefix: &str,
    ) -> Result<Vec<FileEntry>, MetadataError>;

    /// Resolve a single file
    async fn resolve_file(
        &self,
        repo_id: i64,
        commit_id: &str,
        path: &str,
    ) -> Result<FileEntry, MetadataError>;
}