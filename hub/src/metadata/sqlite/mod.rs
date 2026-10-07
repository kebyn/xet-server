//! SQLx-based SQLite metadata store
//!
//! Async SQLite implementation using sqlx for true async database operations.
//! Migrated from rusqlite to prevent blocking the async runtime.
//!
//! Layout: this module owns the store struct, the connection/transaction
//! guard, and the `MetadataStore` impl (thin delegators); the SQL lives in
//! the child modules — `repos`, `revisions`, `file_tree`, `atomic` (the
//! BEGIN IMMEDIATE commit path), and the shared row-mapping `helpers`.
//! Note: `impl MetadataStore for SqliteMetadataStore` must stay a single
//! block in this file — a second impl of the same trait for the same type
//! is rejected by the compiler (E0119) even with disjoint methods.

use super::{
    FileEntry, FileTreeChange, FileTreePage, MetadataError, MetadataStore, Repo, RepoType, Revision,
};
use crate::sqlite_pool::{connect_hub_sqlite_pool, connect_in_memory_hub_sqlite_pool};
use async_trait::async_trait;
use sqlx::Sqlite;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnection, SqlitePool};
use std::ops::{Deref, DerefMut};

mod atomic;
mod file_tree;
mod helpers;
mod repos;
mod revisions;

#[cfg(test)]
mod tests;

/// Async SQLite-based metadata store using sqlx connection pool
///
/// Uses SqlitePool for true async operations with connection pooling.
/// WAL mode is enabled for better read/write concurrency.
pub struct SqliteMetadataStore {
    pool: SqlitePool,
}

enum CommitWrite<'a> {
    Snapshot(&'a [FileEntry]),
    Changes(&'a [FileTreeChange]),
}

/// Prevents a cancelled transaction from returning an open SQLite transaction
/// to the pool. Resolved transactions reuse their connection normally; an
/// unresolved connection is closed, which makes SQLite roll back atomically.
struct TransactionConnectionGuard {
    connection: PoolConnection<Sqlite>,
    resolved: bool,
}

impl TransactionConnectionGuard {
    fn new(connection: PoolConnection<Sqlite>) -> Self {
        Self {
            connection,
            resolved: false,
        }
    }

    fn mark_resolved(&mut self) {
        self.resolved = true;
    }
}

impl Deref for TransactionConnectionGuard {
    type Target = SqliteConnection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl DerefMut for TransactionConnectionGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.connection
    }
}

impl Drop for TransactionConnectionGuard {
    fn drop(&mut self) {
        if !self.resolved {
            self.connection.close_on_drop();
        }
    }
}

impl SqliteMetadataStore {
    /// Create a new SQLite metadata store with connection pool
    pub async fn new(path: &str, pool_size: u32) -> Result<Self, MetadataError> {
        let pool = connect_hub_sqlite_pool(path, pool_size)
            .await
            .map_err(MetadataError::DatabaseError)?;

        Self::init_pool(&pool).await?;

        Ok(Self { pool })
    }

    /// Create a metadata store using a shared connection pool.
    /// This reduces total SQLite connections when both TokenStore and MetadataStore
    /// access the same database file, preventing SQLITE_BUSY under load.
    pub async fn with_pool(pool: SqlitePool) -> Result<Self, MetadataError> {
        Self::init_pool(&pool).await?;
        Ok(Self { pool })
    }

    /// Create an in-memory metadata store for testing
    pub async fn in_memory() -> Result<Self, MetadataError> {
        // Note: SQLite in-memory databases are per-connection. With a pool of
        // multiple connections, each would see its own empty database. We use
        // max_connections(1) so all operations see the same in-memory database.
        let pool = connect_in_memory_hub_sqlite_pool()
            .await
            .map_err(MetadataError::DatabaseError)?;

        Self::init_pool(&pool).await?;

        Ok(Self { pool })
    }

    /// Common pool initialization: create schema, set version.
    /// Note: PRAGMA settings are configured by `sqlite_pool`.
    async fn init_pool(pool: &SqlitePool) -> Result<(), MetadataError> {
        crate::migrations::run_hub_migrations(pool)
            .await
            .map_err(MetadataError::DatabaseError)
    }
}

#[async_trait]
impl MetadataStore for SqliteMetadataStore {
    async fn create_repo(
        &self,
        namespace: &str,
        name: &str,
        repo_type: RepoType,
        private: bool,
    ) -> Result<Repo, MetadataError> {
        repos::create_repo(&self.pool, namespace, name, repo_type, private).await
    }

    async fn get_repo(
        &self,
        namespace: &str,
        name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, MetadataError> {
        repos::get_repo(&self.pool, namespace, name, repo_type).await
    }

    /// Delete a repository and all its metadata (file_tree, heads, revisions, repo record)
    ///
    /// **Known tradeoff:** This does NOT delete associated blobs from CAS. Since blobs are
    /// content-addressed and deduplicated, orphaned blobs don't affect correctness. A background
    /// GC job could clean up orphaned blobs in the future if storage efficiency becomes a concern.
    async fn delete_repo(&self, repo_id: i64) -> Result<(), MetadataError> {
        repos::delete_repo(&self.pool, repo_id).await
    }

    async fn add_revision(&self, revision: Revision) -> Result<(), MetadataError> {
        revisions::add_revision(&self.pool, revision).await
    }

    async fn get_revision(&self, repo_id: i64, commit_id: &str) -> Result<Revision, MetadataError> {
        revisions::get_revision(&self.pool, repo_id, commit_id).await
    }

    async fn get_head(&self, repo_id: i64) -> Result<Option<String>, MetadataError> {
        revisions::get_head(&self.pool, repo_id).await
    }

    async fn set_head(&self, repo_id: i64, commit_id: &str) -> Result<(), MetadataError> {
        revisions::set_head(&self.pool, repo_id, commit_id).await
    }

    async fn get_commit_log(
        &self,
        repo_id: i64,
        limit: Option<usize>,
    ) -> Result<Vec<Revision>, MetadataError> {
        revisions::get_commit_log(&self.pool, repo_id, limit).await
    }

    async fn add_file_entries(&self, entries: Vec<FileEntry>) -> Result<(), MetadataError> {
        file_tree::add_file_entries(&self.pool, entries).await
    }

    async fn get_file_tree(
        &self,
        repo_id: i64,
        commit_id: &str,
    ) -> Result<Vec<FileEntry>, MetadataError> {
        file_tree::get_file_tree(&self.pool, repo_id, commit_id).await
    }

    async fn get_file_tree_prefix(
        &self,
        repo_id: i64,
        commit_id: &str,
        prefix: &str,
    ) -> Result<Vec<FileEntry>, MetadataError> {
        file_tree::get_file_tree_prefix(&self.pool, repo_id, commit_id, prefix).await
    }

    async fn get_file_tree_prefix_page(
        &self,
        repo_id: i64,
        commit_id: &str,
        prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<FileTreePage, MetadataError> {
        file_tree::get_file_tree_prefix_page(
            &self.pool, repo_id, commit_id, prefix, after_path, limit,
        )
        .await
    }

    async fn resolve_file(
        &self,
        repo_id: i64,
        commit_id: &str,
        path: &str,
    ) -> Result<FileEntry, MetadataError> {
        file_tree::resolve_file(&self.pool, repo_id, commit_id, path).await
    }

    async fn commit_atomic(
        &self,
        rev: &Revision,
        entries: &[FileEntry],
        expected_parent: Option<&str>,
    ) -> Result<(), MetadataError> {
        atomic::commit_atomic_write(
            &self.pool,
            rev,
            CommitWrite::Snapshot(entries),
            expected_parent,
        )
        .await
    }

    async fn commit_changes_atomic(
        &self,
        rev: &Revision,
        changes: &[FileTreeChange],
        expected_parent: Option<&str>,
    ) -> Result<(), MetadataError> {
        atomic::commit_atomic_write(
            &self.pool,
            rev,
            CommitWrite::Changes(changes),
            expected_parent,
        )
        .await
    }
}
