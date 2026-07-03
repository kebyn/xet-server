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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileShardRef {
    pub shard_id: String,
    pub file_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFileMapping {
    pub file_hash: String,
    pub file_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChunkMapping {
    pub chunk_hash: String,
    pub xorb_hash: String,
    pub chunk_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedShardRegistration {
    pub shard_id: String,
    pub files: Vec<VerifiedFileMapping>,
    pub chunks: Vec<VerifiedChunkMapping>,
}

/// Metadata index for managing file-to-shard and chunk-to-xorb mappings
#[derive(Debug, Clone)]
pub struct MetadataIndex {
    /// Map from file hash to verified shard references that contain reconstruction info
    file_to_shards: Arc<RwLock<HashMap<String, Vec<FileShardRef>>>>,

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

    /// Register verified shard mappings and update reconstruction/deduplication indexes.
    pub fn register_verified_shard(&self, registration: VerifiedShardRegistration) {
        // Update file-to-shards mapping
        {
            let mut file_map = self.file_to_shards.write();
            for file in &registration.files {
                let entry = file_map.entry(file.file_hash.clone()).or_default();
                let file_ref = FileShardRef {
                    shard_id: registration.shard_id.clone(),
                    file_index: file.file_index,
                };
                if !entry.contains(&file_ref) {
                    entry.push(file_ref);
                }
            }
        }

        // Update chunk-to-xorb mapping
        {
            let mut chunk_map = self.chunk_to_xorb.write();
            for chunk in &registration.chunks {
                chunk_map.insert(
                    chunk.chunk_hash.clone(),
                    (chunk.xorb_hash.clone(), chunk.chunk_index),
                );
            }
        }
    }

    /// Temporary Task 3 bridge for existing declaration-based call sites.
    ///
    /// Task 6 replaces these raw declaration registrations with validated shard content.
    pub fn register_shard(
        &self,
        shard_id: String,
        file_hashes: Vec<String>,
        chunk_mappings: Vec<(String, String, u32)>,
    ) {
        let files = file_hashes
            .into_iter()
            .enumerate()
            .map(|(file_index, file_hash)| VerifiedFileMapping {
                file_hash,
                file_index,
            })
            .collect();
        let chunks = chunk_mappings
            .into_iter()
            .map(
                |(chunk_hash, xorb_hash, chunk_index)| VerifiedChunkMapping {
                    chunk_hash,
                    xorb_hash,
                    chunk_index,
                },
            )
            .collect();

        self.register_verified_shard(VerifiedShardRegistration {
            shard_id,
            files,
            chunks,
        });
    }

    /// Get verified shard references for a file hash
    pub fn get_file_refs(&self, file_hash: &str) -> Option<Vec<FileShardRef>> {
        let file_map = self.file_to_shards.read();
        file_map.get(file_hash).cloned()
    }

    /// Get shard IDs for a file hash
    pub fn get_shards_for_file(&self, file_hash: &str) -> Option<Vec<String>> {
        self.get_file_refs(file_hash).map(|refs| {
            refs.into_iter()
                .map(|r| r.shard_id)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        })
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
                            // Extract file hashes and convert to verified registration entries.
                            // Task 6 will replace declaration registration with validation.
                            let files: Vec<VerifiedFileMapping> = shard
                                .file_hashes()
                                .iter()
                                .enumerate()
                                .map(|(file_index, h)| VerifiedFileMapping {
                                    file_hash: h.to_hex(),
                                    file_index,
                                })
                                .collect();

                            // Extract chunk mappings and convert to verified registration entries.
                            let chunks: Vec<VerifiedChunkMapping> = shard
                                .chunk_mappings()
                                .iter()
                                .map(|(chunk, xorb, idx)| VerifiedChunkMapping {
                                    chunk_hash: chunk.to_hex(),
                                    xorb_hash: xorb.to_hex(),
                                    chunk_index: *idx,
                                })
                                .collect();

                            // Extract shard_id from key (shards/{shard_id})
                            let shard_id = key.strip_prefix("shards/").unwrap_or(&key).to_string();

                            Some(VerifiedShardRegistration {
                                shard_id,
                                files,
                                chunks,
                            })
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
                    Ok(Some(registration)) => {
                        // Register in index (main task only, no concurrent writes)
                        self.register_verified_shard(registration);
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
    fn test_register_verified_shard_and_query_file_refs() {
        let index = MetadataIndex::new();

        index.register_verified_shard(VerifiedShardRegistration {
            shard_id: "shard-001".to_string(),
            files: vec![
                VerifiedFileMapping {
                    file_hash: "file-abc".to_string(),
                    file_index: 0,
                },
                VerifiedFileMapping {
                    file_hash: "file-def".to_string(),
                    file_index: 1,
                },
            ],
            chunks: vec![VerifiedChunkMapping {
                chunk_hash: "chunk-1".to_string(),
                xorb_hash: "xorb-1".to_string(),
                chunk_index: 0,
            }],
        });

        let refs = index.get_file_refs("file-def").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].shard_id, "shard-001");
        assert_eq!(refs[0].file_index, 1);

        assert_eq!(
            index.get_xorb_for_chunk("chunk-1"),
            Some(("xorb-1".to_string(), 0))
        );
    }

    #[test]
    fn test_register_verified_shard_is_idempotent_per_file_ref() {
        let index = MetadataIndex::new();
        let reg = VerifiedShardRegistration {
            shard_id: "shard-001".to_string(),
            files: vec![VerifiedFileMapping {
                file_hash: "file-abc".to_string(),
                file_index: 0,
            }],
            chunks: vec![],
        };
        index.register_verified_shard(reg.clone());
        index.register_verified_shard(reg);

        let refs = index.get_file_refs("file-abc").unwrap();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn test_register_and_query() {
        let index = MetadataIndex::new();

        let shard_id = "shard-001".to_string();
        index.register_verified_shard(VerifiedShardRegistration {
            shard_id: shard_id.clone(),
            files: vec![
                VerifiedFileMapping {
                    file_hash: "file-abc".to_string(),
                    file_index: 0,
                },
                VerifiedFileMapping {
                    file_hash: "file-def".to_string(),
                    file_index: 1,
                },
            ],
            chunks: vec![
                VerifiedChunkMapping {
                    chunk_hash: "chunk-1".to_string(),
                    xorb_hash: "xorb-1".to_string(),
                    chunk_index: 0,
                },
                VerifiedChunkMapping {
                    chunk_hash: "chunk-2".to_string(),
                    xorb_hash: "xorb-1".to_string(),
                    chunk_index: 1,
                },
            ],
        });

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
        index.register_verified_shard(VerifiedShardRegistration {
            shard_id: "shard-001".to_string(),
            files: vec![VerifiedFileMapping {
                file_hash: "file-a".to_string(),
                file_index: 0,
            }],
            chunks: vec![VerifiedChunkMapping {
                chunk_hash: "chunk-1".to_string(),
                xorb_hash: "xorb-1".to_string(),
                chunk_index: 0,
            }],
        });

        // Register second shard with same file
        index.register_verified_shard(VerifiedShardRegistration {
            shard_id: "shard-002".to_string(),
            files: vec![VerifiedFileMapping {
                file_hash: "file-a".to_string(),
                file_index: 0,
            }],
            chunks: vec![VerifiedChunkMapping {
                chunk_hash: "chunk-2".to_string(),
                xorb_hash: "xorb-2".to_string(),
                chunk_index: 0,
            }],
        });

        // File should be in both shards
        let shards = index.get_shards_for_file("file-a").unwrap();
        assert_eq!(shards.len(), 2);
        assert!(shards.contains(&"shard-001".to_string()));
        assert!(shards.contains(&"shard-002".to_string()));
    }

    #[test]
    fn test_chunk_exists() {
        let index = MetadataIndex::new();

        index.register_verified_shard(VerifiedShardRegistration {
            shard_id: "shard-001".to_string(),
            files: vec![],
            chunks: vec![VerifiedChunkMapping {
                chunk_hash: "chunk-1".to_string(),
                xorb_hash: "xorb-1".to_string(),
                chunk_index: 0,
            }],
        });

        assert!(index.chunk_exists("chunk-1"));
        assert!(!index.chunk_exists("chunk-2"));
    }

    #[test]
    fn test_register_shard_compat_bridge_preserves_old_api() {
        let index = MetadataIndex::new();

        index.register_shard(
            "shard-001".to_string(),
            vec!["file-a".to_string(), "file-b".to_string()],
            vec![("chunk-1".to_string(), "xorb-1".to_string(), 0)],
        );

        let refs = index.get_file_refs("file-b").unwrap();
        assert_eq!(
            refs,
            vec![FileShardRef {
                shard_id: "shard-001".to_string(),
                file_index: 1,
            }]
        );
        assert_eq!(
            index.get_xorb_for_chunk("chunk-1"),
            Some(("xorb-1".to_string(), 0))
        );
    }
}
