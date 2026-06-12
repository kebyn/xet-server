//! Metadata Index Manager
//!
//! Manages the index mappings for file reconstruction and global deduplication:
//! - file_hash → shard_id mapping
//! - chunk_hash → (xorb_hash, chunk_index) mapping for global dedup

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
}

impl MetadataIndex {
    /// Create a new empty metadata index
    pub fn new() -> Self {
        Self {
            file_to_shards: Arc::new(RwLock::new(HashMap::new())),
            chunk_to_xorb: Arc::new(RwLock::new(HashMap::new())),
        }
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
            for file_hash in file_hashes {
                file_map
                    .entry(file_hash)
                    .or_default()
                    .push(shard_id.clone());
            }
        }

        // Update chunk-to-xorb mapping
        {
            let mut chunk_map = self.chunk_to_xorb.write();
            for (chunk_hash, xorb_hash, chunk_index) in chunk_mappings {
                chunk_map.insert(chunk_hash, (xorb_hash, chunk_index));
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
}
