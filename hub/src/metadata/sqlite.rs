//! SQLx-based SQLite metadata store
//!
//! Async SQLite implementation using sqlx for true async database operations.
//! Migrated from rusqlite to prevent blocking the async runtime.

use super::{FileEntry, MetadataError, MetadataStore, Repo, RepoType, Revision};
use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;

// Schema version tracking for future migrations
// Current version: 1 (initial schema)
const SCHEMA_VERSION: i64 = 1;

// I15: Extract schema to constant to eliminate duplication
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    namespace TEXT NOT NULL,
    repo_type TEXT NOT NULL,
    sha TEXT,
    private INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(namespace, name, repo_type)
);

CREATE TABLE IF NOT EXISTS revisions (
    commit_id TEXT PRIMARY KEY,
    repo_id INTEGER NOT NULL,
    parent TEXT,
    message TEXT NOT NULL,
    author TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (repo_id) REFERENCES repos(id)
);

CREATE TABLE IF NOT EXISTS heads (
    repo_id INTEGER PRIMARY KEY,
    commit_id TEXT NOT NULL,
    FOREIGN KEY (repo_id) REFERENCES repos(id),
    FOREIGN KEY (commit_id) REFERENCES revisions(commit_id)
);

CREATE TABLE IF NOT EXISTS file_tree (
    path TEXT NOT NULL,
    repo_id INTEGER NOT NULL,
    commit_id TEXT NOT NULL,
    size INTEGER NOT NULL,
    cas_hash TEXT NOT NULL,
    is_lfs INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (path, repo_id, commit_id),
    FOREIGN KEY (repo_id) REFERENCES repos(id),
    FOREIGN KEY (commit_id) REFERENCES revisions(commit_id)
);

CREATE INDEX IF NOT EXISTS idx_file_tree_prefix ON file_tree(repo_id, commit_id, path);
-- M5: Index for GC referenced hash queries (SELECT DISTINCT cas_hash WHERE is_lfs = 1)
CREATE INDEX IF NOT EXISTS idx_file_tree_cas_hash ON file_tree(is_lfs, cas_hash);
"#;

/// Async SQLite-based metadata store using sqlx connection pool
///
/// Uses SqlitePool for true async operations with connection pooling.
/// WAL mode is enabled for better read/write concurrency.
pub struct SqliteMetadataStore {
    pool: SqlitePool,
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
    let repo_type_str: String = row.try_get(3)
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
    let repo_type = repo_type_str.parse::<RepoType>()
        .map_err(|e| MetadataError::DatabaseError(format!("Invalid repo_type: {}", e)))?;
    Ok(Repo {
        id: row.try_get(0).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        name: row.try_get(1).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        namespace: row.try_get(2).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        repo_type,
        sha: row.try_get(4).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        private: row.try_get::<i64, _>(5).map_err(|e| MetadataError::DatabaseError(e.to_string()))? != 0,
        created_at: row.try_get(6).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        updated_at: row.try_get(7).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
    })
}

/// Map a `sqlx::Row` to a `Revision` value.
fn row_to_revision(row: &sqlx::sqlite::SqliteRow) -> Result<Revision, MetadataError> {
    Ok(Revision {
        commit_id: row.try_get(0).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        repo_id: row.try_get(1).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        parent: row.try_get(2).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        message: row.try_get(3).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        author: row.try_get(4).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        created_at: row.try_get(5).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
    })
}

/// Map a `sqlx::Row` to a `FileEntry` value.
fn row_to_file_entry(row: &sqlx::sqlite::SqliteRow) -> Result<FileEntry, MetadataError> {
    Ok(FileEntry {
        path: row.try_get(0).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        repo_id: row.try_get(1).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        commit_id: row.try_get(2).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        size: row.try_get::<i64, _>(3).map_err(|e| MetadataError::DatabaseError(e.to_string()))? as u64,
        cas_hash: row.try_get(4).map_err(|e| MetadataError::DatabaseError(e.to_string()))?,
        is_lfs: row.try_get::<i64, _>(5).map_err(|e| MetadataError::DatabaseError(e.to_string()))? != 0,
    })
}

impl SqliteMetadataStore {
    /// Create a new SQLite metadata store with connection pool
    pub async fn new(path: &str, pool_size: u32) -> Result<Self, MetadataError> {
        // S1 FIX: Use after_connect to ensure PRAGMA settings persist across connection pool recycling.
        // PRAGMA settings are connection-level, not database-level, so they must be set on each new connection.
        let pool = SqlitePoolOptions::new()
            .max_connections(pool_size)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .after_connect(|conn, _| Box::pin(async move {
                sqlx::query("PRAGMA journal_mode = WAL;").execute(&mut *conn).await?;
                sqlx::query("PRAGMA foreign_keys = ON;").execute(&mut *conn).await?;
                // M4 fix: Wait up to 5 seconds for lock release instead of failing immediately
                // with SQLITE_BUSY. Critical for concurrent write workloads.
                sqlx::query("PRAGMA busy_timeout = 5000;").execute(&mut *conn).await?;
                Ok(())
            }))
            .connect(path)
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
        // max_connections(1) with file::memory:?cache=shared to ensure all
        // pool connections share the same in-memory database.
        // S1 FIX: Use after_connect to ensure PRAGMA settings persist.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _| Box::pin(async move {
                sqlx::query("PRAGMA foreign_keys = ON;").execute(&mut *conn).await?;
                Ok(())
            }))
            .connect("sqlite::memory:")
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Self::init_pool(&pool).await?;

        Ok(Self { pool })
    }

    /// Common pool initialization: create schema, set version.
    /// Note: PRAGMA settings (journal_mode, foreign_keys) are set in after_connect callback
    /// to ensure they persist across connection pool recycling (S1 fix).
    async fn init_pool(pool: &SqlitePool) -> Result<(), MetadataError> {
        // Create tables
        sqlx::query(SCHEMA)
            .execute(pool)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // Initialize schema version if not present
        let version_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM schema_version")
            .fetch_optional(pool)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
            .unwrap_or((0,));

        if version_count.0 == 0 {
            sqlx::query("INSERT INTO schema_version (version) VALUES (?1)")
                .bind(SCHEMA_VERSION)
                .execute(pool)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        }

        Ok(())
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
        let repo_type_str = repo_type.to_string();
        let private_int: i64 = if private { 1 } else { 0 };
        let now = chrono_timestamp();

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
            Err(e) if is_unique_violation(&e) => {
                Err(MetadataError::RepoAlreadyExists(format!(
                    "{}/{}/{}", namespace, name, repo_type_str
                )))
            }
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
                "{}/{}/{}", namespace, name, repo_type_str
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
        let mut tx = self.pool.begin().await
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
        }.await;

        match result {
            Ok(()) => {
                tx.commit().await
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

    async fn get_revision(
        &self,
        repo_id: i64,
        commit_id: &str,
    ) -> Result<Revision, MetadataError> {
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
            Some(r) => Ok(Some(r.try_get(0).map_err(|e| MetadataError::DatabaseError(e.to_string()))?)),
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
        let effective_limit = limit
            .map(|l| l as i64)
            .unwrap_or(i64::MAX - 1);

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
        let mut tx = self.pool.begin().await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        for entry in &entries {
            let is_lfs_int: i64 = if entry.is_lfs { 1 } else { 0 };
            sqlx::query(
                "INSERT OR REPLACE INTO file_tree (path, repo_id, commit_id, size, cas_hash, is_lfs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            )
            .bind(&entry.path)
            .bind(entry.repo_id)
            .bind(&entry.commit_id)
            .bind(entry.size as i64)
            .bind(&entry.cas_hash)
            .bind(is_lfs_int)
            .execute(&mut *tx)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        }

        tx.commit().await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

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
        // I10: Escape SQL LIKE metacharacters to prevent logic bugs
        let escaped_prefix = prefix.replace('%', "\\%").replace('_', "\\_");
        let prefix_pattern = format!("{}%", escaped_prefix);

        let rows = sqlx::query(
            "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 AND path LIKE ?3 ESCAPE '\\' ORDER BY path"
        )
        .bind(repo_id)
        .bind(commit_id)
        .bind(&prefix_pattern)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        rows.iter().map(row_to_file_entry).collect()
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
                "{}/{}", commit_id, path
            ))),
        }
    }

    async fn commit_atomic(
        &self,
        rev: &Revision,
        entries: &[FileEntry],
        expected_parent: Option<&str>,
    ) -> Result<(), MetadataError> {
        // S3 NOTE: Manual transaction control is necessary for BEGIN IMMEDIATE,
        // which sqlx's Transaction API doesn't directly support.
        // Tradeoff: If panic occurs between BEGIN and COMMIT/ROLLBACK, the connection
        // may return to pool with an open transaction. However, sqlx will discard
        // connections that error, and SQLite will auto-rollback when connection closes.
        let mut conn = self.pool.acquire().await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // Use IMMEDIATE to acquire write lock upfront (SQLite only supports one writer)
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let result = async {
            // Authoritative HEAD check (I3): this is the race-safe check under
            // BEGIN IMMEDIATE lock. The handler also does a pre-check for better
            // error messages, but this one is the source of truth.
            let current_head: Option<String> = sqlx::query(
                "SELECT commit_id FROM heads WHERE repo_id = ?1"
            )
            .bind(rev.repo_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
            .map(|r| r.try_get::<String, _>(0))
            .transpose()
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            if current_head.as_deref() != expected_parent {
                return Err(MetadataError::Conflict(
                    current_head.unwrap_or_default(),
                ));
            }

            // Insert revision
            sqlx::query(
                "INSERT INTO revisions (commit_id, repo_id, parent, message, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            )
            .bind(&rev.commit_id)
            .bind(rev.repo_id)
            .bind(&rev.parent)
            .bind(&rev.message)
            .bind(&rev.author)
            .bind(rev.created_at)
            .execute(&mut *conn)
            .await
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            // Insert file entries
            for entry in entries {
                let is_lfs_int: i64 = if entry.is_lfs { 1 } else { 0 };
                sqlx::query(
                    "INSERT OR REPLACE INTO file_tree (path, repo_id, commit_id, size, cas_hash, is_lfs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                )
                .bind(&entry.path)
                .bind(entry.repo_id)
                .bind(&entry.commit_id)
                .bind(entry.size as i64)
                .bind(&entry.cas_hash)
                .bind(is_lfs_int)
                .execute(&mut *conn)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
            }

            // Set HEAD
            sqlx::query("INSERT OR REPLACE INTO heads (repo_id, commit_id) VALUES (?1, ?2)")
                .bind(rev.repo_id)
                .bind(&rev.commit_id)
                .execute(&mut *conn)
                .await
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            Ok::<(), MetadataError>(())
        }.await;

        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                Ok(())
            }
            Err(e) => {
                // Explicit ROLLBACK before returning error
                sqlx::query("ROLLBACK").execute(&mut *conn).await.ok();
                Err(e)
            }
        }
    }

    async fn get_all_referenced_hashes(&self) -> Result<std::collections::HashSet<String>, MetadataError> {
        // C2 fix: Use streaming query instead of fetch_all to avoid OOM on large datasets.
        // fetch() returns a Stream that processes rows one at a time, keeping memory
        // usage bounded to O(1) for the query itself (though we still accumulate into HashSet).
        use futures_util::StreamExt;
        use sqlx::Executor;

        let mut hashes = std::collections::HashSet::new();
        let mut stream = self.pool.fetch(
            sqlx::query("SELECT DISTINCT cas_hash FROM file_tree WHERE is_lfs = 1")
        );

        while let Some(row_result) = stream.next().await {
            let row = row_result.map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
            let hash: String = row.try_get(0)
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
            hashes.insert(hash);
        }

        Ok(hashes)
    }
}

/// Get current Unix timestamp in seconds
fn chrono_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
