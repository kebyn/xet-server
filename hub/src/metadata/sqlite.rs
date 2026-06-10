use super::{FileEntry, MetadataError, MetadataStore, Repo, RepoType, Revision};
use async_trait::async_trait;
use rusqlite::params;
use std::sync::Mutex;

/// SQLite-based metadata store
pub struct SqliteMetadataStore {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteMetadataStore {
    /// Create a new SQLite metadata store
    pub fn new(path: &str) -> Result<Self, MetadataError> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        // Create tables
        conn.execute_batch(
            r#"
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
            "#,
        )
        .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an in-memory SQLite metadata store (for testing)
    pub fn in_memory() -> Result<Self, MetadataError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| MetadataError::DatabaseError(e.to_string()))?;

        conn.execute_batch(
            r#"
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
            "#,
        )
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

    async fn delete_repo(&self, repo_id: i64) -> Result<(), MetadataError> {
        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

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
        let prefix_pattern = format!("{}%", prefix);

        let conn = self.conn.lock().map_err(|e| {
            MetadataError::DatabaseError(format!("Failed to acquire lock: {}", e))
        })?;

        let mut stmt = conn
            .prepare(
                "SELECT path, repo_id, commit_id, size, cas_hash, is_lfs FROM file_tree WHERE repo_id = ?1 AND commit_id = ?2 AND path LIKE ?3 ORDER BY path",
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
}

/// Get current Unix timestamp in seconds
fn chrono_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}