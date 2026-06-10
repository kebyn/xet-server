//! SQLite implementation of the StorageStateManager trait.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusqlite::Connection;

use super::{FileState, StateError, StorageState, StorageStateManager};

/// SQLite-based state manager for blob storage.
pub struct SqliteStateManager {
    conn: Mutex<Connection>,
}

impl SqliteStateManager {
    /// Create a new SQLite state manager at the given path.
    ///
    /// This will create the database file and tables if they don't exist.
    pub fn new(path: &str) -> Result<Self, StateError> {
        let conn = Connection::open(path).map_err(|e| StateError::Database(e.to_string()))?;

        // I6: Enable foreign key constraints
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| StateError::Database(e.to_string()))?;

        // Create tables
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS file_states (
                oid          TEXT PRIMARY KEY,
                state        TEXT NOT NULL,       -- "raw_only" | "xet_only"
                xet_file_id  TEXT,
                size         INTEGER NOT NULL,
                sha256       TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                converted_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS conversion_locks (
                oid        TEXT PRIMARY KEY,
                locked_at  INTEGER NOT NULL,
                locked_by  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS xorb_refs (
                xorb_hash  TEXT NOT NULL,
                shard_id   TEXT NOT NULL,
                PRIMARY KEY (xorb_hash, shard_id)
            );
            "#,
        )
        .map_err(|e| StateError::Database(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an in-memory SQLite state manager (useful for testing).
    pub fn new_in_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory().map_err(|e| StateError::Database(e.to_string()))?;

        // I6: Enable foreign key constraints
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| StateError::Database(e.to_string()))?;

        // Create tables
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS file_states (
                oid          TEXT PRIMARY KEY,
                state        TEXT NOT NULL,       -- "raw_only" | "xet_only"
                xet_file_id  TEXT,
                size         INTEGER NOT NULL,
                sha256       TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                converted_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS conversion_locks (
                oid        TEXT PRIMARY KEY,
                locked_at  INTEGER NOT NULL,
                locked_by  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS xorb_refs (
                xorb_hash  TEXT NOT NULL,
                shard_id   TEXT NOT NULL,
                PRIMARY KEY (xorb_hash, shard_id)
            );
            "#,
        )
        .map_err(|e| StateError::Database(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Get the current Unix timestamp in seconds.
    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}

#[async_trait]
impl StorageStateManager for SqliteStateManager {
    async fn get_state(&self, oid: &str) -> Result<Option<FileState>, StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT state, xet_file_id, size, sha256, created_at, converted_at
                 FROM file_states WHERE oid = ?",
            )
            .map_err(|e| StateError::Database(e.to_string()))?;

        let result = stmt.query_row([oid], |row| {
            let state_str: String = row.get(0)?;
            let state = match state_str.as_str() {
                "raw_only" => StorageState::RawOnly,
                "xet_only" => StorageState::XetOnly,
                _ => return Err(rusqlite::Error::InvalidQuery),
            };

            Ok(FileState {
                state,
                xet_file_id: row.get(1)?,
                size: row.get(2)?,
                sha256: row.get(3)?,
                created_at: row.get(4)?,
                converted_at: row.get(5)?,
            })
        });

        match result {
            Ok(state) => Ok(Some(state)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StateError::Database(e.to_string())),
        }
    }

    async fn register_raw_blob(&self, oid: &str, size: u64) -> Result<(), StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let timestamp = Self::current_timestamp();

        // Use INSERT OR IGNORE for idempotency - sha256 is the same as oid
        conn.execute(
            "INSERT OR IGNORE INTO file_states (oid, state, xet_file_id, size, sha256, created_at, converted_at)
             VALUES (?, 'raw_only', NULL, ?, ?, ?, NULL)",
            (oid, size, oid, timestamp),
        )
        .map_err(|e| StateError::Database(e.to_string()))?;

        Ok(())
    }

    async fn register_xet_only(&self, oid: &str, file_id: &str, size: u64) -> Result<(), StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let timestamp = Self::current_timestamp();

        // Use INSERT OR REPLACE to ensure xet_only state
        conn.execute(
            "INSERT OR REPLACE INTO file_states (oid, state, xet_file_id, size, sha256, created_at, converted_at)
             VALUES (?, 'xet_only', ?, ?, ?, ?, ?)",
            (oid, file_id, size, oid, timestamp, timestamp),
        )
        .map_err(|e| StateError::Database(e.to_string()))?;

        Ok(())
    }

    async fn mark_converted(&self, oid: &str, file_id: &str) -> Result<(), StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;
        let timestamp = Self::current_timestamp();

        let rows_affected = conn
            .execute(
                "UPDATE file_states SET state = 'xet_only', xet_file_id = ?, converted_at = ? WHERE oid = ?",
                (file_id, timestamp, oid),
            )
            .map_err(|e| StateError::Database(e.to_string()))?;

        if rows_affected == 0 {
            return Err(StateError::Database(format!("OID not found: {}", oid)));
        }

        Ok(())
    }

    async fn get_states(&self, oids: &[String]) -> Result<Vec<(String, Option<FileState>)>, StateError> {
        let conn = self.conn.lock().map_err(|e| StateError::Internal(e.to_string()))?;

        let mut results = Vec::with_capacity(oids.len());

        for oid in oids {
            let mut stmt = conn
                .prepare(
                    "SELECT state, xet_file_id, size, sha256, created_at, converted_at
                     FROM file_states WHERE oid = ?",
                )
                .map_err(|e| StateError::Database(e.to_string()))?;

            let result = stmt.query_row([oid], |row| {
                let state_str: String = row.get(0)?;
                let state = match state_str.as_str() {
                    "raw_only" => StorageState::RawOnly,
                    "xet_only" => StorageState::XetOnly,
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };

                Ok(FileState {
                    state,
                    xet_file_id: row.get(1)?,
                    size: row.get(2)?,
                    sha256: row.get(3)?,
                    created_at: row.get(4)?,
                    converted_at: row.get(5)?,
                })
            });

            let state = match result {
                Ok(s) => Some(s),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(StateError::Database(e.to_string())),
            };

            results.push((oid.clone(), state));
        }

        Ok(results)
    }
}