use std::collections::{HashMap, HashSet};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::format::shard::MDBShardFile;
use crate::format::xorb::{VerifiedXorbInfo, verify_xorb_from_file_with_info};
use crate::hash::{compute_data_hash, file_hash as compute_file_hash};
use crate::index::{VerifiedChunkMapping, VerifiedFileMapping, VerifiedShardRegistration};
use crate::reconstruction_plan::build_file_chunk_plan;
use crate::storage::StorageBackend;
use crate::types::MerkleHash;
use crate::util::StreamingHasher;
use crate::xorb_reader::{TempPathGuard, extract_chunk_verified_from_file};

struct ValidatedXorb {
    temp_guard: TempPathGuard,
    info: VerifiedXorbInfo,
}

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

    let mut validated_xorbs: HashMap<MerkleHash, ValidatedXorb> = HashMap::new();
    let mut xorb_chunk_offset = 0usize;
    for (xorb_entry_index, xorb_entry) in shard.xorb_entries.iter().enumerate() {
        if validated_xorbs.contains_key(&xorb_entry.xorb_hash) {
            return Err(format!(
                "Duplicate xorb {} in shard validation",
                xorb_entry.xorb_hash
            ));
        }

        let num_entries = xorb_entry.num_entries as usize;
        let xorb_chunk_end = xorb_chunk_offset.checked_add(num_entries).ok_or_else(|| {
            format!(
                "Xorb {} chunk table offset overflow at entry {}",
                xorb_entry.xorb_hash, xorb_entry_index
            )
        })?;
        if xorb_chunk_end > shard.xorb_chunk_entries.len() {
            return Err(format!(
                "Xorb {} declares {} chunks at offset {}, beyond shard chunk table length {}",
                xorb_entry.xorb_hash,
                num_entries,
                xorb_chunk_offset,
                shard.xorb_chunk_entries.len()
            ));
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
            .download_to_path(
                &xorb_key,
                temp_guard.try_path().map_err(|e| {
                    format!("Failed to resolve xorb temp path {}: {}", xorb_hash_hex, e)
                })?,
            )
            .await
            .map_err(|e| format!("Failed to download xorb {}: {}", xorb_hash_hex, e))?;

        let xorb_info =
            verify_xorb_from_file_with_info(temp_guard.try_path().map_err(|e| {
                format!("Failed to resolve xorb temp path {}: {}", xorb_hash_hex, e)
            })?)
            .map_err(|e| format!("Failed to verify xorb {}: {}", xorb_hash_hex, e))?;

        if xorb_info.xorb_hash != xorb_hash {
            return Err(format!(
                "Xorb hash mismatch for shard entry {}: shard declares {}, stored object verifies as {}",
                xorb_entry_index, xorb_hash, xorb_info.xorb_hash
            ));
        }

        if xorb_entry.num_entries as usize != xorb_info.chunks.len() {
            return Err(format!(
                "Xorb chunk count mismatch for {}: shard declares {}, verified xorb has {}",
                xorb_hash,
                xorb_entry.num_entries,
                xorb_info.chunks.len()
            ));
        }

        if xorb_entry.num_bytes_in_xorb as u64 != xorb_info.total_unpacked_bytes {
            return Err(format!(
                "Xorb size mismatch for {}: shard declares {} unpacked bytes, verified xorb has {}",
                xorb_hash, xorb_entry.num_bytes_in_xorb, xorb_info.total_unpacked_bytes
            ));
        }

        if xorb_entry.num_bytes_on_disk as u64 != xorb_info.total_compressed_payload_bytes {
            return Err(format!(
                "Xorb size mismatch for {}: shard declares {} compressed payload bytes, verified xorb has {}",
                xorb_hash, xorb_entry.num_bytes_on_disk, xorb_info.total_compressed_payload_bytes
            ));
        }

        let shard_chunk_entries = &shard.xorb_chunk_entries[xorb_chunk_offset..xorb_chunk_end];
        for (chunk_index, (shard_chunk, verified_chunk)) in shard_chunk_entries
            .iter()
            .zip(xorb_info.chunks.iter())
            .enumerate()
        {
            if shard_chunk.chunk_hash != verified_chunk.serialized_chunk_hash {
                return Err(format!(
                    "Xorb chunk metadata mismatch for {} chunk {}: shard declares serialized hash {}, verified xorb has {}",
                    xorb_hash,
                    chunk_index,
                    shard_chunk.chunk_hash,
                    verified_chunk.serialized_chunk_hash
                ));
            }

            if shard_chunk.chunk_byte_range_start as u64 != verified_chunk.serialized_start {
                return Err(format!(
                    "Xorb chunk metadata mismatch for {} chunk {}: shard declares start {}, verified xorb has {}",
                    xorb_hash,
                    chunk_index,
                    shard_chunk.chunk_byte_range_start,
                    verified_chunk.serialized_start
                ));
            }

            if shard_chunk.unpacked_segment_bytes as u64 != verified_chunk.unpacked_len {
                return Err(format!(
                    "Xorb chunk metadata mismatch for {} chunk {}: shard declares unpacked length {}, verified xorb has {}",
                    xorb_hash,
                    chunk_index,
                    shard_chunk.unpacked_segment_bytes,
                    verified_chunk.unpacked_len
                ));
            }
        }

        validated_xorbs.insert(
            xorb_hash,
            ValidatedXorb {
                temp_guard,
                info: xorb_info,
            },
        );
        xorb_chunk_offset = xorb_chunk_end;
    }

    if xorb_chunk_offset != shard.xorb_chunk_entries.len() {
        return Err(format!(
            "Shard has {} unclaimed xorb chunk entries after validation",
            shard.xorb_chunk_entries.len() - xorb_chunk_offset
        ));
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
            let validated_xorb = validated_xorbs.get(&planned.xorb_hash).ok_or_else(|| {
                format!(
                    "File {} references xorb {} that was not downloaded",
                    declared_file_hash, xorb_hash_hex
                )
            })?;

            if planned.xorb_chunk_index as usize >= validated_xorb.info.chunks.len() {
                return Err(format!(
                    "File {} references xorb {} chunk {} beyond verified chunk count {}",
                    declared_file_hash,
                    xorb_hash_hex,
                    planned.xorb_chunk_index,
                    validated_xorb.info.chunks.len()
                ));
            }

            let xorb_path = validated_xorb.temp_guard.try_path().map_err(|e| {
                format!("Failed to resolve xorb temp path {}: {}", xorb_hash_hex, e)
            })?;
            let mut xorb_file = tokio::fs::File::open(xorb_path).await.map_err(|e| {
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

    struct BuiltXorb {
        data: Vec<u8>,
        xorb_hash: MerkleHash,
        chunks: Vec<XorbChunkBuildEntry>,
        raw_hashes: Vec<MerkleHash>,
        total_uncompressed_size: u32,
        total_compressed_size: u32,
    }

    fn sha256_merkle_hash(data: &[u8]) -> MerkleHash {
        let digest = Sha256::digest(data);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        MerkleHash::from(bytes)
    }

    fn build_xorb(raw_chunks: &[&[u8]]) -> BuiltXorb {
        let mut xorb_builder = XorbBuilder::new(CompressionScheme::None);
        let mut chunk_entries = Vec::new();
        let mut raw_hashes = Vec::new();
        let mut offset = 0u32;

        for raw_chunk in raw_chunks {
            let (serialized_chunk_hash, compressed_len) =
                xorb_builder.add_chunk(raw_chunk).unwrap();
            chunk_entries.push(XorbChunkBuildEntry {
                chunk_hash: serialized_chunk_hash,
                chunk_byte_range_start: offset,
                unpacked_segment_bytes: raw_chunk.len() as u32,
            });
            raw_hashes.push(compute_data_hash(raw_chunk));
            offset += 8 + compressed_len;
        }

        let xorb = xorb_builder.build().unwrap();
        BuiltXorb {
            data: xorb.data,
            xorb_hash: xorb.xorb_hash,
            chunks: chunk_entries,
            raw_hashes,
            total_uncompressed_size: xorb.total_uncompressed_size as u32,
            total_compressed_size: xorb.total_compressed_size as u32,
        }
    }

    fn build_shard_from_xorb(
        declared_xorb_hash: MerkleHash,
        xorb: &BuiltXorb,
        declared_file_hash: MerkleHash,
        chunk_entries: Vec<XorbChunkBuildEntry>,
        raw_hashes: Vec<MerkleHash>,
        num_bytes_in_xorb: u32,
        num_bytes_on_disk: u32,
    ) -> MDBShardFile {
        let mut shard_builder = ShardBuilder::new();
        let xorb_index = shard_builder
            .add_xorb_with_raw_chunk_hashes(
                declared_xorb_hash,
                num_bytes_in_xorb,
                num_bytes_on_disk,
                chunk_entries,
                raw_hashes,
            )
            .unwrap();

        shard_builder.add_file(
            declared_file_hash,
            vec![FileSegment {
                xorb_hash: declared_xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: xorb.chunks.len() as u32,
                unpacked_segment_bytes: xorb.total_uncompressed_size,
            }],
        );

        let shard_data = shard_builder.build().unwrap();
        MDBShardFile::parse(&shard_data).unwrap()
    }

    fn build_one_chunk_xorb_and_shard(
        raw_chunk: &[u8],
        declared_file_hash: MerkleHash,
    ) -> (Vec<u8>, MDBShardFile, MerkleHash, MerkleHash) {
        let xorb = build_xorb(&[raw_chunk]);
        let shard = build_shard_from_xorb(
            xorb.xorb_hash,
            &xorb,
            declared_file_hash,
            xorb.chunks.clone(),
            xorb.raw_hashes.clone(),
            xorb.total_uncompressed_size,
            xorb.total_compressed_size,
        );
        (xorb.data, shard, xorb.xorb_hash, xorb.raw_hashes[0])
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

    #[tokio::test]
    async fn test_validate_shard_rejects_mismatched_xorb_identity() {
        let declared_xorb = build_xorb(&[b"declared identity"]);
        let stored_xorb = build_xorb(&[b"stored under the wrong key"]);
        let declared_file_hash = sha256_merkle_hash(b"stored under the wrong key");
        let shard = build_shard_from_xorb(
            declared_xorb.xorb_hash,
            &stored_xorb,
            declared_file_hash,
            stored_xorb.chunks.clone(),
            stored_xorb.raw_hashes.clone(),
            stored_xorb.total_uncompressed_size,
            stored_xorb.total_compressed_size,
        );

        let storage_dir = tempdir().unwrap();
        let temp_dir = tempdir().unwrap();
        let storage = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        storage
            .put(
                &format!("xorbs/{}", declared_xorb.xorb_hash.to_hex()),
                Bytes::from(stored_xorb.data),
            )
            .await
            .unwrap();

        let err = validate_shard_for_index("test-shard", &shard, &storage, temp_dir.path())
            .await
            .unwrap_err();

        let err = err.to_lowercase();
        assert!(
            err.contains("xorb") && err.contains("mismatch"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_validate_shard_rejects_swapped_chunk_metadata() {
        let first = b"first chunk content".as_slice();
        let second = b"second chunk content".as_slice();
        let xorb = build_xorb(&[first, second]);
        let declared_file_hash = sha256_merkle_hash(&[second, first].concat());

        let mut swapped_chunks = xorb.chunks.clone();
        swapped_chunks.swap(0, 1);
        let mut swapped_raw_hashes = xorb.raw_hashes.clone();
        swapped_raw_hashes.swap(0, 1);
        let shard = build_shard_from_xorb(
            xorb.xorb_hash,
            &xorb,
            declared_file_hash,
            swapped_chunks,
            swapped_raw_hashes,
            xorb.total_uncompressed_size,
            xorb.total_compressed_size,
        );

        let storage_dir = tempdir().unwrap();
        let temp_dir = tempdir().unwrap();
        let storage = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        storage
            .put(
                &format!("xorbs/{}", xorb.xorb_hash.to_hex()),
                Bytes::from(xorb.data),
            )
            .await
            .unwrap();

        let err = validate_shard_for_index("test-shard", &shard, &storage, temp_dir.path())
            .await
            .unwrap_err();

        let err = err.to_lowercase();
        assert!(
            err.contains("chunk") && err.contains("mismatch"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_validate_shard_rejects_xorb_size_mismatch() {
        let raw_chunk = b"content with incorrect declared xorb sizes";
        let declared_file_hash = sha256_merkle_hash(raw_chunk);
        let xorb = build_xorb(&[raw_chunk]);
        let shard = build_shard_from_xorb(
            xorb.xorb_hash,
            &xorb,
            declared_file_hash,
            xorb.chunks.clone(),
            xorb.raw_hashes.clone(),
            xorb.total_uncompressed_size + 1,
            xorb.total_compressed_size,
        );

        let storage_dir = tempdir().unwrap();
        let temp_dir = tempdir().unwrap();
        let storage = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        storage
            .put(
                &format!("xorbs/{}", xorb.xorb_hash.to_hex()),
                Bytes::from(xorb.data),
            )
            .await
            .unwrap();

        let err = validate_shard_for_index("test-shard", &shard, &storage, temp_dir.path())
            .await
            .unwrap_err();

        let err = err.to_lowercase();
        assert!(
            err.contains("xorb") && err.contains("size"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_validate_shard_rejects_xorb_compressed_size_mismatch() {
        let raw_chunk = b"content with incorrect declared compressed payload size";
        let declared_file_hash = sha256_merkle_hash(raw_chunk);
        let xorb = build_xorb(&[raw_chunk]);
        let shard = build_shard_from_xorb(
            xorb.xorb_hash,
            &xorb,
            declared_file_hash,
            xorb.chunks.clone(),
            xorb.raw_hashes.clone(),
            xorb.total_uncompressed_size,
            xorb.total_compressed_size + 1,
        );

        let storage_dir = tempdir().unwrap();
        let temp_dir = tempdir().unwrap();
        let storage = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        storage
            .put(
                &format!("xorbs/{}", xorb.xorb_hash.to_hex()),
                Bytes::from(xorb.data),
            )
            .await
            .unwrap();

        let err = validate_shard_for_index("test-shard", &shard, &storage, temp_dir.path())
            .await
            .unwrap_err();

        let err = err.to_lowercase();
        assert!(
            err.contains("xorb") && err.contains("size"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_validate_shard_rejects_xorb_num_entries_mismatch() {
        let first = b"first chunk for count mismatch".as_slice();
        let second = b"second chunk for count mismatch".as_slice();
        let xorb = build_xorb(&[first, second]);
        let declared_file_hash = sha256_merkle_hash(&[first, second].concat());
        let shard = build_shard_from_xorb(
            xorb.xorb_hash,
            &xorb,
            declared_file_hash,
            vec![xorb.chunks[0].clone()],
            vec![xorb.raw_hashes[0]],
            xorb.total_uncompressed_size,
            xorb.total_compressed_size,
        );

        let storage_dir = tempdir().unwrap();
        let temp_dir = tempdir().unwrap();
        let storage = LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap();
        storage
            .put(
                &format!("xorbs/{}", xorb.xorb_hash.to_hex()),
                Bytes::from(xorb.data),
            )
            .await
            .unwrap();

        let err = validate_shard_for_index("test-shard", &shard, &storage, temp_dir.path())
            .await
            .unwrap_err();

        let err = err.to_lowercase();
        assert!(
            err.contains("xorb") && err.contains("count"),
            "unexpected error: {err}"
        );
    }
}
