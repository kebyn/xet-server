//! SQLx-based SQLite metadata store
//!
//! Async SQLite implementation using sqlx for true async database operations.
//! Migrated from rusqlite to prevent blocking the async runtime.

use super::{FileEntry, FileTreeChange, MetadataError, MetadataStore, Repo, RepoType, Revision};
use crate::sqlite_pool::{connect_hub_sqlite_pool, connect_in_memory_hub_sqlite_pool};
use async_trait::async_trait;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnection, SqlitePool};
use sqlx::{Connection, Row, Sqlite};
use std::ops::{Deref, DerefMut};

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

/// Check if a sqlx::Error represents a UNIQUE constraint violation.
///
/// SQLite returns extended error code 1555 (SQLITE_CONSTRAINT_UNIQUE) or
/// 2067 (SQLITE_CONSTRAINT_PRIMARYKEY) for unique constraint violations.
/// sqlx exposes these via `DatabaseError::code()` as string representations.
fn is_unique_violation(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        if let Some(code) = db_err.code() {
            // SQLite extended codes: 1555 = CONSTRAINT_UNIQUE, 2067 = CONSTRAINT_PRIMARYKEY
            // SQLite primary code: 19 = SQLITE_CONSTRAINT (used when extended codes disabled)
            code == "1555" || code == "2067" || code == "19"
        } else {
            // Fall back to message inspection if code not available
            db_err.message().contains("UNIQUE constraint failed")
        }
    } else {
        false
    }
}

/// Map a `sqlx::Row` to a `Repo` value.
fn row_to_repo(row: &sqlx::sqlite::SqliteRow) -> Result<Repo, MetadataError> {
    let repo_type_str: String = row
        .try_get(3)
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    let repo_type = repo_type_str
        .parse::<RepoType>()
        .map_err(|e| MetadataError::DatabaseError(format!("Invalid repo_type: {}", e)))?;
    Ok(Repo {
        id: row
            .try_get(0)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        name: row
            .try_get(1)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        namespace: row
            .try_get(2)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        repo_type,
        sha: row
            .try_get(4)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        private: row
            .try_get::<i64, _>(5)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
            != 0,
        created_at: row
            .try_get(6)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        updated_at: row
            .try_get(7)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
    })
}

/// Map a `sqlx::Row` to a `Revision` value.
fn row_to_revision(row: &sqlx::sqlite::SqliteRow) -> Result<Revision, MetadataError> {
    Ok(Revision {
        commit_id: row
            .try_get(0)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        repo_id: row
            .try_get(1)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        parent: row
            .try_get(2)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        message: row
            .try_get(3)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        author: row
            .try_get(4)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        created_at: row
            .try_get(5)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
    })
}

/// Map a `sqlx::Row` to a `FileEntry` value.
fn row_to_file_entry(row: &sqlx::sqlite::SqliteRow) -> Result<FileEntry, MetadataError> {
    let stored_size = row
        .try_get::<i64, _>(3)
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    let size = u64::try_from(stored_size).map_err(|_| {
        MetadataError::DatabaseError(format!(
            "Corrupt file_tree row contains negative size {}",
            stored_size
        ))
    })?;

    Ok(FileEntry {
        path: row
            .try_get(0)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        repo_id: row
            .try_get(1)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        commit_id: row
            .try_get(2)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        size,
        cas_hash: row
            .try_get(4)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        is_lfs: row
            .try_get::<i64, _>(5)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
            != 0,
    })
}

fn file_size_to_sql(size: u64) -> Result<i64, MetadataError> {
    i64::try_from(size).map_err(|_| {
        MetadataError::InvalidOperation(format!("File size {} exceeds SQLite INTEGER range", size))
    })
}

fn escape_like_pattern(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl SqliteMetadataStore {
    /// Create a new SQLite metadata store with connection pool
    pub async fn new(path: &str, pool_size: u32) -> Result<Self, MetadataError> {
        let pool = connect_hub_sqlite_pool(path, pool_size)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Self::init_pool(&pool).await?;

        Ok(Self { pool })
    }

    /// M2 fix: Create a metadata store using a shared connection pool.
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
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Self::init_pool(&pool).await?;

        Ok(Self { pool })
    }

    /// Common pool initialization: create schema, set version.
    /// Note: PRAGMA settings are configured by `sqlite_pool`.
    async fn init_pool(pool: &SqlitePool) -> Result<(), MetadataError> {
        crate::migrations::run_hub_migrations(pool)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))
    }

    async fn commit_atomic_write(
        &self,
        rev: &Revision,
        write: CommitWrite<'_>,
        expected_parent: Option<&str>,
    ) -> Result<(), MetadataError> {
        // BEGIN IMMEDIATE acquires SQLite's single-writer lock before checking HEAD.
        // If this future is cancelled, the connection guard closes the connection;
        // SQLite then rolls back before the pool can create a replacement.
        let connection = self
            .pool
            .acquire()
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        let mut connection = TransactionConnectionGuard::new(connection);
        let mut tx = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let result = async {
            let current_head: Option<String> =
                sqlx::query("SELECT commit_id FROM heads WHERE repo_id = ?1")
                    .bind(rev.repo_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
                    .map(|row| row.try_get::<String, _>(0))
                    .transpose()
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            if current_head.as_deref() != expected_parent {
                return Err(MetadataError::Conflict(current_head.unwrap_or_default()));
            }

            sqlx::query(
                "INSERT INTO revisions (commit_id, repo_id, parent, message, author, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .bind(&rev.commit_id)
            .bind(rev.repo_id)
            .bind(&rev.parent)
            .bind(&rev.message)
            .bind(&rev.author)
            .bind(rev.created_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            if let CommitWrite::Changes(_) = write {
                if let Some(parent) = expected_parent {
                    sqlx::query(
                        "INSERT INTO file_tree \
                         (path, repo_id, commit_id, size, cas_hash, is_lfs) \
                         SELECT path, ?1, ?2, size, cas_hash, is_lfs \
                         FROM file_tree WHERE repo_id = ?1 AND commit_id = ?3",
                    )
                    .bind(rev.repo_id)
                    .bind(&rev.commit_id)
                    .bind(parent)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                }
            }

            match write {
                CommitWrite::Snapshot(entries) => {
                    for entry in entries {
                        insert_file_entry(&mut tx, rev, entry).await?;
                    }
                }
                CommitWrite::Changes(changes) => {
                    for change in changes {
                        match change {
                            FileTreeChange::Upsert(entry) => {
                                insert_file_entry(&mut tx, rev, entry).await?;
                            }
                            FileTreeChange::Delete(path) => {
                                sqlx::query(
                                    "DELETE FROM file_tree \
                                     WHERE path = ?1 AND repo_id = ?2 AND commit_id = ?3",
                                )
                                .bind(path)
                                .bind(rev.repo_id)
                                .bind(&rev.commit_id)
                                .execute(&mut *tx)
                                .await
                                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                            }
                        }
                    }
                }
            }

            sqlx::query("INSERT OR REPLACE INTO heads (repo_id, commit_id) VALUES (?1, ?2)")
                .bind(rev.repo_id)
                .bind(&rev.commit_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            Ok::<(), MetadataError>(())
        }
        .await;

        match result {
            Ok(()) => {
                tx.commit()
                    .await
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                connection.mark_resolved();
                Ok(())
            }
            Err(error) => {
                tx.rollback().await.map_err(|rollback_error| {
                    MetadataError::DatabaseError(format!(
                        "Commit failed ({error}); rollback failed: {rollback_error}"
                    ))
                })?;
                connection.mark_resolved();
                Err(error)
            }
        }
    }
}

async fn insert_file_entry(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    rev: &Revision,
    entry: &FileEntry,
) -> Result<(), MetadataError> {
    let is_lfs: i64 = if entry.is_lfs { 1 } else { 0 };
    let size = file_size_to_sql(entry.size)?;
    sqlx::query(
        "INSERT OR REPLACE INTO file_tree \
         (path, repo_id, commit_id, size, cas_hash, is_lfs) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&entry.path)
    .bind(rev.repo_id)
    .bind(&rev.commit_id)
    .bind(size)
    .bind(&entry.cas_hash)
    .bind(is_lfs)
    .execute(&mut **tx)
    .await
    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    Ok(())
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
        let repo_type_str = repo_type.to_string();
        let private_int: i64 = if private { 1 } else { 0 };
        let now = crate::util::unix_now_secs() as i64;

        let result = sqlx::query(
            "INSERT INTO repos (name, namespace, repo_type, private, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        )
        .bind(name)
        .bind(namespace)
        .bind(&repo_type_str)
        .bind(private_int)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;

        match result {
            Ok(result) => {
                let id = result.last_insert_rowid();
                Ok(Repo {
                    id,
                    name: name.to_string(),
                    namespace: namespace.to_string(),
                    repo_type,
                    sha: None,
                    private,
                    created_at: now,
                    updated_at: now,
                })
            }
            Err(e) if is_unique_violation(&e) => Err(MetadataError::RepoAlreadyExists(format!(
                "{}/{}/{}",
                namespace, name, repo_type_str
            ))),
            Err(e) => Err(MetadataError::DatabaseError(e.to_string())),
        }
    }

    async fn get_repo(
        &self,
        namespace: &str,
        name: &str,
        repo_type: RepoType,
    ) -> Result<Repo, MetadataError> {
        let repo_type_str = repo_type.to_string();

        let row = sqlx::query(
            "SELECT id, name, namespace, repo_type, sha, private, created_at, updated_at FROM repos WHERE namespace = ?1 AND name = ?2 AND repo_type = ?3"
        )
        .bind(namespace)
        .bind(name)
        .bind(&repo_type_str)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        match row {
            Some(row) => row_to_repo(&row),
            None => Err(MetadataError::RepoNotFound(format!(
                "{}/{}/{}",
                namespace, name, repo_type_str
            ))),
        }
    }

    /// Delete a repository and all its metadata (file_tree, heads, revisions, repo record)
    ///
    /// **Known tradeoff:** This does NOT delete associated blobs from CAS. Since blobs are
    /// content-addressed and deduplicated, orphaned blobs don't affect correctness. A background
    /// GC job could clean up orphaned blobs in the future if storage efficiency becomes a concern.
    async fn delete_repo(&self, repo_id: i64) -> Result<(), MetadataError> {
        // I5: Wrap deletion in a transaction for atomicity
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let result = async {
            // Delete in order (foreign key constraints)
            sqlx::query("DELETE FROM file_tree WHERE repo_id = ?1")
                .bind(repo_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            sqlx::query("DELETE FROM heads WHERE repo_id = ?1")
                .bind(repo_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            sqlx::query("DELETE FROM revisions WHERE repo_id = ?1")
                .bind(repo_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            let rows = sqlx::query("DELETE FROM repos WHERE id = ?1")
                .bind(repo_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            if rows.rows_affected() == 0 {
                Err(MetadataError::RepoNotFound(format!("id={}", repo_id)))
            } else {
                Ok(())
            }
        }
        .await;

        match result {
            Ok(()) => {
                tx.commit()
                    .await
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                Ok(())
            }
            Err(e) => {
                // I2 FIX: Rollback is implicit when tx is dropped without commit.
                // Removed meaningless SELECT 1 statement.
                drop(tx);
                Err(e)
            }
        }
    }

    async fn add_revision(&self, revision: Revision) -> Result<(), MetadataError> {
        sqlx::query(
            "INSERT INTO revisions (commit_id, repo_id, parent, message, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        )
        .bind(&revision.commit_id)
        .bind(revision.repo_id)
        .bind(&revision.parent)
        .bind(&revision.message)
        .bind(&revision.author)
        .bind(revision.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(())
    }

    async fn get_revision(&self, repo_id: i64, commit_id: &str) -> Result<Revision, MetadataError> {
        let row = sqlx::query(
            "SELECT commit_id, repo_id, parent, message, author, created_at FROM revisions WHERE repo_id = ?1 AND commit_id = ?2"
        )
        .bind(repo_id)
        .bind(commit_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        match row {
            Some(row) => row_to_revision(&row),
            None => Err(MetadataError::RevisionNotFound(commit_id.to_string())),
        }
    }

    async fn get_head(&self, repo_id: i64) -> Result<Option<String>, MetadataError> {
        let row = sqlx::query("SELECT commit_id FROM heads WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        match row {
            Some(r) => {
                Ok(Some(r.try_get(0).map_err(|e| {
                    MetadataError::DatabaseError(e.to_string())
                })?))
            }
            None => Ok(None),
        }
    }

    async fn set_head(&self, repo_id: i64, commit_id: &str) -> Result<(), MetadataError> {
        sqlx::query("INSERT OR REPLACE INTO heads (repo_id, commit_id) VALUES (?1, ?2)")
            .bind(repo_id)
            .bind(commit_id)
            .execute(&self.pool)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(())
    }

    async fn get_commit_log(
        &self,
        repo_id: i64,
        limit: Option<usize>,
    ) -> Result<Vec<Revision>, MetadataError> {
        // M3 fix: Use recursive CTE instead of N+1 queries.
        // Single SQL query walks the entire parent chain from HEAD.
        // Use i64::MAX - 1 to avoid overflow when casting from usize.
        let effective_limit = limit.map(|l| l as i64).unwrap_or(i64::MAX - 1);

        // The recursive step condition uses (ch.depth + 1 < ?2) so that
        // the base case (depth=0) plus (limit-1) recursive steps yields
        // exactly `limit` rows total.
        let rows = sqlx::query(
            "WITH RECURSIVE commit_history AS (
                SELECT r.commit_id, r.repo_id, r.parent, r.message, r.author, r.created_at, 0 AS depth
                FROM revisions r
                INNER JOIN heads h ON r.commit_id = h.commit_id
                WHERE h.repo_id = ?1
                UNION ALL
                SELECT r.commit_id, r.repo_id, r.parent, r.message, r.author, r.created_at, ch.depth + 1
                FROM revisions r
                INNER JOIN commit_history ch ON r.commit_id = ch.parent
                WHERE ch.depth + 1 < ?2
            )
            SELECT commit_id, repo_id, parent, message, author, created_at
            FROM commit_history
            ORDER BY depth"
        )
        .bind(repo_id)
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        rows.iter().map(row_to_revision).collect()
    }

    async fn add_file_entries(&self, entries: Vec<FileEntry>) -> Result<(), MetadataError> {
        let connection = self
            .pool
            .acquire()
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        let mut connection = TransactionConnectionGuard::new(connection);
        let mut tx = connection
            .begin()
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        for entry in &entries {
            let is_lfs_int: i64 = if entry.is_lfs { 1 } else { 0 };
            let size = file_size_to_sql(entry.size)?;
            sqlx::query(
                "INSERT OR REPLACE INTO file_tree (path, repo_id, commit_id, size, cas_hash, is_lfs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            )
            .bind(&entry.path)
            .bind(entry.repo_id)
            .bind(&entry.commit_id)
            .bind(size)
            .bind(&entry.cas_hash)
            .bind(is_lfs_int)
            .execute(&mut *tx)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        connection.mark_resolved();

        Ok(())
    }

    async fn get_file_tree(
        &self,
        repo_id: i64,
        commit_id: &str,
    ) -> Result<Vec<FileEntry>, MetadataError> {
        let rows = sqlx::query(
            "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 ORDER BY path"
        )
        .bind(repo_id)
        .bind(commit_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        rows.iter().map(row_to_file_entry).collect()
    }

    async fn get_file_tree_prefix(
        &self,
        repo_id: i64,
        commit_id: &str,
        prefix: &str,
    ) -> Result<Vec<FileEntry>, MetadataError> {
        let normalized_prefix = prefix.trim_matches('/');

        let rows = if normalized_prefix.is_empty() {
            sqlx::query(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                 FROM file_tree \
                 WHERE repo_id = ?1 AND commit_id = ?2 \
                 ORDER BY path",
            )
            .bind(repo_id)
            .bind(commit_id)
            .fetch_all(&self.pool)
            .await
        } else {
            let escaped_prefix = escape_like_pattern(normalized_prefix);
            let child_pattern = format!("{}/%", escaped_prefix);
            sqlx::query(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                 FROM file_tree \
                 WHERE repo_id = ?1 \
                   AND commit_id = ?2 \
                   AND (path = ?3 OR path LIKE ?4 ESCAPE '\\') \
                 ORDER BY path",
            )
            .bind(repo_id)
            .bind(commit_id)
            .bind(normalized_prefix)
            .bind(&child_pattern)
            .fetch_all(&self.pool)
            .await
        };

        let rows = rows.map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        rows.iter().map(row_to_file_entry).collect()
    }

    async fn get_file_tree_prefix_page(
        &self,
        repo_id: i64,
        commit_id: &str,
        prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<super::FileTreePage, MetadataError> {
        let normalized_prefix = prefix.trim_matches('/');
        let fetch_limit = limit
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| {
                MetadataError::InvalidOperation(
                    "Tree page limit exceeds the supported SQLite range".to_string(),
                )
            })?;

        let rows = match (normalized_prefix.is_empty(), after_path) {
            (true, None) => {
                sqlx::query(
                    "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                     FROM file_tree \
                     WHERE repo_id = ?1 AND commit_id = ?2 \
                     ORDER BY path LIMIT ?3",
                )
                .bind(repo_id)
                .bind(commit_id)
                .bind(fetch_limit)
                .fetch_all(&self.pool)
                .await
            }
            (true, Some(after_path)) => {
                sqlx::query(
                    "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                     FROM file_tree \
                     WHERE repo_id = ?1 AND commit_id = ?2 AND path > ?3 \
                     ORDER BY path LIMIT ?4",
                )
                .bind(repo_id)
                .bind(commit_id)
                .bind(after_path)
                .bind(fetch_limit)
                .fetch_all(&self.pool)
                .await
            }
            (false, None) => {
                let escaped_prefix = escape_like_pattern(normalized_prefix);
                let child_pattern = format!("{}/%", escaped_prefix);
                sqlx::query(
                    "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                     FROM file_tree \
                     WHERE repo_id = ?1 AND commit_id = ?2 \
                       AND (path = ?3 OR path LIKE ?4 ESCAPE '\\') \
                     ORDER BY path LIMIT ?5",
                )
                .bind(repo_id)
                .bind(commit_id)
                .bind(normalized_prefix)
                .bind(child_pattern)
                .bind(fetch_limit)
                .fetch_all(&self.pool)
                .await
            }
            (false, Some(after_path)) => {
                let escaped_prefix = escape_like_pattern(normalized_prefix);
                let child_pattern = format!("{}/%", escaped_prefix);
                sqlx::query(
                    "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs \
                     FROM file_tree \
                     WHERE repo_id = ?1 AND commit_id = ?2 AND path > ?3 \
                       AND (path = ?4 OR path LIKE ?5 ESCAPE '\\') \
                     ORDER BY path LIMIT ?6",
                )
                .bind(repo_id)
                .bind(commit_id)
                .bind(after_path)
                .bind(normalized_prefix)
                .bind(child_pattern)
                .bind(fetch_limit)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|error| MetadataError::DatabaseError(error.to_string()))?;

        let mut entries: Vec<FileEntry> = rows
            .iter()
            .map(row_to_file_entry)
            .collect::<Result<_, _>>()?;
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        let next_after_path = if has_more {
            entries.last().map(|entry| entry.path.clone())
        } else {
            None
        };

        Ok(super::FileTreePage {
            entries,
            next_after_path,
        })
    }

    async fn resolve_file(
        &self,
        repo_id: i64,
        commit_id: &str,
        path: &str,
    ) -> Result<FileEntry, MetadataError> {
        let row = sqlx::query(
            "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 AND path = ?3"
        )
        .bind(repo_id)
        .bind(commit_id)
        .bind(path)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        match row {
            Some(r) => row_to_file_entry(&r),
            None => Err(MetadataError::FileNotFound(format!(
                "{}/{}",
                commit_id, path
            ))),
        }
    }

    async fn commit_atomic(
        &self,
        rev: &Revision,
        entries: &[FileEntry],
        expected_parent: Option<&str>,
    ) -> Result<(), MetadataError> {
        self.commit_atomic_write(rev, CommitWrite::Snapshot(entries), expected_parent)
            .await
    }

    async fn commit_changes_atomic(
        &self,
        rev: &Revision,
        changes: &[FileTreeChange],
        expected_parent: Option<&str>,
    ) -> Result<(), MetadataError> {
        self.commit_atomic_write(rev, CommitWrite::Changes(changes), expected_parent)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{FileEntry, MetadataStore, RepoType};

    #[tokio::test]
    async fn test_get_file_tree_prefix_respects_path_boundary() {
        let store = SqliteMetadataStore::in_memory().await.unwrap();
        let repo = store
            .create_repo("ns", "repo", RepoType::Model, false)
            .await
            .unwrap();
        let commit_id = "c1";
        store
            .add_revision(Revision {
                commit_id: commit_id.to_string(),
                repo_id: repo.id,
                parent: None,
                message: "Initial".to_string(),
                author: "ns".to_string(),
                created_at: 1000,
            })
            .await
            .unwrap();
        store
            .add_file_entries(vec![
                FileEntry {
                    path: "models/a.bin".to_string(),
                    repo_id: repo.id,
                    commit_id: commit_id.to_string(),
                    size: 1,
                    cas_hash: "h1".to_string(),
                    is_lfs: true,
                },
                FileEntry {
                    path: "models2/b.bin".to_string(),
                    repo_id: repo.id,
                    commit_id: commit_id.to_string(),
                    size: 1,
                    cas_hash: "h2".to_string(),
                    is_lfs: true,
                },
            ])
            .await
            .unwrap();

        let entries = store
            .get_file_tree_prefix(repo.id, commit_id, "models")
            .await
            .unwrap();
        let paths: Vec<_> = entries.into_iter().map(|e| e.path).collect();
        assert_eq!(paths, vec!["models/a.bin".to_string()]);
    }

    #[tokio::test]
    async fn test_negative_stored_file_size_is_reported_as_corruption() {
        let store = SqliteMetadataStore::in_memory().await.unwrap();
        let repo = store
            .create_repo("ns", "negative-size", RepoType::Model, false)
            .await
            .unwrap();
        let revision = Revision {
            commit_id: "negative-size-commit".to_string(),
            repo_id: repo.id,
            parent: None,
            message: "corrupt fixture".to_string(),
            author: "ns".to_string(),
            created_at: 1,
        };
        store.add_revision(revision).await.unwrap();
        sqlx::query(
            "INSERT INTO file_tree (path, repo_id, commit_id, size, cas_hash, is_lfs) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind("bad.bin")
        .bind(repo.id)
        .bind("negative-size-commit")
        .bind(-1_i64)
        .bind("bad-hash")
        .bind(1_i64)
        .execute(&store.pool)
        .await
        .unwrap();

        let err = store
            .get_file_tree(repo.id, "negative-size-commit")
            .await
            .expect_err("negative SQLite size must not become a huge u64");

        assert!(matches!(err, MetadataError::DatabaseError(_)));
        assert!(err.to_string().contains("negative size"));
    }

    #[tokio::test]
    async fn test_commit_atomic_rejects_file_size_above_sqlite_range() {
        let store = SqliteMetadataStore::in_memory().await.unwrap();
        let repo = store
            .create_repo("ns", "oversized", RepoType::Model, false)
            .await
            .unwrap();
        let revision = Revision {
            commit_id: "oversized-commit".to_string(),
            repo_id: repo.id,
            parent: None,
            message: "oversized".to_string(),
            author: "ns".to_string(),
            created_at: 1,
        };
        let entry = FileEntry {
            path: "huge.bin".to_string(),
            repo_id: repo.id,
            commit_id: revision.commit_id.clone(),
            size: u64::MAX,
            cas_hash: "hash".to_string(),
            is_lfs: true,
        };

        let err = store
            .commit_atomic(&revision, &[entry], None)
            .await
            .expect_err("u64 size above i64::MAX must be rejected");

        assert!(matches!(err, MetadataError::InvalidOperation(_)));
        assert_eq!(store.get_head(repo.id).await.unwrap(), None);
        assert!(matches!(
            store.get_revision(repo.id, &revision.commit_id).await,
            Err(MetadataError::RevisionNotFound(_))
        ));
    }

    #[tokio::test]
    async fn commit_changes_copies_parent_and_applies_ordered_delta_atomically() {
        let store = SqliteMetadataStore::in_memory().await.unwrap();
        let repo = store
            .create_repo("ns", "delta", RepoType::Model, false)
            .await
            .unwrap();
        let parent = Revision {
            commit_id: "parent".to_string(),
            repo_id: repo.id,
            parent: None,
            message: "parent".to_string(),
            author: "ns".to_string(),
            created_at: 1,
        };
        let parent_entries = vec![
            FileEntry {
                path: "keep.bin".to_string(),
                repo_id: repo.id,
                commit_id: parent.commit_id.clone(),
                size: 1,
                cas_hash: "keep-v1".to_string(),
                is_lfs: true,
            },
            FileEntry {
                path: "remove.bin".to_string(),
                repo_id: repo.id,
                commit_id: parent.commit_id.clone(),
                size: 2,
                cas_hash: "remove".to_string(),
                is_lfs: true,
            },
        ];
        store
            .commit_atomic(&parent, &parent_entries, None)
            .await
            .unwrap();

        let child = Revision {
            commit_id: "child".to_string(),
            repo_id: repo.id,
            parent: Some(parent.commit_id.clone()),
            message: "child".to_string(),
            author: "ns".to_string(),
            created_at: 2,
        };
        let changes = vec![
            FileTreeChange::Delete("remove.bin".to_string()),
            FileTreeChange::Upsert(FileEntry {
                path: "keep.bin".to_string(),
                repo_id: -1,
                commit_id: "ignored".to_string(),
                size: 3,
                cas_hash: "keep-v2".to_string(),
                is_lfs: false,
            }),
            FileTreeChange::Delete("keep.bin".to_string()),
            FileTreeChange::Upsert(FileEntry {
                path: "keep.bin".to_string(),
                repo_id: -1,
                commit_id: "ignored".to_string(),
                size: 4,
                cas_hash: "keep-final".to_string(),
                is_lfs: true,
            }),
        ];
        store
            .commit_changes_atomic(&child, &changes, Some("parent"))
            .await
            .unwrap();

        let parent_tree = store.get_file_tree(repo.id, "parent").await.unwrap();
        assert_eq!(parent_tree.len(), 2);
        let child_tree = store.get_file_tree(repo.id, "child").await.unwrap();
        assert_eq!(child_tree.len(), 1);
        assert_eq!(child_tree[0].path, "keep.bin");
        assert_eq!(child_tree[0].repo_id, repo.id);
        assert_eq!(child_tree[0].commit_id, "child");
        assert_eq!(child_tree[0].size, 4);
        assert_eq!(child_tree[0].cas_hash, "keep-final");
        assert_eq!(
            store.get_head(repo.id).await.unwrap().as_deref(),
            Some("child")
        );
    }

    #[tokio::test]
    async fn commit_changes_failure_rolls_back_copied_snapshot_revision_and_head() {
        let store = SqliteMetadataStore::in_memory().await.unwrap();
        let repo = store
            .create_repo("ns", "delta-rollback", RepoType::Model, false)
            .await
            .unwrap();
        let parent = Revision {
            commit_id: "parent".to_string(),
            repo_id: repo.id,
            parent: None,
            message: "parent".to_string(),
            author: "ns".to_string(),
            created_at: 1,
        };
        let parent_entry = FileEntry {
            path: "keep.bin".to_string(),
            repo_id: repo.id,
            commit_id: parent.commit_id.clone(),
            size: 1,
            cas_hash: "keep".to_string(),
            is_lfs: true,
        };
        store
            .commit_atomic(&parent, &[parent_entry], None)
            .await
            .unwrap();

        let child = Revision {
            commit_id: "child".to_string(),
            repo_id: repo.id,
            parent: Some(parent.commit_id.clone()),
            message: "child".to_string(),
            author: "ns".to_string(),
            created_at: 2,
        };
        let changes = [FileTreeChange::Upsert(FileEntry {
            path: "too-large.bin".to_string(),
            repo_id: repo.id,
            commit_id: child.commit_id.clone(),
            size: u64::MAX,
            cas_hash: "large".to_string(),
            is_lfs: true,
        })];
        let error = store
            .commit_changes_atomic(&child, &changes, Some("parent"))
            .await
            .expect_err("invalid delta must roll back the complete commit transaction");
        assert!(matches!(error, MetadataError::InvalidOperation(_)));

        assert_eq!(
            store.get_head(repo.id).await.unwrap().as_deref(),
            Some("parent")
        );
        assert!(matches!(
            store.get_revision(repo.id, "child").await,
            Err(MetadataError::RevisionNotFound(_))
        ));
        assert!(
            store
                .get_file_tree(repo.id, "child")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.get_file_tree(repo.id, "parent").await.unwrap().len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_commit_rolls_back_before_connection_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("cancelled-commit.db");
        let store = std::sync::Arc::new(
            SqliteMetadataStore::new(database_path.to_str().unwrap(), 1)
                .await
                .unwrap(),
        );
        let repo = store
            .create_repo("ns", "cancelled", RepoType::Model, false)
            .await
            .unwrap();

        sqlx::query(
            "CREATE TRIGGER slow_file_insert BEFORE INSERT ON file_tree BEGIN \
             SELECT length(randomblob(64000000)); END",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        let revision = Revision {
            commit_id: "cancelled-commit".to_string(),
            repo_id: repo.id,
            parent: None,
            message: "cancelled".to_string(),
            author: "ns".to_string(),
            created_at: 1,
        };
        let entries = vec![FileEntry {
            path: "slow.bin".to_string(),
            repo_id: repo.id,
            commit_id: revision.commit_id.clone(),
            size: 1,
            cas_hash: "hash".to_string(),
            is_lfs: false,
        }];

        let task_store = store.clone();
        let task =
            tokio::spawn(async move { task_store.commit_atomic(&revision, &entries, None).await });

        while store.pool.num_idle() != 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(
            !task.is_finished(),
            "test trigger must keep the commit in flight"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        sqlx::query("DROP TRIGGER slow_file_insert")
            .execute(&store.pool)
            .await
            .unwrap();

        let replacement = Revision {
            commit_id: "replacement-commit".to_string(),
            repo_id: repo.id,
            parent: None,
            message: "replacement".to_string(),
            author: "ns".to_string(),
            created_at: 2,
        };
        store
            .commit_atomic(&replacement, &[], None)
            .await
            .expect("a cancelled commit must not poison the pooled connection");

        assert_eq!(
            store.get_head(repo.id).await.unwrap().as_deref(),
            Some("replacement-commit")
        );
        assert!(
            store
                .get_revision(repo.id, "cancelled-commit")
                .await
                .is_err()
        );
    }
}
