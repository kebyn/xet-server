//! Metadata Index Manager
//!
//! Manages the index mappings for file reconstruction and global deduplication:
//! - file_hash → shard_id mapping
//! - chunk_hash → (xorb_hash, chunk_index) mapping for global dedup
//!
//! The index is rebuilt from storage on each startup (stateless server design).
//! This ensures consistency and avoids local state management complexity.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Metadata index for managing file-to-shard and chunk-to-xorb mappings
#[derive(Debug, Clone)]
pub struct MetadataIndex {
    /// Map from file hash to shard IDs that contain this file's reconstruction info
    file_to_shards: Arc<RwLock<HashMap<String, Vec<String>>>>,

    /// Map from chunk hash to (xorb_hash, chunk_index) for global deduplication
    chunk_to_xorb: Arc<RwLock<HashMap<String, (String, u32)>>>,
}

impl MetadataIndex {
    /// Create a new empty metadata index (in-memory only)
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
    /// I1/M1 fix: Uses bounded parallelism to fetch and parse shards concurrently,
    /// significantly reducing startup time for large storage (thousands of shards).
    /// Processes shards in batches of 10 to balance parallelism with resource usage.
    ///
    /// Lists all objects under the `"shards/"` prefix, parses each shard,
    /// and registers its file and chunk mappings in the index.
    ///
    /// Returns the number of shards successfully indexed.
    pub async fn rebuild_from_storage(
        &self,
        storage: Arc<Box<dyn crate::storage::StorageBackend>>,
    ) -> Result<usize, String> {
        use crate::format::shard::MDBShardFile;

        let shard_keys = storage
            .list_objects("shards/")
            .await
            .map_err(|e| format!("Failed to list shards: {}", e))?;

        // I1 fix: Process shards concurrently with bounded parallelism
        // Using batches of 10 to balance parallelism with resource usage
        const BATCH_SIZE: usize = 10;
        let mut total_count = 0;

        for chunk in shard_keys.chunks(BATCH_SIZE) {
            let mut handles = vec![];

            for shard_key in chunk {
                let storage_clone = storage.clone();
                let key = shard_key.clone();

                let handle = tokio::spawn(async move {
                    let shard_data = match storage_clone.get(&key).await {
                        Ok(data) => data,
                        Err(e) => {
                            tracing::warn!("Failed to fetch shard {}: {}", key, e);
                            return None;
                        }
                    };

                    // Parse shard and extract mappings
                    match MDBShardFile::parse(&shard_data) {
                        Ok(shard) => {
                            // Extract file hashes and convert to strings
                            let file_hashes: Vec<String> =
                                shard.file_hashes().iter().map(|h| h.to_hex()).collect();

                            // Extract chunk mappings and convert to strings
                            let chunk_mappings: Vec<(String, String, u32)> = shard
                                .chunk_mappings()
                                .iter()
                                .map(|(chunk, xorb, idx)| (chunk.to_hex(), xorb.to_hex(), *idx))
                                .collect();

                            // Extract shard_id from key (shards/{shard_id})
                            let shard_id = key.strip_prefix("shards/").unwrap_or(&key).to_string();

                            Some((shard_id, file_hashes, chunk_mappings))
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse shard {}: {}", key, e);
                            None
                        }
                    }
                });

                handles.push(handle);
            }

            // Wait for all tasks in this batch to complete and register results
            for handle in handles {
                match handle.await {
                    Ok(Some((shard_id, file_hashes, chunk_mappings))) => {
                        // Register in index (main task only, no concurrent writes)
                        self.register_shard(shard_id, file_hashes, chunk_mappings);
                        total_count += 1;
                    }
                    Ok(None) => {
                        // Shard fetch or parse failed, already logged
                    }
                    Err(e) => {
                        tracing::warn!("Shard processing task failed: {}", e);
                    }
                }
            }
        }

        Ok(total_count)
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
    fn test_register_and_query() {
        let index = MetadataIndex::new();

        // Register a shard
        let shard_id = "shard-001".to_string();
        let file_hashes = vec!["file-abc".to_string(), "file-def".to_string()];
        let chunk_mappings = vec![
            ("chunk-1".to_string(), "xorb-1".to_string(), 0),
            ("chunk-2".to_string(), "xorb-1".to_string(), 1),
        ];

        index.register_shard(shard_id.clone(), file_hashes.clone(), chunk_mappings);

        // Verify file-to-shards mapping
        let shards = index.get_shards_for_file("file-abc");
        assert!(shards.is_some());
        assert_eq!(shards.unwrap(), vec![shard_id]);

        // Verify chunk-to-xorb mapping
        let xorb = index.get_xorb_for_chunk("chunk-1");
        assert!(xorb.is_some());
        assert_eq!(xorb.unwrap(), ("xorb-1".to_string(), 0));

        // Verify stats
        let stats = index.stats();
        assert_eq!(stats.num_files, 2);
        assert_eq!(stats.num_chunks, 2);
    }

    #[test]
    fn test_multiple_shards() {
        let index = MetadataIndex::new();

        // Register first shard
        index.register_shard(
            "shard-001".to_string(),
            vec!["file-a".to_string()],
            vec![("chunk-1".to_string(), "xorb-1".to_string(), 0)],
        );

        // Register second shard with same file
        index.register_shard(
            "shard-002".to_string(),
            vec!["file-a".to_string()],
            vec![("chunk-2".to_string(), "xorb-2".to_string(), 0)],
        );

        // File should be in both shards
        let shards = index.get_shards_for_file("file-a").unwrap();
        assert_eq!(shards.len(), 2);
        assert!(shards.contains(&"shard-001".to_string()));
        assert!(shards.contains(&"shard-002".to_string()));
    }

    #[test]
    fn test_chunk_exists() {
        let index = MetadataIndex::new();

        index.register_shard(
            "shard-001".to_string(),
            vec![],
            vec![("chunk-1".to_string(), "xorb-1".to_string(), 0)],
        );

        assert!(index.chunk_exists("chunk-1"));
        assert!(!index.chunk_exists("chunk-2"));
    }
}
