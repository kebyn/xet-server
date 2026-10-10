//! Metadata Index Manager
//!
//! Manages the index mappings for file reconstruction and global deduplication:
//! - file_hash → shard_id mapping
//! - chunk_hash → (xorb_hash, chunk_index) mapping for global dedup
//!
//! The index is rebuilt from storage on each startup (stateless server design).
//! This ensures consistency and avoids local state management complexity.
//!
//! Index keys are `MerkleHash` values (fixed 32 bytes, zero heap allocation)
//! rather than 64-character hex strings, keeping the resident memory of the
//! dedup tables low at scale. Hex is only a transport encoding at the HTTP and
//! storage boundaries: because typed keys are parsed from hex, lookups are
//! case-insensitive — a query built from uppercase hex resolves to the same
//! entry a lowercase-hex producer registered.

use crate::types::MerkleHash;
use futures_util::StreamExt;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileShardRef {
    pub shard_id: String,
    pub file_index: usize,
    pub file_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFileMapping {
    pub file_hash: MerkleHash,
    pub file_index: usize,
    pub file_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexRegistrationError {
    #[error(
        "File hash {file_hash} is already registered with size {existing_size}, cannot register size {new_size}"
    )]
    FileSizeConflict {
        file_hash: MerkleHash,
        existing_size: u64,
        new_size: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChunkMapping {
    pub chunk_hash: MerkleHash,
    pub xorb_hash: MerkleHash,
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
    file_to_shards: Arc<RwLock<HashMap<MerkleHash, Vec<FileShardRef>>>>,

    /// Map from chunk hash to (xorb_hash, chunk_index) for global deduplication
    chunk_to_xorb: Arc<RwLock<HashMap<MerkleHash, (MerkleHash, u32)>>>,
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
    pub fn register_verified_shard(
        &self,
        registration: VerifiedShardRegistration,
    ) -> Result<(), IndexRegistrationError> {
        // Update file-to-shards mapping
        {
            let mut file_map = self.file_to_shards.write();
            let mut registration_sizes = HashMap::new();

            // Validate all file sizes before mutating either index so a rejected
            // registration cannot leave a partially applied file mapping.
            for file in &registration.files {
                if let Some(existing_size) =
                    registration_sizes.insert(file.file_hash, file.file_size)
                    && existing_size != file.file_size
                {
                    return Err(IndexRegistrationError::FileSizeConflict {
                        file_hash: file.file_hash,
                        existing_size,
                        new_size: file.file_size,
                    });
                }

                if let Some(existing_ref) = file_map
                    .get(&file.file_hash)
                    .and_then(|references| references.first())
                    && existing_ref.file_size != file.file_size
                {
                    return Err(IndexRegistrationError::FileSizeConflict {
                        file_hash: file.file_hash,
                        existing_size: existing_ref.file_size,
                        new_size: file.file_size,
                    });
                }
            }

            for file in &registration.files {
                let entry = file_map.entry(file.file_hash).or_default();
                let file_ref = FileShardRef {
                    shard_id: registration.shard_id.clone(),
                    file_index: file.file_index,
                    file_size: file.file_size,
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
                chunk_map.insert(chunk.chunk_hash, (chunk.xorb_hash, chunk.chunk_index));
            }
        }

        Ok(())
    }

    /// Get verified shard references for a file hash
    pub fn get_file_refs(&self, file_hash: &MerkleHash) -> Option<Vec<FileShardRef>> {
        let file_map = self.file_to_shards.read();
        file_map.get(file_hash).cloned()
    }

    /// Get the verified reconstructed size for a file hash.
    pub fn get_file_size(&self, file_hash: &MerkleHash) -> Option<u64> {
        let file_map = self.file_to_shards.read();
        file_map
            .get(file_hash)
            .and_then(|references| references.first())
            .map(|reference| reference.file_size)
    }

    /// Get shard IDs for a file hash
    pub fn get_shards_for_file(&self, file_hash: &MerkleHash) -> Option<Vec<String>> {
        self.get_file_refs(file_hash).map(|refs| {
            refs.into_iter()
                .map(|r| r.shard_id)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        })
    }

    /// Get xorb location for a chunk hash (for global dedup)
    pub fn get_xorb_for_chunk(&self, chunk_hash: &MerkleHash) -> Option<(MerkleHash, u32)> {
        let chunk_map = self.chunk_to_xorb.read();
        chunk_map.get(chunk_hash).cloned()
    }

    /// Check if a chunk exists in the index (for global dedup query)
    pub fn chunk_exists(&self, chunk_hash: &MerkleHash) -> bool {
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
    /// Uses bounded parallelism to fetch and parse shards concurrently,
    /// significantly reducing startup time for large storage (thousands of shards).
    /// Processes shards in batches of 10 to balance parallelism with resource usage.
    ///
    /// Lists all objects under the `"shards/"` prefix, parses each shard,
    /// and registers its file and chunk mappings in the index.
    ///
    /// Returns the number indexed only when every listed shard succeeds.
    /// Any unrecovered failure returns a bounded summary; object details stay in logs.
    pub async fn rebuild_from_storage(
        &self,
        storage: Arc<Box<dyn crate::storage::StorageBackend>>,
        temp_dir: std::path::PathBuf,
    ) -> Result<usize, String> {
        let mut shard_keys = storage.list_objects_stream("shards/");

        // Process shard keys as they are listed, keeping both parsing concurrency
        // and the number of retained listing entries bounded.
        const BATCH_SIZE: usize = 10;
        let mut total_count = 0;
        let mut failure_count = 0;
        let mut listing_failed = false;

        loop {
            let mut batch = Vec::with_capacity(BATCH_SIZE);
            while batch.len() < BATCH_SIZE {
                let Some(shard_key) = shard_keys.next().await else {
                    break;
                };
                match shard_key {
                    Ok(key) => batch.push(key),
                    Err(error) => {
                        tracing::warn!("Failed to list shards during rebuild: {}", error);
                        listing_failed = true;
                        break;
                    }
                }
            }
            if batch.is_empty() {
                break;
            }

            let mut handles = Vec::with_capacity(batch.len());
            for key in batch {
                let storage_clone = storage.clone();
                let temp_dir_clone = temp_dir.clone();
                let task_key = key.clone();

                let handle = tokio::spawn(async move {
                    let shard = match crate::shard_io::parse_shard_from_storage(
                        &**storage_clone,
                        &key,
                        &temp_dir_clone,
                    )
                    .await
                    {
                        Ok(shard) => shard,
                        Err(e) => {
                            tracing::warn!("Failed to fetch or parse shard {}: {}", key, e);
                            return None;
                        }
                    };

                    // Extract shard_id from key (shards/{shard_id})
                    let shard_id = key.strip_prefix("shards/").unwrap_or(&key).to_string();

                    match crate::shard_validation::validate_shard_for_index(
                        &shard_id,
                        &shard,
                        &**storage_clone,
                        &temp_dir_clone,
                    )
                    .await
                    {
                        Ok(registration) => Some(registration),
                        Err(e) => {
                            tracing::warn!(
                                "Skipping unverified shard {} during rebuild: {}",
                                shard_id,
                                e
                            );
                            None
                        }
                    }
                });

                handles.push((task_key, handle));
            }

            // Wait for all tasks in this batch to complete and register results
            for (key, handle) in handles {
                match handle.await {
                    Ok(Some(registration)) => {
                        // Register in index (main task only, no concurrent writes)
                        let shard_id = registration.shard_id.clone();
                        match self.register_verified_shard(registration) {
                            Ok(()) => total_count += 1,
                            Err(error) => {
                                failure_count += 1;
                                tracing::warn!(
                                    "Skipping shard {} due to an index registration conflict: {}",
                                    shard_id,
                                    error
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        // Shard fetch, parse or validation failed, already logged.
                        failure_count += 1;
                    }
                    Err(e) => {
                        failure_count += 1;
                        tracing::warn!("Shard processing task failed for {}: {}", key, e);
                    }
                }
            }
            if listing_failed {
                break;
            }
        }

        if failure_count > 0 || listing_failed {
            Err(format!(
                "Index rebuild incomplete: {total_count} shards indexed, {failure_count} shards failed, listing failed: {listing_failed}"
            ))
        } else {
            Ok(total_count)
        }
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
    use async_trait::async_trait;
    use bytes::Bytes;
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    use crate::format::compression::CompressionScheme;
    use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
    use crate::format::xorb_builder::XorbBuilder;
    use crate::hash::compute_data_hash;
    use crate::storage::local::LocalStorage;
    use crate::storage::{ObjectKeyStream, StorageBackend, StorageError, StorageResult};
    use crate::types::MerkleHash;

    fn test_hash(seed: u8) -> MerkleHash {
        MerkleHash::from([seed; 32])
    }

    fn sha256_merkle_hash(data: &[u8]) -> MerkleHash {
        let digest = Sha256::digest(data);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        MerkleHash::from(bytes)
    }

    /// A valid one-chunk shard plus the xorb it references, so rebuild tests
    /// can store both and let `validate_shard_for_index` pass.
    /// Returns (shard_data, xorb_data, xorb_hash_hex, file_hash).
    fn build_one_chunk_shard_parts(raw_chunk: &[u8]) -> (Vec<u8>, Vec<u8>, String, MerkleHash) {
        let raw_hash = compute_data_hash(raw_chunk);
        let file_hash = sha256_merkle_hash(raw_chunk);
        let mut xorb_builder = XorbBuilder::new(CompressionScheme::None);
        let (serialized_chunk_hash, compressed_len) = xorb_builder.add_chunk(raw_chunk).unwrap();
        let xorb = xorb_builder.build().unwrap();

        let mut shard_builder = ShardBuilder::new();
        let xorb_index = shard_builder
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                vec![XorbChunkBuildEntry {
                    chunk_hash: serialized_chunk_hash,
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: raw_chunk.len() as u32,
                }],
                vec![raw_hash],
            )
            .unwrap();

        shard_builder.add_file(
            file_hash,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: raw_chunk.len() as u32,
            }],
        );

        assert_eq!(compressed_len, raw_chunk.len() as u32);
        (
            shard_builder.build().unwrap(),
            xorb.data,
            xorb.xorb_hash.to_hex(),
            file_hash,
        )
    }

    fn build_one_chunk_shard(raw_chunk: &[u8]) -> (Vec<u8>, MerkleHash) {
        let (shard_data, _xorb_data, _xorb_hash, file_hash) =
            build_one_chunk_shard_parts(raw_chunk);
        (shard_data, file_hash)
    }

    struct StreamingListOnlyStorage {
        inner: LocalStorage,
        fail_listing: bool,
    }

    #[async_trait]
    impl StorageBackend for StreamingListOnlyStorage {
        async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
            self.inner.put(key, data).await
        }

        async fn get(&self, key: &str) -> StorageResult<Bytes> {
            self.inner.get(key).await
        }

        async fn get_path(&self, key: &str) -> StorageResult<Option<PathBuf>> {
            self.inner.get_path(key).await
        }

        async fn exists(&self, key: &str) -> StorageResult<bool> {
            self.inner.exists(key).await
        }

        async fn delete(&self, key: &str) -> StorageResult<()> {
            self.inner.delete(key).await
        }

        async fn list_objects(&self, _prefix: &str) -> StorageResult<Vec<String>> {
            Err(StorageError::internal(
                "index rebuild must not collect the complete key list",
            ))
        }

        fn list_objects_stream<'a>(&'a self, prefix: &'a str) -> ObjectKeyStream<'a> {
            if self.fail_listing {
                Box::pin(self.inner.list_objects_stream(prefix).take(1).chain(
                    futures_util::stream::once(async {
                        Err(StorageError::internal("injected list failure"))
                    }),
                ))
            } else {
                self.inner.list_objects_stream(prefix)
            }
        }

        async fn get_size(&self, key: &str) -> StorageResult<u64> {
            self.inner.get_size(key).await
        }

        async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
            self.inner.download_to_path(key, dest).await
        }
    }

    #[test]
    fn test_register_verified_shard_and_query_file_refs() {
        let index = MetadataIndex::new();

        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "shard-001".to_string(),
                files: vec![
                    VerifiedFileMapping {
                        file_hash: test_hash(0xA1),
                        file_index: 0,
                        file_size: 10,
                    },
                    VerifiedFileMapping {
                        file_hash: test_hash(0xA2),
                        file_index: 1,
                        file_size: 20,
                    },
                ],
                chunks: vec![VerifiedChunkMapping {
                    chunk_hash: test_hash(0xB1),
                    xorb_hash: test_hash(0xC1),
                    chunk_index: 0,
                }],
            })
            .unwrap();

        let refs = index.get_file_refs(&test_hash(0xA2)).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].shard_id, "shard-001");
        assert_eq!(refs[0].file_index, 1);
        assert_eq!(refs[0].file_size, 20);
        assert_eq!(index.get_file_size(&test_hash(0xA2)), Some(20));

        assert_eq!(
            index.get_xorb_for_chunk(&test_hash(0xB1)),
            Some((test_hash(0xC1), 0))
        );
    }

    #[test]
    fn test_register_verified_shard_is_idempotent_per_file_ref() {
        let index = MetadataIndex::new();
        let reg = VerifiedShardRegistration {
            shard_id: "shard-001".to_string(),
            files: vec![VerifiedFileMapping {
                file_hash: test_hash(0xA1),
                file_index: 0,
                file_size: 10,
            }],
            chunks: vec![],
        };
        index.register_verified_shard(reg.clone()).unwrap();
        index.register_verified_shard(reg).unwrap();

        let refs = index.get_file_refs(&test_hash(0xA1)).unwrap();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn test_register_and_query() {
        let index = MetadataIndex::new();

        let shard_id = "shard-001".to_string();
        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: shard_id.clone(),
                files: vec![
                    VerifiedFileMapping {
                        file_hash: test_hash(0xA1),
                        file_index: 0,
                        file_size: 10,
                    },
                    VerifiedFileMapping {
                        file_hash: test_hash(0xA2),
                        file_index: 1,
                        file_size: 20,
                    },
                ],
                chunks: vec![
                    VerifiedChunkMapping {
                        chunk_hash: test_hash(0xB1),
                        xorb_hash: test_hash(0xC1),
                        chunk_index: 0,
                    },
                    VerifiedChunkMapping {
                        chunk_hash: test_hash(0xB2),
                        xorb_hash: test_hash(0xC1),
                        chunk_index: 1,
                    },
                ],
            })
            .unwrap();

        // Verify file-to-shards mapping
        let shards = index.get_shards_for_file(&test_hash(0xA1));
        assert!(shards.is_some());
        assert_eq!(shards.unwrap(), vec![shard_id]);

        // Verify chunk-to-xorb mapping
        let xorb = index.get_xorb_for_chunk(&test_hash(0xB1));
        assert!(xorb.is_some());
        assert_eq!(xorb.unwrap(), (test_hash(0xC1), 0));

        // Verify stats
        let stats = index.stats();
        assert_eq!(stats.num_files, 2);
        assert_eq!(stats.num_chunks, 2);
    }

    #[test]
    fn test_multiple_shards() {
        let index = MetadataIndex::new();

        // Register first shard
        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "shard-001".to_string(),
                files: vec![VerifiedFileMapping {
                    file_hash: test_hash(0xA3),
                    file_index: 0,
                    file_size: 10,
                }],
                chunks: vec![VerifiedChunkMapping {
                    chunk_hash: test_hash(0xB1),
                    xorb_hash: test_hash(0xC1),
                    chunk_index: 0,
                }],
            })
            .unwrap();

        // Register second shard with same file
        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "shard-002".to_string(),
                files: vec![VerifiedFileMapping {
                    file_hash: test_hash(0xA3),
                    file_index: 0,
                    file_size: 10,
                }],
                chunks: vec![VerifiedChunkMapping {
                    chunk_hash: test_hash(0xB2),
                    xorb_hash: test_hash(0xC2),
                    chunk_index: 0,
                }],
            })
            .unwrap();

        // File should be in both shards
        let shards = index.get_shards_for_file(&test_hash(0xA3)).unwrap();
        assert_eq!(shards.len(), 2);
        assert!(shards.contains(&"shard-001".to_string()));
        assert!(shards.contains(&"shard-002".to_string()));
    }

    #[test]
    fn test_rejects_conflicting_file_size_without_partial_registration() {
        let index = MetadataIndex::new();
        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "shard-001".to_string(),
                files: vec![VerifiedFileMapping {
                    file_hash: test_hash(0xA3),
                    file_index: 0,
                    file_size: 10,
                }],
                chunks: vec![VerifiedChunkMapping {
                    chunk_hash: test_hash(0xB1),
                    xorb_hash: test_hash(0xC1),
                    chunk_index: 0,
                }],
            })
            .unwrap();

        let error = index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "shard-002".to_string(),
                files: vec![
                    VerifiedFileMapping {
                        file_hash: test_hash(0xA4),
                        file_index: 0,
                        file_size: 5,
                    },
                    VerifiedFileMapping {
                        file_hash: test_hash(0xA3),
                        file_index: 1,
                        file_size: 11,
                    },
                ],
                chunks: vec![VerifiedChunkMapping {
                    chunk_hash: test_hash(0xB2),
                    xorb_hash: test_hash(0xC2),
                    chunk_index: 0,
                }],
            })
            .expect_err("same hash with a different verified size must be rejected");

        assert!(matches!(
            error,
            IndexRegistrationError::FileSizeConflict {
                existing_size: 10,
                new_size: 11,
                ..
            }
        ));
        assert_eq!(index.get_file_size(&test_hash(0xA3)), Some(10));
        assert!(index.get_file_refs(&test_hash(0xA4)).is_none());
        assert!(!index.chunk_exists(&test_hash(0xB2)));
    }

    #[test]
    fn test_chunk_exists() {
        let index = MetadataIndex::new();

        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "shard-001".to_string(),
                files: vec![],
                chunks: vec![VerifiedChunkMapping {
                    chunk_hash: test_hash(0xB1),
                    xorb_hash: test_hash(0xC1),
                    chunk_index: 0,
                }],
            })
            .unwrap();

        assert!(index.chunk_exists(&test_hash(0xB1)));
        assert!(!index.chunk_exists(&test_hash(0xB2)));
    }

    #[tokio::test]
    async fn test_rebuild_from_storage_fails_when_referenced_xorb_missing() {
        let raw = b"rebuild should not trust shard declarations without xorb validation";
        let (shard_data, file_hash) = build_one_chunk_shard(raw);

        let storage_dir = tempdir().unwrap();
        let rebuild_temp_dir = tempdir().unwrap();
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(
            LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
        ));

        let shard_id = compute_data_hash(&shard_data).to_hex();
        storage
            .put(&format!("shards/{}", shard_id), Bytes::from(shard_data))
            .await
            .unwrap();

        let index = MetadataIndex::new();
        let count = index
            .rebuild_from_storage(storage, rebuild_temp_dir.path().to_path_buf())
            .await
            .unwrap_err();

        assert_eq!(
            count,
            "Index rebuild incomplete: 0 shards indexed, 1 shards failed, listing failed: false"
        );
        assert!(index.get_shards_for_file(&file_hash).is_none());
    }

    #[tokio::test]
    async fn rebuild_consumes_streaming_listing_without_collecting_all_keys() {
        let raw = b"stream the shard listing before validating referenced objects";
        let (shard_data, file_hash) = build_one_chunk_shard(raw);
        let storage_dir = tempdir().unwrap();
        let rebuild_temp_dir = tempdir().unwrap();
        let local = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        let shard_id = compute_data_hash(&shard_data).to_hex();
        local
            .put(&format!("shards/{shard_id}"), Bytes::from(shard_data))
            .await
            .unwrap();
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(StreamingListOnlyStorage {
            inner: local,
            fail_listing: false,
        }));

        let index = MetadataIndex::new();
        let count = index
            .rebuild_from_storage(storage, rebuild_temp_dir.path().to_path_buf())
            .await
            .unwrap_err();

        assert_eq!(
            count,
            "Index rebuild incomplete: 0 shards indexed, 1 shards failed, listing failed: false"
        );
        assert!(index.get_shards_for_file(&file_hash).is_none());
    }

    #[test]
    fn typed_keys_make_hex_lookups_case_insensitive() {
        let index = MetadataIndex::new();
        let chunk = test_hash(0xB1);
        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "shard-001".to_string(),
                files: vec![],
                chunks: vec![VerifiedChunkMapping {
                    chunk_hash: chunk,
                    xorb_hash: test_hash(0xC1),
                    chunk_index: 0,
                }],
            })
            .unwrap();

        // Hex is only a transport encoding: an uppercase rendering of the
        // same digest parses back to the identical typed key.
        let upper_hex = chunk.to_hex().to_uppercase();
        let parsed = MerkleHash::from_hex(&upper_hex).unwrap();
        assert_eq!(parsed, chunk);
        assert!(index.chunk_exists(&parsed));
        assert_eq!(
            index.get_xorb_for_chunk(&parsed),
            Some((test_hash(0xC1), 0))
        );
    }

    #[tokio::test]
    async fn rebuild_retries_transient_shard_fetch_and_indexes_shard() {
        let raw = b"transient storage blips must not permanently skip a shard";
        let (shard_data, xorb_data, xorb_hash, _file_hash) = build_one_chunk_shard_parts(raw);
        let storage_dir = tempdir().unwrap();
        let rebuild_temp_dir = tempdir().unwrap();
        let local = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        let shard_id = compute_data_hash(&shard_data).to_hex();
        local
            .put(&format!("shards/{}", shard_id), Bytes::from(shard_data))
            .await
            .unwrap();
        local
            .put(&format!("xorbs/{}", xorb_hash), Bytes::from(xorb_data))
            .await
            .unwrap();

        let (flaky, attempts) = crate::storage::flaky::FlakyStorage::new(
            local,
            format!("shards/{}", shard_id),
            crate::storage::flaky::InjectedFailure::Transient,
            1,
        );
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(flaky));

        let index = MetadataIndex::new();
        let count = index
            .rebuild_from_storage(storage, rebuild_temp_dir.path().to_path_buf())
            .await
            .unwrap();

        assert_eq!(count, 1, "the retried fetch must index the shard");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly one retry must happen"
        );
    }

    #[tokio::test]
    async fn rebuild_does_not_retry_shard_parse_failures() {
        let garbage = b"this is not a shard".to_vec();
        let storage_dir = tempdir().unwrap();
        let rebuild_temp_dir = tempdir().unwrap();
        let local = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        let shard_id = compute_data_hash(&garbage).to_hex();
        local
            .put(&format!("shards/{}", shard_id), Bytes::from(garbage))
            .await
            .unwrap();

        // No injection: get_path must serve the garbage file so the failure
        // comes from parsing (permanent), not from the fetch.
        let (flaky, attempts) = crate::storage::flaky::FlakyStorage::new(
            local,
            format!("shards/{}", shard_id),
            crate::storage::flaky::InjectedFailure::Transient,
            0,
        );
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(flaky));

        let index = MetadataIndex::new();
        let count = index
            .rebuild_from_storage(storage, rebuild_temp_dir.path().to_path_buf())
            .await
            .unwrap_err();

        assert_eq!(
            count,
            "Index rebuild incomplete: 0 shards indexed, 1 shards failed, listing failed: false"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "parse failures must never be retried"
        );
    }

    #[tokio::test]
    async fn rebuild_does_not_retry_missing_shards() {
        let raw = b"a shard whose fetch always reports the object missing";
        let (shard_data, _xorb_data, _xorb_hash, _file_hash) = build_one_chunk_shard_parts(raw);
        let storage_dir = tempdir().unwrap();
        let rebuild_temp_dir = tempdir().unwrap();
        let local = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        let shard_id = compute_data_hash(&shard_data).to_hex();
        // The bytes exist on disk so the listing yields the key, but the
        // wrapper's get_path always reports NotFound.
        local
            .put(&format!("shards/{}", shard_id), Bytes::from(shard_data))
            .await
            .unwrap();

        let (flaky, attempts) = crate::storage::flaky::FlakyStorage::new(
            local,
            format!("shards/{}", shard_id),
            crate::storage::flaky::InjectedFailure::Missing,
            usize::MAX,
        );
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(flaky));

        let index = MetadataIndex::new();
        let count = index
            .rebuild_from_storage(storage, rebuild_temp_dir.path().to_path_buf())
            .await
            .unwrap_err();

        assert_eq!(
            count,
            "Index rebuild incomplete: 0 shards indexed, 1 shards failed, listing failed: false"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "missing objects must not consume the retry budget"
        );
    }
    #[tokio::test]
    async fn rebuild_empty_storage_is_ready() {
        let dir = tempdir().unwrap();
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap(),
        ));
        assert_eq!(
            MetadataIndex::new()
                .rebuild_from_storage(storage, dir.path().join("temp"))
                .await
                .unwrap(),
            0
        );
    }

    async fn store_valid_shard(local: &LocalStorage, raw: &[u8]) -> (String, MerkleHash) {
        let (shard, xorb, xorb_hash, file_hash) = build_one_chunk_shard_parts(raw);
        let key = format!("shards/{}", compute_data_hash(&shard).to_hex());
        local.put(&key, Bytes::from(shard)).await.unwrap();
        local
            .put(&format!("xorbs/{xorb_hash}"), Bytes::from(xorb))
            .await
            .unwrap();
        (key, file_hash)
    }

    #[tokio::test]
    async fn rebuild_reports_mixed_failure_and_keeps_valid_shards() {
        let dir = tempdir().unwrap();
        let local = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        let (_, file_hash) = store_valid_shard(&local, b"valid shard survives").await;
        local
            .put("shards/corrupt", Bytes::from_static(b"not a shard"))
            .await
            .unwrap();
        let index = MetadataIndex::new();
        let error = index
            .rebuild_from_storage(Arc::new(Box::new(local)), dir.path().join("temp"))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "Index rebuild incomplete: 1 shards indexed, 1 shards failed, listing failed: false"
        );
        assert!(index.get_file_refs(&file_hash).is_some());
    }

    #[tokio::test]
    async fn rebuild_propagates_registration_failure() {
        let dir = tempdir().unwrap();
        let local = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        let (_, file_hash) = store_valid_shard(&local, b"registration conflict").await;
        let index = MetadataIndex::new();
        index
            .register_verified_shard(VerifiedShardRegistration {
                shard_id: "existing".to_string(),
                files: vec![VerifiedFileMapping {
                    file_hash,
                    file_index: 0,
                    file_size: 999,
                }],
                chunks: vec![],
            })
            .unwrap();
        let error = index
            .rebuild_from_storage(Arc::new(Box::new(local)), dir.path().join("temp"))
            .await
            .unwrap_err();
        assert!(error.contains("0 shards indexed, 1 shards failed"));
        assert_eq!(index.get_file_size(&file_hash), Some(999));
    }

    #[tokio::test]
    async fn rebuild_propagates_exhausted_retries_and_task_panics() {
        use crate::storage::flaky::{FlakyStorage, InjectedFailure};
        for (failure, expected_attempts) in [
            (
                InjectedFailure::Transient,
                crate::storage::TRANSIENT_STORAGE_RETRY_DELAYS.len() + 1,
            ),
            (InjectedFailure::Panic, 1),
        ] {
            let dir = tempdir().unwrap();
            let local = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
            let (key, _) = store_valid_shard(&local, b"failing task").await;
            let (flaky, attempts) = FlakyStorage::new(local, key, failure, usize::MAX);
            let error = MetadataIndex::new()
                .rebuild_from_storage(Arc::new(Box::new(flaky)), dir.path().join("temp"))
                .await
                .unwrap_err();
            assert!(error.contains("0 shards indexed, 1 shards failed"));
            assert_eq!(
                attempts.load(std::sync::atomic::Ordering::SeqCst),
                expected_attempts
            );
        }
    }

    #[tokio::test]
    async fn rebuild_reports_listing_failure_after_processing_pending_batch() {
        let dir = tempdir().unwrap();
        let local = LocalStorage::new(dir.path().to_str().unwrap()).unwrap();
        let (_, file_hash) = store_valid_shard(&local, b"listed before failure").await;
        let storage = StreamingListOnlyStorage {
            inner: local,
            fail_listing: true,
        };
        let index = MetadataIndex::new();
        let error = index
            .rebuild_from_storage(Arc::new(Box::new(storage)), dir.path().join("temp"))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "Index rebuild incomplete: 1 shards indexed, 0 shards failed, listing failed: true"
        );
        assert!(index.get_file_refs(&file_hash).is_some());
    }
}
