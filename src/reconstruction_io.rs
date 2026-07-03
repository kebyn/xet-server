use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::api::reconstruction::fetch_and_parse_shard;
use crate::hash::{compute_data_hash, file_hash};
use crate::index::FileShardRef;
use crate::reconstruction_plan::build_file_chunk_plan;
use crate::storage::StorageBackend;
use crate::types::MerkleHash;
use crate::util::StreamingHasher;
use crate::xorb_reader::{TempPathGuard, extract_chunk_verified_from_file};

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
) -> Result<VerifiedReconstruction, String> {
    std::fs::create_dir_all(temp_dir)
        .map_err(|e| format!("failed to create reconstruction temp dir: {}", e))?;
    let target_hash =
        MerkleHash::from_hex(file_id).map_err(|e| format!("invalid file id {}: {}", file_id, e))?;
    let output_guard = TempPathGuard::new(temp_dir.join(format!(
        "reconstruct-{}-{}.tmp",
        file_id,
        uuid::Uuid::new_v4()
    )));
    let mut output = tokio::fs::File::create(output_guard.path())
        .await
        .map_err(|e| format!("failed to create reconstruction temp file: {}", e))?;

    let mut sha = Sha256::new();
    let mut whole_blake3 = StreamingHasher::new();
    let mut chunk_nodes: Vec<(MerkleHash, u64)> = Vec::new();
    let mut total_size = 0u64;
    let mut xorb_cache: HashMap<MerkleHash, TempPathGuard> = HashMap::new();

    for file_ref in file_refs {
        let shard = fetch_and_parse_shard(&file_ref.shard_id, storage).await?;
        let plan = build_file_chunk_plan(&shard, &target_hash, Some(file_ref.file_index))
            .map_err(|e| e.to_string())?;

        for planned in plan.chunks {
            if !xorb_cache.contains_key(&planned.xorb_hash) {
                let key = format!("xorbs/{}", planned.xorb_hash.to_hex());
                let guard = TempPathGuard::new(temp_dir.join(format!(
                    "reconstruct-xorb-{}-{}.tmp",
                    planned.xorb_hash.to_hex(),
                    uuid::Uuid::new_v4()
                )));
                storage
                    .download_to_path(&key, guard.path())
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to download xorb {}: {}",
                            planned.xorb_hash.to_hex(),
                            e
                        )
                    })?;
                xorb_cache.insert(planned.xorb_hash, guard);
            }

            let guard = xorb_cache.get(&planned.xorb_hash).unwrap();
            let mut xorb_file = tokio::fs::File::open(guard.path()).await.map_err(|e| {
                format!("failed to open xorb {}: {}", planned.xorb_hash.to_hex(), e)
            })?;
            let bytes = extract_chunk_verified_from_file(
                &mut xorb_file,
                planned.chunk_byte_range_start as u64,
                planned.unpacked_segment_bytes,
                &planned.serialized_chunk_hash,
            )
            .await?;
            let raw_hash = compute_data_hash(&bytes);
            if raw_hash != planned.raw_chunk_hash {
                return Err(format!(
                    "raw chunk hash mismatch for xorb {} chunk {}",
                    planned.xorb_hash.to_hex(),
                    planned.xorb_chunk_index
                ));
            }
            output
                .write_all(&bytes)
                .await
                .map_err(|e| format!("failed to write reconstructed bytes: {}", e))?;
            sha.update(&bytes);
            whole_blake3.update(&bytes);
            total_size += bytes.len() as u64;
            chunk_nodes.push((raw_hash, bytes.len() as u64));
        }
    }

    output
        .sync_all()
        .await
        .map_err(|e| format!("failed to sync reconstructed file: {}", e))?;

    let sha_hex = format!("{:x}", sha.finalize());
    let blake3_hex = whole_blake3.finalize().to_hex();
    let xet_file_hash = file_hash(&chunk_nodes).to_hex();
    if file_id != sha_hex && file_id != blake3_hex && file_id != xet_file_hash {
        return Err(format!(
            "reconstructed file hash mismatch for {}: sha256={}, blake3={}, xet_file_hash={}",
            file_id, sha_hex, blake3_hex, xet_file_hash
        ));
    }

    Ok(VerifiedReconstruction {
        path_guard: output_guard,
        size: total_size,
    })
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
    use tempfile::tempdir;

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
}
