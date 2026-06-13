//! Metadata Index Manager
//!
//! Manages the index mappings for file reconstruction and global deduplication:
//! - file_hash → shard_id mapping
//! - chunk_hash → (xorb_hash, chunk_index) mapping for global dedup
//!
//! I5: Optional persistence to SQLite for faster startup.
//! Without persistence, the index is rebuilt from storage on each startup
//! which requires fetching and parsing all shards.

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;

/// Metadata index for managing file-to-shard and chunk-to-xorb mappings
#[derive(Debug, Clone)]
pub struct MetadataIndex {
    /// Map from file hash to shard IDs that contain this file's reconstruction info
    file_to_shards: Arc<RwLock<HashMap<String, Vec<String>>>>,

    /// Map from chunk hash to (xorb_hash, chunk_index) for global deduplication
    chunk_to_xorb: Arc<RwLock<HashMap<String, (String, u32)>>>,

    /// I5: Optional SQLite connection for persisting the index
    db: Option<Arc<std::sync::Mutex<rusqlite::Connection>>>,
}

impl MetadataIndex {
    /// Create a new empty metadata index (in-memory only)
    pub fn new() -> Self {
        Self {
            file_to_shards: Arc::new(RwLock::new(HashMap::new())),
            chunk_to_xorb: Arc::new(RwLock::new(HashMap::new())),
            db: None,
        }
    }

    /// I5: Create a metadata index with SQLite persistence.
    /// Loads existing index from disk if available, otherwise starts empty.
    /// The index will be persisted to disk on each register_shard() call.
    pub fn with_persistence(db_path: &str) -> Result<Self, String> {
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| format!("Failed to open index DB: {}", e))?;

        // Create tables if they don't exist
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS index_files (
                file_hash TEXT NOT NULL,
                shard_id TEXT NOT NULL,
                PRIMARY KEY (file_hash, shard_id)
            );
            CREATE TABLE IF NOT EXISTS index_chunks (
                chunk_hash TEXT PRIMARY KEY,
                xorb_hash TEXT NOT NULL,
                chunk_index INTEGER NOT NULL
            );"
        ).map_err(|e| format!("Failed to create index tables: {}", e))?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("Failed to enable WAL mode: {}", e))?;

        let db = Arc::new(std::sync::Mutex::new(conn));

        // Load existing index from disk
        let mut index = Self {
            file_to_shards: Arc::new(RwLock::new(HashMap::new())),
            chunk_to_xorb: Arc::new(RwLock::new(HashMap::new())),
            db: Some(db.clone()),
        };

        index.load_from_db()?;

        Ok(index)
    }

    /// I5: Load index data from SQLite database into memory
    fn load_from_db(&mut self) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database connection")?;
        let conn = db.lock().map_err(|e| format!("Lock error: {}", e))?;

        // Load file_to_shards
        let mut stmt = conn.prepare("SELECT file_hash, shard_id FROM index_files")
            .map_err(|e| format!("Prepare statement failed: {}", e))?;

        let file_rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }).map_err(|e| format!("Query failed: {}", e))?;

        let mut file_map = self.file_to_shards.write();
        for row in file_rows {
            let (file_hash, shard_id) = row.map_err(|e| format!("Row error: {}", e))?;
            file_map.entry(file_hash).or_default().push(shard_id);
        }

        // Load chunk_to_xorb
        let mut stmt = conn.prepare("SELECT chunk_hash, xorb_hash, chunk_index FROM index_chunks")
            .map_err(|e| format!("Prepare statement failed: {}", e))?;

        let chunk_rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
            ))
        }).map_err(|e| format!("Query failed: {}", e))?;

        let mut chunk_map = self.chunk_to_xorb.write();
        for row in chunk_rows {
            let (chunk_hash, xorb_hash, chunk_index) = row.map_err(|e| format!("Row error: {}", e))?;
            chunk_map.insert(chunk_hash, (xorb_hash, chunk_index));
        }

        Ok(())
    }

    /// I5: Persist a shard registration to SQLite
    fn persist_shard_to_db(
        &self,
        shard_id: &str,
        file_hashes: &[String],
        chunk_mappings: &[(String, String, u32)],
    ) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database connection")?;
        let conn = db.lock().map_err(|e| format!("Lock error: {}", e))?;

        // Insert file mappings
        for file_hash in file_hashes {
            conn.execute(
                "INSERT OR IGNORE INTO index_files (file_hash, shard_id) VALUES (?1, ?2)",
                rusqlite::params![file_hash, shard_id],
            ).map_err(|e| format!("Insert file mapping failed: {}", e))?;
        }

        // Insert chunk mappings
        for (chunk_hash, xorb_hash, chunk_index) in chunk_mappings {
            conn.execute(
                "INSERT OR REPLACE INTO index_chunks (chunk_hash, xorb_hash, chunk_index) VALUES (?1, ?2, ?3)",
                rusqlite::params![chunk_hash, xorb_hash, chunk_index],
            ).map_err(|e| format!("Insert chunk mapping failed: {}", e))?;
        }

        Ok(())
    }

    /// Register a shard and update file-to-shard mappings
    ///
    /// # Arguments
    /// * `shard_id` - Unique identifier for the shard
    /// * `file_hashes` - List of file hashes contained in this shard
    /// * `chunk_mappings` - List of (chunk_hash, xorb_hash, chunk_index) for global dedup
    pub fn register_shard(
        &self,
        shard_id: String,
        file_hashes: Vec<String>,
        chunk_mappings: Vec<(String, String, u32)>,
    ) {
        // Update file-to-shards mapping
        {
            let mut file_map = self.file_to_shards.write();
            for file_hash in &file_hashes {
                file_map
                    .entry(file_hash.clone())
                    .or_default()
                    .push(shard_id.clone());
            }
        }

        // Update chunk-to-xorb mapping
        {
            let mut chunk_map = self.chunk_to_xorb.write();
            for (chunk_hash, xorb_hash, chunk_index) in &chunk_mappings {
                chunk_map.insert(chunk_hash.clone(), (xorb_hash.clone(), *chunk_index));
            }
        }

        // I5: Persist to SQLite if persistence is enabled
        if self.db.is_some() {
            if let Err(e) = self.persist_shard_to_db(&shard_id, &file_hashes, &chunk_mappings) {
                tracing::warn!("Failed to persist index to disk: {}", e);
            }
        }
    }

    /// Get shard IDs for a file hash
    pub fn get_shards_for_file(&self, file_hash: &str) -> Option<Vec<String>> {
        let file_map = self.file_to_shards.read();
        file_map.get(file_hash).cloned()
    }

    /// Get xorb location for a chunk hash (for global dedup)
    pub fn get_xorb_for_chunk(&self, chunk_hash: &str) -> Option<(String, u32)> {
        let chunk_map = self.chunk_to_xorb.read();
        chunk_map.get(chunk_hash).cloned()
    }

    /// Check if a chunk exists in the index (for global dedup query)
    pub fn chunk_exists(&self, chunk_hash: &str) -> bool {
        let chunk_map = self.chunk_to_xorb.read();
        chunk_map.contains_key(chunk_hash)
    }

    /// Get statistics about the index
    pub fn stats(&self) -> IndexStats {
        let file_map = self.file_to_shards.read();
        let chunk_map = self.chunk_to_xorb.read();

        IndexStats {
            num_files: file_map.len(),
            num_chunks: chunk_map.len(),
        }
    }

    /// Rebuild the index by scanning shards in storage.
    /// Called once at server startup.
    ///
    /// Lists all objects under the `"shards/"` prefix, parses each shard,
    /// and registers its file and chunk mappings in the index.
    ///
    /// Returns the number of shards successfully indexed.
    pub async fn rebuild_from_storage(
        &self,
        storage: &dyn crate::storage::StorageBackend,
    ) -> Result<usize, String> {
        use crate::format::shard::MDBShardFile;

        let shard_keys = storage.list_objects("shards/").await
            .map_err(|e| format!("Failed to list shards: {}", e))?;

        let mut count = 0;
        for shard_key in &shard_keys {
            let shard_data = match storage.get(shard_key).await {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!("Failed to fetch shard {}: {}", shard_key, e);
                    continue;
                }
            };

            let shard = match MDBShardFile::parse(&shard_data) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Failed to parse shard {}: {}", shard_key, e);
                    continue;
                }
            };

            // Extract shard_id from key: "shards/{shard_id}"
            let shard_id = shard_key
                .strip_prefix("shards/")
                .unwrap_or(shard_key)
                .to_string();

            // MDBShardFile::parse pre-computes file_hashes and chunk_mappings
            let file_hashes: Vec<String> = shard.file_hashes()
                .iter()
                .map(|h| h.to_hex())
                .collect();

            let chunk_mappings: Vec<(String, String, u32)> = shard.chunk_mappings()
                .iter()
                .map(|(chunk_hash, xorb_hash, chunk_index)| {
                    (chunk_hash.to_hex(), xorb_hash.to_hex(), *chunk_index)
                })
                .collect();

            self.register_shard(shard_id, file_hashes, chunk_mappings);
            count += 1;
        }

        Ok(count)
    }
}

impl Default for MetadataIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the metadata index
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub num_files: usize,
    pub num_chunks: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_index_basic() {
        let index = MetadataIndex::new();

        // Register a shard with file and chunk mappings
        index.register_shard(
            "shard-1".to_string(),
            vec!["file-abc".to_string()],
            vec![
                ("chunk-1".to_string(), "xorb-xyz".to_string(), 0),
                ("chunk-2".to_string(), "xorb-xyz".to_string(), 1),
            ],
        );

        // Test file-to-shard mapping
        let shards = index.get_shards_for_file("file-abc").unwrap();
        assert_eq!(shards, vec!["shard-1"]);

        // Test chunk-to-xorb mapping
        let (xorb_hash, chunk_idx) = index.get_xorb_for_chunk("chunk-1").unwrap();
        assert_eq!(xorb_hash, "xorb-xyz");
        assert_eq!(chunk_idx, 0);

        // Test chunk existence
        assert!(index.chunk_exists("chunk-1"));
        assert!(!index.chunk_exists("nonexistent"));

        // Test stats
        let stats = index.stats();
        assert_eq!(stats.num_files, 1);
        assert_eq!(stats.num_chunks, 2);
    }

    #[test]
    fn test_metadata_index_multiple_shards() {
        let index = MetadataIndex::new();

        // Register first shard
        index.register_shard(
            "shard-1".to_string(),
            vec!["file-abc".to_string()],
            vec![("chunk-1".to_string(), "xorb-1".to_string(), 0)],
        );

        // Register second shard for same file
        index.register_shard(
            "shard-2".to_string(),
            vec!["file-abc".to_string()],
            vec![("chunk-2".to_string(), "xorb-2".to_string(), 0)],
        );

        // File should map to both shards
        let shards = index.get_shards_for_file("file-abc").unwrap();
        assert_eq!(shards.len(), 2);
        assert!(shards.contains(&"shard-1".to_string()));
        assert!(shards.contains(&"shard-2".to_string()));

        // Stats should reflect all chunks
        let stats = index.stats();
        assert_eq!(stats.num_files, 1);
        assert_eq!(stats.num_chunks, 2);
    }

    #[test]
    fn test_metadata_index_nonexistent_file() {
        let index = MetadataIndex::new();

        assert!(index.get_shards_for_file("nonexistent").is_none());
    }

    #[test]
    fn test_metadata_index_persistence() {
        use tempfile::NamedTempFile;

        // Create a temp file for the database
        let db_file = NamedTempFile::new().unwrap();
        let db_path = db_file.path().to_str().unwrap();

        // Create an index with persistence
        let index1 = MetadataIndex::with_persistence(db_path).unwrap();

        // Register some data
        index1.register_shard(
            "shard-persist-1".to_string(),
            vec!["file-persist-abc".to_string()],
            vec![
                ("chunk-p1".to_string(), "xorb-pxyz".to_string(), 0),
                ("chunk-p2".to_string(), "xorb-pxyz".to_string(), 1),
            ],
        );

        // Create a new index from the same database
        let index2 = MetadataIndex::with_persistence(db_path).unwrap();

        // Verify the data was loaded
        let shards = index2.get_shards_for_file("file-persist-abc").unwrap();
        assert_eq!(shards, vec!["shard-persist-1"]);

        let (xorb_hash, chunk_idx) = index2.get_xorb_for_chunk("chunk-p1").unwrap();
        assert_eq!(xorb_hash, "xorb-pxyz");
        assert_eq!(chunk_idx, 0);

        assert!(index2.chunk_exists("chunk-p1"));
        assert!(index2.chunk_exists("chunk-p2"));
        assert!(!index2.chunk_exists("nonexistent"));
    }
}
