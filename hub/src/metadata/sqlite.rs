use super::{FileEntry, MetadataError, MetadataStore, Repo, RepoType, Revision};
use async_trait::async_trait;
use rusqlite::params;
use std::sync::Mutex;

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

/// SQLite-based metadata store
///
/// Note: Uses `Mutex<Connection>` which serializes all database operations.
/// WAL mode (enabled in `new()`) allows concurrent readers, but writes still
/// acquire exclusive locks. For higher concurrency under heavy write loads,
/// consider migrating to a connection pool with separate read-only connections.
pub struct SqliteMetadataStore {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteMetadataStore {
    /// Create a new SQLite metadata store
    pub fn new(path: &str) -> Result<Self, MetadataError> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // I6: Enable foreign key constraints
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // I2: Enable WAL mode for better read/write concurrency.
        // SQLite WAL mode allows concurrent readers with a single writer,
        // reducing lock contention compared to the default journal mode.
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // Create tables
        conn.execute_batch(SCHEMA)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // Initialize schema version if not present
        let version_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schema_version",
            [],
            |row| row.get(0),
        ).unwrap_or(0);

        if version_count == 0 {
            conn.execute("INSERT INTO schema_version (version) VALUES (?1)", params![SCHEMA_VERSION])
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an in-memory SQLite metadata store.
    ///
    /// **Warning:** This is intended for testing only. In-memory stores do not persist
    /// data across restarts and should not be used in production deployments.
    pub fn in_memory() -> Result<Self, MetadataError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // I6: Enable foreign key constraints
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // I2: Enable WAL mode for consistency with production path
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        conn.execute_batch(SCHEMA)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
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
        let namespace = namespace.to_string();
        let name = name.to_string();
        let repo_type_str = repo_type.to_string();
        let private_int = if private { 1 } else { 0 };
        let now = chrono_timestamp();

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let result = conn.execute(
            "INSERT INTO repos (name, namespace, repo_type, private, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![name, namespace, repo_type_str, private_int, now, now],
        );

        match result {
            Ok(_) => {
                let id = conn.last_insert_rowid();
                Ok(Repo {
                    id,
                    name,
                    namespace,
                    repo_type,
                    sha: None,
                    private,
                    created_at: now,
                    updated_at: now,
                })
            }
            Err(rusqlite::Error::SqliteFailure(err, _)) => {
                if err.code == rusqlite::ErrorCode::ConstraintViolation {
                    Err(MetadataError::RepoAlreadyExists(format!(
                        "{}/{}/{}",
                        namespace, name, repo_type_str
                    )))
                } else {
                    Err(MetadataError::DatabaseError(err.to_string()))
                }
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
        let namespace = namespace.to_string();
        let name = name.to_string();
        let repo_type_str = repo_type.to_string();

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let result = conn.query_row(
            "SELECT id, name, namespace, repo_type, sha, private, created_at, updated_at FROM repos WHERE namespace = ?1 AND name = ?2 AND repo_type = ?3",
            params![namespace, name, repo_type_str],
            |row| {
                let repo_type_str: String = row.get(3)?;
                let repo_type = repo_type_str.parse().map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        3,
                        "repo_type".to_string(),
                        rusqlite::types::Type::Text,
                    )
                })?;
                Ok(Repo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    namespace: row.get(2)?,
                    repo_type,
                    sha: row.get(4)?,
                    private: row.get::<_, i64>(5)? != 0,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            },
        );

        match result {
            Ok(repo) => Ok(repo),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(MetadataError::RepoNotFound(
                format!("{}/{}/{}", namespace, name, repo_type_str),
            )),
            Err(e) => Err(MetadataError::DatabaseError(e.to_string())),
        }
    }

    /// Delete a repository and all its metadata (file_tree, heads, revisions, repo record)
    ///
    /// **Known tradeoff:** This does NOT delete associated blobs from CAS. Since blobs are
    /// content-addressed and deduplicated, orphaned blobs don't affect correctness. A background
    /// GC job could clean up orphaned blobs in the future if storage efficiency becomes a concern.
    async fn delete_repo(&self, repo_id: i64) -> Result<(), MetadataError> {
        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        // I5: Wrap deletion in a transaction for atomicity
        conn.execute("BEGIN", [])
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let result = (|| -> Result<(), MetadataError> {
            // Delete in order (foreign key constraints)
            conn.execute("DELETE FROM file_tree WHERE repo_id = ?1", params![repo_id])
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            conn.execute("DELETE FROM heads WHERE repo_id = ?1", params![repo_id])
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            conn.execute("DELETE FROM revisions WHERE repo_id = ?1", params![repo_id])
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            let rows = conn
                .execute("DELETE FROM repos WHERE id = ?1", params![repo_id])
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            if rows == 0 {
                Err(MetadataError::RepoNotFound(format!("id={}", repo_id)))
            } else {
                Ok(())
            }
        })();

        match result {
            Ok(()) => {
                conn.execute("COMMIT", []).ok();
                Ok(())
            }
            Err(e) => {
                conn.execute("ROLLBACK", []).ok();
                Err(e)
            }
        }
    }

    async fn add_revision(&self, revision: Revision) -> Result<(), MetadataError> {
        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        conn.execute(
            "INSERT INTO revisions (commit_id, repo_id, parent, message, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                revision.commit_id,
                revision.repo_id,
                revision.parent,
                revision.message,
                revision.author,
                revision.created_at,
            ],
        )
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(())
    }

    async fn get_revision(
        &self,
        repo_id: i64,
        commit_id: &str,
    ) -> Result<Revision, MetadataError> {
        let commit_id = commit_id.to_string();

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let result = conn.query_row(
            "SELECT commit_id, repo_id, parent, message, author, created_at FROM revisions WHERE repo_id = ?1 AND commit_id = ?2",
            params![repo_id, commit_id],
            |row| {
                Ok(Revision {
                    commit_id: row.get(0)?,
                    repo_id: row.get(1)?,
                    parent: row.get(2)?,
                    message: row.get(3)?,
                    author: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        );

        match result {
            Ok(revision) => Ok(revision),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Err(MetadataError::RevisionNotFound(commit_id))
            }
            Err(e) => Err(MetadataError::DatabaseError(e.to_string())),
        }
    }

    async fn get_head(&self, repo_id: i64) -> Result<Option<String>, MetadataError> {
        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let result = conn.query_row(
            "SELECT commit_id FROM heads WHERE repo_id = ?1",
            params![repo_id],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(commit_id) => Ok(Some(commit_id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MetadataError::DatabaseError(e.to_string())),
        }
    }

    async fn set_head(&self, repo_id: i64, commit_id: &str) -> Result<(), MetadataError> {
        let commit_id = commit_id.to_string();

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        conn.execute(
            "INSERT OR REPLACE INTO heads (repo_id, commit_id) VALUES (?1, ?2)",
            params![repo_id, commit_id],
        )
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(())
    }

    async fn get_commit_log(
        &self,
        repo_id: i64,
        limit: Option<usize>,
    ) -> Result<Vec<Revision>, MetadataError> {
        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        // Get HEAD first
        let head_result = conn.query_row(
            "SELECT commit_id FROM heads WHERE repo_id = ?1",
            params![repo_id],
            |row| row.get::<_, String>(0),
        );

        let head = match head_result {
            Ok(h) => h,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
            Err(e) => return Err(MetadataError::DatabaseError(e.to_string())),
        };

        // Walk the parent chain
        let mut log = Vec::new();
        let mut current = Some(head);
        let limit = limit.unwrap_or(usize::MAX);

        while let Some(commit_id) = current {
            if log.len() >= limit {
                break;
            }

            let result = conn.query_row(
                "SELECT commit_id, repo_id, parent, message, author, created_at FROM revisions WHERE commit_id = ?1",
                params![commit_id],
                |row| {
                    Ok(Revision {
                        commit_id: row.get(0)?,
                        repo_id: row.get(1)?,
                        parent: row.get(2)?,
                        message: row.get(3)?,
                        author: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                },
            );

            match result {
                Ok(revision) => {
                    current = revision.parent.clone();
                    log.push(revision);
                }
                Err(_) => break, // Stop if revision not found
            }
        }

        Ok(log)
    }

    async fn add_file_entries(&self, entries: Vec<FileEntry>) -> Result<(), MetadataError> {
        let mut conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let tx = conn
            .transaction()
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        {
            for entry in entries {
                let is_lfs_int = if entry.is_lfs { 1 } else { 0 };
                tx.execute(
                    "INSERT OR REPLACE INTO file_tree (path, repo_id, commit_id, size, cas_hash, is_lfs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![entry.path, entry.repo_id, entry.commit_id, entry.size as i64, entry.cas_hash, is_lfs_int],
                )
                .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
            }
        }

        tx.commit()
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(())
    }

    async fn get_file_tree(
        &self,
        repo_id: i64,
        commit_id: &str,
    ) -> Result<Vec<FileEntry>, MetadataError> {
        let commit_id = commit_id.to_string();

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let mut stmt = conn
            .prepare(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 ORDER BY path",
            )
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let entries = stmt
            .query_map(params![repo_id, commit_id], |row| {
                Ok(FileEntry {
                    path: row.get(0)?,
                    repo_id: row.get(1)?,
                    commit_id: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    cas_hash: row.get(4)?,
                    is_lfs: row.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(entries)
    }

    async fn get_file_tree_prefix(
        &self,
        repo_id: i64,
        commit_id: &str,
        prefix: &str,
    ) -> Result<Vec<FileEntry>, MetadataError> {
        let commit_id = commit_id.to_string();
        // I10: Escape SQL LIKE metacharacters to prevent logic bugs
        let escaped_prefix = prefix.replace('%', "\\%").replace('_', "\\_");
        let prefix_pattern = format!("{}%", escaped_prefix);

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let mut stmt = conn
            .prepare(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 AND path LIKE ?3 ESCAPE '\\' ORDER BY path",
            )
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let entries = stmt
            .query_map(params![repo_id, commit_id, prefix_pattern], |row| {
                Ok(FileEntry {
                    path: row.get(0)?,
                    repo_id: row.get(1)?,
                    commit_id: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    cas_hash: row.get(4)?,
                    is_lfs: row.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(entries)
    }

    async fn resolve_file(
        &self,
        repo_id: i64,
        commit_id: &str,
        path: &str,
    ) -> Result<FileEntry, MetadataError> {
        let commit_id = commit_id.to_string();
        let path = path.to_string();

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let result = conn.query_row(
            "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 AND path = ?3",
            params![repo_id, commit_id, path],
            |row| {
                Ok(FileEntry {
                    path: row.get(0)?,
                    repo_id: row.get(1)?,
                    commit_id: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    cas_hash: row.get(4)?,
                    is_lfs: row.get::<_, i64>(5)? != 0,
                })
            },
        );

        match result {
            Ok(entry) => Ok(entry),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Err(MetadataError::FileNotFound(format!(
                    "{}/{}",
                    commit_id, path
                )))
            }
            Err(e) => Err(MetadataError::DatabaseError(e.to_string())),
        }
    }

    async fn commit_atomic(
        &self,
        rev: &Revision,
        entries: &[FileEntry],
        expected_parent: Option<&str>,
    ) -> Result<(), MetadataError> {
        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        // Use IMMEDIATE to acquire write lock upfront (SQLite only supports one writer)
        conn.execute("BEGIN IMMEDIATE", [])
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let result = (|| -> Result<(), MetadataError> {
            // Authoritative HEAD check (I3): this is the race-safe check under
            // BEGIN IMMEDIATE lock. The handler also does a pre-check for better
            // error messages, but this one is the source of truth.
            let current_head: Option<String> = conn
                .query_row(
                    "SELECT commit_id FROM heads WHERE repo_id = ?1",
                    params![rev.repo_id],
                    |row| row.get(0),
                )
                .ok();

            if current_head.as_deref() != expected_parent {
                return Err(MetadataError::Conflict(
                    current_head.unwrap_or_default(),
                ));
            }

            // Insert revision
            conn.execute(
                "INSERT INTO revisions (commit_id, repo_id, parent, message, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    rev.commit_id,
                    rev.repo_id,
                    rev.parent,
                    rev.message,
                    rev.author,
                    rev.created_at,
                ],
            )
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            // Insert file entries
            if !entries.is_empty() {
                let mut stmt = conn
                    .prepare(
                        "INSERT OR REPLACE INTO file_tree (path, repo_id, commit_id, size, cas_hash, is_lfs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                for entry in entries {
                    let is_lfs_int = if entry.is_lfs { 1 } else { 0 };
                    stmt.execute(params![
                        entry.path,
                        entry.repo_id,
                        entry.commit_id,
                        entry.size as i64,
                        entry.cas_hash,
                        is_lfs_int
                    ])
                    .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;
                }
            }

            // Set HEAD
            conn.execute(
                "INSERT OR REPLACE INTO heads (repo_id, commit_id) VALUES (?1, ?2)",
                params![rev.repo_id, rev.commit_id],
            )
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.execute("COMMIT", []).ok();
                Ok(())
            }
            Err(e) => {
                conn.execute("ROLLBACK", []).ok();
                Err(e)
            }
        }
    }

    async fn get_all_referenced_hashes(&self) -> Result<std::collections::HashSet<String>, MetadataError> {
        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let mut stmt = conn
            .prepare("SELECT DISTINCT cas_hash FROM file_tree WHERE is_lfs = 1")
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        let hashes: std::collections::HashSet<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

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