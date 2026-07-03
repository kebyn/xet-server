use std::collections::{HashMap, HashSet};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::format::shard::MDBShardFile;
use crate::format::xorb::verify_xorb_from_file;
use crate::hash::{compute_data_hash, file_hash as compute_file_hash};
use crate::index::{VerifiedChunkMapping, VerifiedFileMapping, VerifiedShardRegistration};
use crate::reconstruction_plan::build_file_chunk_plan;
use crate::storage::StorageBackend;
use crate::types::MerkleHash;
use crate::util::StreamingHasher;
use crate::xorb_reader::{TempPathGuard, extract_chunk_verified_from_file};

/// Validate shard declarations against stored xorb contents before index registration.
///
/// A shard is accepted only if every declared file reconstructs from the referenced
/// xorbs and the declared file hash matches one supported whole-file hash form.
pub async fn validate_shard_for_index(
    shard_id: &str,
    shard: &MDBShardFile,
    storage: &dyn StorageBackend,
    temp_dir: &Path,
) -> Result<VerifiedShardRegistration, String> {
    tokio::fs::create_dir_all(temp_dir).await.map_err(|e| {
        format!(
            "Failed to create shard validation temp dir {}: {}",
            temp_dir.display(),
            e
        )
    })?;

    let mut xorb_temps: HashMap<MerkleHash, TempPathGuard> = HashMap::new();
    for xorb_entry in &shard.xorb_entries {
        if xorb_temps.contains_key(&xorb_entry.xorb_hash) {
            continue;
        }

        let xorb_hash = xorb_entry.xorb_hash;
        let xorb_hash_hex = xorb_hash.to_hex();
        let xorb_key = format!("xorbs/{}", xorb_hash_hex);
        let temp_path = temp_dir.join(format!(
            "validate-xorb-{}-{}.tmp",
            xorb_hash_hex,
            uuid::Uuid::new_v4()
        ));
        let temp_guard = TempPathGuard::new(temp_path);

        storage
            .download_to_path(&xorb_key, temp_guard.path())
            .await
            .map_err(|e| format!("Failed to download xorb {}: {}", xorb_hash_hex, e))?;

        verify_xorb_from_file(temp_guard.path())
            .map_err(|e| format!("Failed to verify xorb {}: {}", xorb_hash_hex, e))?;

        xorb_temps.insert(xorb_hash, temp_guard);
    }

    let mut files = Vec::with_capacity(shard.file_entries.len());
    let mut validated_chunks: HashSet<(MerkleHash, u32)> = HashSet::new();

    for (file_index, file_entry) in shard.file_entries.iter().enumerate() {
        let declared_file_hash = file_entry.file_hash;
        let plan =
            build_file_chunk_plan(shard, &declared_file_hash, Some(file_index)).map_err(|e| {
                format!(
                    "Failed to build validation plan for file {} at index {}: {}",
                    declared_file_hash, file_index, e
                )
            })?;

        let mut sha256 = Sha256::new();
        let mut whole_file_blake3 = StreamingHasher::new();
        let mut raw_chunk_hashes_and_sizes = Vec::with_capacity(plan.chunks.len());

        for planned in &plan.chunks {
            let xorb_hash_hex = planned.xorb_hash.to_hex();
            let xorb_temp = xorb_temps.get(&planned.xorb_hash).ok_or_else(|| {
                format!(
                    "File {} references xorb {} that was not downloaded",
                    declared_file_hash, xorb_hash_hex
                )
            })?;

            let mut xorb_file = tokio::fs::File::open(xorb_temp.path()).await.map_err(|e| {
                format!(
                    "Failed to open temp xorb {} for validation: {}",
                    xorb_hash_hex, e
                )
            })?;

            let raw_chunk = extract_chunk_verified_from_file(
                &mut xorb_file,
                planned.chunk_byte_range_start as u64,
                planned.unpacked_segment_bytes,
                &planned.serialized_chunk_hash,
            )
            .await
            .map_err(|e| {
                format!(
                    "Failed to extract xorb {} chunk {} for file {}: {}",
                    xorb_hash_hex, planned.xorb_chunk_index, declared_file_hash, e
                )
            })?;

            let actual_raw_chunk_hash = compute_data_hash(&raw_chunk);
            if actual_raw_chunk_hash != planned.raw_chunk_hash {
                return Err(format!(
                    "Raw chunk hash mismatch for file {} xorb {} chunk {}: shard declares {}, reconstructed {}",
                    declared_file_hash,
                    xorb_hash_hex,
                    planned.xorb_chunk_index,
                    planned.raw_chunk_hash,
                    actual_raw_chunk_hash
                ));
            }

            sha256.update(&raw_chunk);
            whole_file_blake3.update(&raw_chunk);
            raw_chunk_hashes_and_sizes.push((actual_raw_chunk_hash, raw_chunk.len() as u64));
            validated_chunks.insert((planned.xorb_hash, planned.xorb_chunk_index));
        }

        let declared_file_hash_hex = declared_file_hash.to_hex();
        let sha256_hex = hex::encode(sha256.finalize());
        let blake3_hash = whole_file_blake3.finalize();
        let xet_file_hash = compute_file_hash(&raw_chunk_hashes_and_sizes);

        if declared_file_hash_hex != sha256_hex
            && declared_file_hash != blake3_hash
            && declared_file_hash != xet_file_hash
        {
            return Err(format!(
                "File hash mismatch for file index {}: shard declares {}, reconstructed sha256 {}, keyed blake3 {}, xet file hash {}",
                file_index, declared_file_hash, sha256_hex, blake3_hash, xet_file_hash
            ));
        }

        files.push(VerifiedFileMapping {
            file_hash: declared_file_hash_hex,
            file_index,
        });
    }

    let chunks = shard
        .chunk_lookup_entries
        .iter()
        .filter(|entry| validated_chunks.contains(&(entry.xorb_hash, entry.chunk_index)))
        .map(|entry| VerifiedChunkMapping {
            chunk_hash: entry.chunk_hash.to_hex(),
            xorb_hash: entry.xorb_hash.to_hex(),
            chunk_index: entry.chunk_index,
        })
        .collect();

    Ok(VerifiedShardRegistration {
        shard_id: shard_id.to_string(),
        files,
        chunks,
    })
}

#[cfg(test)]
mod tests {
    use super::validate_shard_for_index;
    use bytes::Bytes;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::format::compression::CompressionScheme;
    use crate::format::shard::MDBShardFile;
    use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
    use crate::format::xorb_builder::XorbBuilder;
    use crate::hash::compute_data_hash;
    use crate::storage::StorageBackend;
    use crate::storage::local::LocalStorage;
    use crate::types::MerkleHash;

    fn sha256_merkle_hash(data: &[u8]) -> MerkleHash {
        let digest = Sha256::digest(data);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        MerkleHash::from(bytes)
    }

    fn build_one_chunk_xorb_and_shard(
        raw_chunk: &[u8],
        declared_file_hash: MerkleHash,
    ) -> (Vec<u8>, MDBShardFile, MerkleHash, MerkleHash) {
        let mut xorb_builder = XorbBuilder::new(CompressionScheme::None);
        let (serialized_chunk_hash, compressed_len) = xorb_builder.add_chunk(raw_chunk).unwrap();
        let xorb = xorb_builder.build().unwrap();
        let raw_chunk_hash = compute_data_hash(raw_chunk);

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
                vec![raw_chunk_hash],
            )
            .unwrap();

        assert_eq!(compressed_len as usize, raw_chunk.len());
        shard_builder.add_file(
            declared_file_hash,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: raw_chunk.len() as u32,
            }],
        );

        let shard_data = shard_builder.build().unwrap();
        let shard = MDBShardFile::parse(&shard_data).unwrap();
        (xorb.data, shard, xorb.xorb_hash, raw_chunk_hash)
    }

    #[tokio::test]
    async fn test_validate_shard_rejects_missing_xorb() {
        let raw_chunk = b"single chunk stored in a xorb";
        let declared_file_hash = sha256_merkle_hash(raw_chunk);
        let (_xorb_data, shard, _xorb_hash, _raw_chunk_hash) =
            build_one_chunk_xorb_and_shard(raw_chunk, declared_file_hash);

        let storage_dir = tempdir().unwrap();
        let temp_dir = tempdir().unwrap();
        let storage = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();

        let err = validate_shard_for_index("test-shard", &shard, &storage, temp_dir.path())
            .await
            .unwrap_err();

        assert!(err.contains("xorb"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_validate_shard_registers_only_matching_file_hash() {
        let raw_chunk = b"file content addressed by sha256";
        let declared_file_hash = sha256_merkle_hash(raw_chunk);
        let (xorb_data, shard, xorb_hash, raw_chunk_hash) =
            build_one_chunk_xorb_and_shard(raw_chunk, declared_file_hash);

        let storage_dir = tempdir().unwrap();
        let temp_dir = tempdir().unwrap();
        let storage = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        storage
            .put(
                &format!("xorbs/{}", xorb_hash.to_hex()),
                Bytes::from(xorb_data),
            )
            .await
            .unwrap();

        let registration =
            validate_shard_for_index("test-shard", &shard, &storage, temp_dir.path())
                .await
                .unwrap();

        assert_eq!(registration.shard_id, "test-shard");
        assert_eq!(registration.files.len(), 1);
        assert_eq!(registration.files[0].file_hash, declared_file_hash.to_hex());
        assert_eq!(registration.files[0].file_index, 0);
        assert_eq!(registration.chunks.len(), 1);
        assert_eq!(registration.chunks[0].chunk_hash, raw_chunk_hash.to_hex());
        assert_eq!(registration.chunks[0].xorb_hash, xorb_hash.to_hex());
        assert_eq!(registration.chunks[0].chunk_index, 0);
    }
}
