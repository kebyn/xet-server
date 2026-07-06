use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::format::shard::MDBShardFile;
use crate::hash::{compute_data_hash, file_hash};
use crate::index::FileShardRef;
use crate::reconstruction_plan::build_file_chunk_plan;
use crate::storage::{StorageBackend, StorageError};
use crate::types::MerkleHash;
use crate::util::StreamingHasher;
use crate::xorb_reader::{TempPathGuard, extract_chunk_verified_from_file};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconstructionError {
    InvalidInput(String),
    Stale(String),
    Storage(String),
    TempIo(String),
    Parse(String),
    Integrity(String),
}

impl ReconstructionError {
    pub fn is_stale(&self) -> bool {
        matches!(self, Self::Stale(_))
    }
}

impl std::fmt::Display for ReconstructionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(e) => write!(f, "invalid input: {}", e),
            Self::Stale(e) => write!(f, "stale reconstruction reference: {}", e),
            Self::Storage(e) => write!(f, "storage error: {}", e),
            Self::TempIo(e) => write!(f, "temporary file error: {}", e),
            Self::Parse(e) => write!(f, "parse error: {}", e),
            Self::Integrity(e) => write!(f, "integrity error: {}", e),
        }
    }
}

impl std::error::Error for ReconstructionError {}

pub struct VerifiedReconstruction {
    path_guard: TempPathGuard,
    size: u64,
}

impl VerifiedReconstruction {
    pub fn path(&self) -> &Path {
        self.path_guard.path()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn into_guard(self) -> TempPathGuard {
        self.path_guard
    }
}

pub async fn reconstruct_verified_file_to_temp(
    file_id: &str,
    file_refs: Vec<FileShardRef>,
    storage: &dyn StorageBackend,
    temp_dir: &Path,
) -> Result<VerifiedReconstruction, ReconstructionError> {
    std::fs::create_dir_all(temp_dir).map_err(|e| {
        ReconstructionError::TempIo(format!("failed to create reconstruction temp dir: {}", e))
    })?;
    let target_hash = MerkleHash::from_hex(file_id).map_err(|e| {
        ReconstructionError::InvalidInput(format!("invalid file id {}: {}", file_id, e))
    })?;
    let canonical_file_id = target_hash.to_hex();

    let mut first_non_stale_error: Option<ReconstructionError> = None;
    for file_ref in file_refs {
        match reconstruct_single_file_ref_to_temp(
            &canonical_file_id,
            &target_hash,
            file_ref,
            storage,
            temp_dir,
        )
        .await
        {
            Ok(reconstruction) => return Ok(reconstruction),
            Err(e) if e.is_stale() => {}
            Err(e) => {
                if first_non_stale_error.is_none() {
                    first_non_stale_error = Some(e);
                }
            }
        }
    }

    Err(first_non_stale_error.unwrap_or_else(|| {
        ReconstructionError::Stale(format!(
            "no verified shard reference could reconstruct {}",
            canonical_file_id
        ))
    }))
}

async fn reconstruct_single_file_ref_to_temp(
    canonical_file_id: &str,
    target_hash: &MerkleHash,
    file_ref: FileShardRef,
    storage: &dyn StorageBackend,
    temp_dir: &Path,
) -> Result<VerifiedReconstruction, ReconstructionError> {
    let output_guard = TempPathGuard::new(temp_dir.join(format!(
        "reconstruct-{}-{}.tmp",
        canonical_file_id,
        uuid::Uuid::new_v4()
    )));
    let output_path = output_guard.try_path().map_err(|e| {
        ReconstructionError::TempIo(format!("failed to resolve reconstruction temp file: {}", e))
    })?;
    let mut output = tokio::fs::File::create(output_path).await.map_err(|e| {
        ReconstructionError::TempIo(format!("failed to create reconstruction temp file: {}", e))
    })?;

    let mut sha = Sha256::new();
    let mut whole_blake3 = StreamingHasher::new();
    let mut chunk_nodes: Vec<(MerkleHash, u64)> = Vec::new();
    let mut total_size = 0u64;
    let mut xorb_cache: HashMap<MerkleHash, TempPathGuard> = HashMap::new();

    let shard = fetch_shard(&file_ref.shard_id, storage).await?;
    let plan = build_file_chunk_plan(&shard, target_hash, Some(file_ref.file_index))
        .map_err(|e| ReconstructionError::Integrity(e.to_string()))?;

    for planned in plan.chunks {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            xorb_cache.entry(planned.xorb_hash)
        {
            let key = format!("xorbs/{}", planned.xorb_hash.to_hex());
            let guard = TempPathGuard::new(temp_dir.join(format!(
                "reconstruct-xorb-{}-{}.tmp",
                planned.xorb_hash.to_hex(),
                uuid::Uuid::new_v4()
            )));
            storage
                .download_to_path(
                    &key,
                    guard.try_path().map_err(|e| {
                        ReconstructionError::TempIo(format!(
                            "failed to resolve xorb temp file {}: {}",
                            planned.xorb_hash.to_hex(),
                            e
                        ))
                    })?,
                )
                .await
                .map_err(|e| map_storage_error(&key, e))?;
            entry.insert(guard);
        }

        let guard = cached_xorb_guard(&xorb_cache, &planned.xorb_hash)?;
        let xorb_path = guard.try_path().map_err(|e| {
            ReconstructionError::TempIo(format!(
                "failed to resolve xorb {} temp file: {}",
                planned.xorb_hash.to_hex(),
                e
            ))
        })?;
        let mut xorb_file = tokio::fs::File::open(xorb_path).await.map_err(|e| {
            ReconstructionError::TempIo(format!(
                "failed to open xorb {}: {}",
                planned.xorb_hash.to_hex(),
                e
            ))
        })?;
        let bytes = extract_chunk_verified_from_file(
            &mut xorb_file,
            planned.chunk_byte_range_start as u64,
            planned.unpacked_segment_bytes,
            &planned.serialized_chunk_hash,
        )
        .await
        .map_err(ReconstructionError::Integrity)?;
        let raw_hash = compute_data_hash(&bytes);
        if raw_hash != planned.raw_chunk_hash {
            return Err(ReconstructionError::Integrity(format!(
                "raw chunk hash mismatch for xorb {} chunk {}",
                planned.xorb_hash.to_hex(),
                planned.xorb_chunk_index
            )));
        }
        output.write_all(&bytes).await.map_err(|e| {
            ReconstructionError::TempIo(format!("failed to write reconstructed bytes: {}", e))
        })?;
        sha.update(&bytes);
        whole_blake3.update(&bytes);
        total_size += bytes.len() as u64;
        chunk_nodes.push((raw_hash, bytes.len() as u64));
    }

    output.sync_all().await.map_err(|e| {
        ReconstructionError::TempIo(format!("failed to sync reconstructed file: {}", e))
    })?;

    let sha_hex = format!("{:x}", sha.finalize());
    let blake3_hex = whole_blake3.finalize().to_hex();
    let xet_file_hash = file_hash(&chunk_nodes).to_hex();
    if canonical_file_id != sha_hex
        && canonical_file_id != blake3_hex
        && canonical_file_id != xet_file_hash
    {
        return Err(ReconstructionError::Integrity(format!(
            "reconstructed file hash mismatch for {}: sha256={}, blake3={}, xet_file_hash={}",
            canonical_file_id, sha_hex, blake3_hex, xet_file_hash
        )));
    }

    Ok(VerifiedReconstruction {
        path_guard: output_guard,
        size: total_size,
    })
}

async fn fetch_shard(
    shard_id: &str,
    storage: &dyn StorageBackend,
) -> Result<MDBShardFile, ReconstructionError> {
    let key = format!("shards/{}", shard_id);
    let shard_data = storage
        .get(&key)
        .await
        .map_err(|e| map_storage_error(&key, e))?;
    MDBShardFile::parse(&shard_data).map_err(|e| {
        ReconstructionError::Parse(format!("failed to parse shard {}: {}", shard_id, e))
    })
}

fn cached_xorb_guard<'a>(
    xorb_cache: &'a HashMap<MerkleHash, TempPathGuard>,
    xorb_hash: &MerkleHash,
) -> Result<&'a TempPathGuard, ReconstructionError> {
    xorb_cache.get(xorb_hash).ok_or_else(|| {
        ReconstructionError::Integrity(format!(
            "xorb {} was not present in reconstruction cache",
            xorb_hash.to_hex()
        ))
    })
}

fn map_storage_error(key: &str, error: StorageError) -> ReconstructionError {
    match error {
        StorageError::NotFound(_) => ReconstructionError::Stale(format!("missing {}", key)),
        StorageError::Internal(e) => ReconstructionError::Storage(format!("{}: {}", key, e)),
        StorageError::InvalidArgument(e) => ReconstructionError::Storage(format!("{}: {}", key, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::compression::CompressionScheme;
    use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
    use crate::format::xorb_builder::XorbBuilder;
    use crate::hash::compute_data_hash;
    use crate::index::FileShardRef;
    use crate::storage::StorageBackend;
    use crate::storage::local::LocalStorage;
    use crate::types::MerkleHash;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn build_single_file_shard(raw: &[u8], scheme: CompressionScheme) -> (Vec<u8>, MerkleHash) {
        let mut xb = XorbBuilder::new(scheme);
        let raw_hash = compute_data_hash(raw);
        let (serialized_hash, _compressed_len) = xb.add_chunk(raw).unwrap();
        let xorb = xb.build().unwrap();

        let file_oid = format!("{:x}", Sha256::digest(raw));
        let file_hash = MerkleHash::from_hex(&file_oid).unwrap();

        let mut sb = ShardBuilder::new();
        let xorb_index = sb
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                vec![XorbChunkBuildEntry {
                    chunk_hash: serialized_hash,
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: raw.len() as u32,
                }],
                vec![raw_hash],
            )
            .unwrap();
        sb.add_file(
            file_hash,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: raw.len() as u32,
            }],
        );

        (sb.build().unwrap(), xorb.xorb_hash)
    }

    #[tokio::test]
    async fn test_reconstruct_verified_file_uses_only_requested_file_segments() {
        let raw_a = b"aaa";
        let raw_b = b"bbb";
        let mut xb = XorbBuilder::new(CompressionScheme::None);
        let mut xorb_chunks = Vec::new();
        let mut raw_hashes = Vec::new();
        let mut offset = 0u32;
        for raw in [raw_a.as_slice(), raw_b.as_slice()] {
            raw_hashes.push(compute_data_hash(raw));
            let (serialized_hash, compressed_len) = xb.add_chunk(raw).unwrap();
            xorb_chunks.push(XorbChunkBuildEntry {
                chunk_hash: serialized_hash,
                chunk_byte_range_start: offset,
                unpacked_segment_bytes: raw.len() as u32,
            });
            offset += 8 + compressed_len;
        }
        let xorb = xb.build().unwrap();

        let file_a_oid = format!("{:x}", Sha256::digest(raw_a));
        let file_b_oid = format!("{:x}", Sha256::digest(raw_b));
        let file_a = MerkleHash::from_hex(&file_a_oid).unwrap();
        let file_b = MerkleHash::from_hex(&file_b_oid).unwrap();

        let mut sb = ShardBuilder::new();
        let xorb_index = sb
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                xorb_chunks,
                raw_hashes,
            )
            .unwrap();
        sb.add_file(
            file_a,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: raw_a.len() as u32,
            }],
        );
        sb.add_file(
            file_b,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 1,
                chunk_index_end: 2,
                unpacked_segment_bytes: raw_b.len() as u32,
            }],
        );

        let shard_data = sb.build().unwrap();
        let shard_id = compute_data_hash(&shard_data).to_hex();
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        storage
            .put(
                &format!("xorbs/{}", xorb.xorb_hash.to_hex()),
                bytes::Bytes::from(xorb.data),
            )
            .await
            .unwrap();
        storage
            .put(
                &format!("shards/{}", shard_id),
                bytes::Bytes::from(shard_data),
            )
            .await
            .unwrap();

        let result = reconstruct_verified_file_to_temp(
            &file_b_oid,
            vec![FileShardRef {
                shard_id,
                file_index: 1,
            }],
            &*storage,
            dir.path(),
        )
        .await
        .unwrap();

        let bytes = tokio::fs::read(result.path()).await.unwrap();
        assert_eq!(bytes, raw_b);
    }

    #[tokio::test]
    async fn test_reconstruct_verified_file_treats_multiple_refs_as_candidates() {
        let raw = b"same file may appear in multiple verified shards";
        let file_oid = format!("{:x}", Sha256::digest(raw));
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

        let (shard_data_a, xorb_hash_a) = build_single_file_shard(raw, CompressionScheme::None);
        let shard_id_a = compute_data_hash(&shard_data_a).to_hex();
        let (xorb_data_a, rebuilt_xorb_hash_a) = {
            let mut xb = XorbBuilder::new(CompressionScheme::None);
            xb.add_chunk(raw).unwrap();
            let xorb = xb.build().unwrap();
            (xorb.data, xorb.xorb_hash)
        };
        assert_eq!(xorb_hash_a, rebuilt_xorb_hash_a);

        let (shard_data_b, xorb_hash_b) = build_single_file_shard(raw, CompressionScheme::LZ4);
        let shard_id_b = compute_data_hash(&shard_data_b).to_hex();
        let (xorb_data_b, rebuilt_xorb_hash_b) = {
            let mut xb = XorbBuilder::new(CompressionScheme::LZ4);
            xb.add_chunk(raw).unwrap();
            let xorb = xb.build().unwrap();
            (xorb.data, xorb.xorb_hash)
        };
        assert_eq!(xorb_hash_b, rebuilt_xorb_hash_b);

        storage
            .put(
                &format!("xorbs/{}", xorb_hash_a.to_hex()),
                bytes::Bytes::from(xorb_data_a),
            )
            .await
            .unwrap();
        storage
            .put(
                &format!("shards/{}", shard_id_a),
                bytes::Bytes::from(shard_data_a),
            )
            .await
            .unwrap();
        storage
            .put(
                &format!("xorbs/{}", xorb_hash_b.to_hex()),
                bytes::Bytes::from(xorb_data_b),
            )
            .await
            .unwrap();
        storage
            .put(
                &format!("shards/{}", shard_id_b),
                bytes::Bytes::from(shard_data_b),
            )
            .await
            .unwrap();

        let result = reconstruct_verified_file_to_temp(
            &file_oid,
            vec![
                FileShardRef {
                    shard_id: shard_id_a,
                    file_index: 0,
                },
                FileShardRef {
                    shard_id: shard_id_b,
                    file_index: 0,
                },
            ],
            &*storage,
            dir.path(),
        )
        .await
        .unwrap();

        let bytes = tokio::fs::read(result.path()).await.unwrap();
        assert_eq!(bytes, raw);
    }

    #[test]
    fn test_cached_xorb_guard_reports_missing_cache_entry() {
        let cache = HashMap::new();
        let missing_hash = MerkleHash::from([42u8; 32]);

        let err = cached_xorb_guard(&cache, &missing_hash).expect_err("missing xorb should fail");

        assert!(matches!(err, ReconstructionError::Integrity(_)));
        assert!(err.to_string().contains(&missing_hash.to_hex()));
    }
}
