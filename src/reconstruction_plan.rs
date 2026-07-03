use std::collections::HashMap;

use crate::error::{Result, XetError};
use crate::format::shard::MDBShardFile;
use crate::types::MerkleHash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub file_hash: MerkleHash,
    pub file_index: usize,
    pub chunks: Vec<PlannedChunk>,
    pub total_unpacked_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedChunk {
    pub xorb_hash: MerkleHash,
    pub xorb_chunk_index: u32,
    pub chunk_byte_range_start: u32,
    pub unpacked_segment_bytes: u32,
    pub serialized_chunk_hash: MerkleHash,
    pub raw_chunk_hash: MerkleHash,
}

pub fn build_file_chunk_plan(
    shard: &MDBShardFile,
    file_hash: &MerkleHash,
    expected_file_index: Option<usize>,
) -> Result<PlannedFile> {
    let file_index = match expected_file_index {
        Some(index) => {
            let entry = shard.file_entries.get(index).ok_or_else(|| {
                XetError::ParseError(format!(
                    "Expected file index {} out of bounds for shard with {} files",
                    index,
                    shard.file_entries.len()
                ))
            })?;
            if entry.file_hash != *file_hash {
                return Err(XetError::ParseError(format!(
                    "Expected file index {} to contain file {}, got {}",
                    index, file_hash, entry.file_hash
                )));
            }
            index
        }
        None => shard
            .file_entries
            .iter()
            .position(|entry| entry.file_hash == *file_hash)
            .ok_or_else(|| {
                XetError::ParseError(format!("File {} not found in shard", file_hash))
            })?,
    };

    let file_entry_start =
        shard.file_entries[..file_index]
            .iter()
            .try_fold(0usize, |acc, entry| {
                acc.checked_add(entry.num_entries as usize)
                    .ok_or_else(|| XetError::ParseError("File entry offset overflow".to_string()))
            })?;
    let file_entry = &shard.file_entries[file_index];
    let file_entry_end = file_entry_start
        .checked_add(file_entry.num_entries as usize)
        .ok_or_else(|| XetError::ParseError("File entry end offset overflow".to_string()))?;
    if file_entry_end > shard.file_data_entries.len() {
        return Err(XetError::ParseError(format!(
            "File {} entries exceed shard file data entries: end {}, len {}",
            file_hash,
            file_entry_end,
            shard.file_data_entries.len()
        )));
    }

    let mut xorb_offsets: HashMap<MerkleHash, (usize, usize)> = HashMap::new();
    let mut xorb_chunk_offset = 0usize;
    for xorb_entry in &shard.xorb_entries {
        if xorb_offsets.contains_key(&xorb_entry.xorb_hash) {
            return Err(XetError::ParseError(format!(
                "duplicate xorb hash {} in shard reconstruction plan",
                xorb_entry.xorb_hash
            )));
        }

        let num_entries = xorb_entry.num_entries as usize;
        if xorb_chunk_offset
            .checked_add(num_entries)
            .is_none_or(|end| end > shard.xorb_chunk_entries.len())
        {
            return Err(XetError::ParseError(format!(
                "Xorb {} chunk entries exceed shard chunk table",
                xorb_entry.xorb_hash
            )));
        }
        xorb_offsets.insert(xorb_entry.xorb_hash, (xorb_chunk_offset, num_entries));
        xorb_chunk_offset = xorb_chunk_offset
            .checked_add(num_entries)
            .ok_or_else(|| XetError::ParseError("Xorb chunk offset overflow".to_string()))?;
    }

    let mut raw_chunk_lookup: HashMap<(MerkleHash, u32), MerkleHash> = HashMap::new();
    for entry in &shard.chunk_lookup_entries {
        let key = (entry.xorb_hash, entry.chunk_index);
        if raw_chunk_lookup.insert(key, entry.chunk_hash).is_some() {
            return Err(XetError::ParseError(format!(
                "duplicate raw chunk lookup for xorb {} chunk {}",
                entry.xorb_hash, entry.chunk_index
            )));
        }
    }

    let mut chunks = Vec::new();
    let mut total_unpacked_bytes = 0u64;

    for segment in &shard.file_data_entries[file_entry_start..file_entry_end] {
        if segment.chunk_index_end < segment.chunk_index_start {
            return Err(XetError::ParseError(format!(
                "File {} has invalid chunk range {}..{} for xorb {}",
                file_hash, segment.chunk_index_start, segment.chunk_index_end, segment.xorb_hash
            )));
        }

        let (xorb_global_start, xorb_num_entries) = xorb_offsets
            .get(&segment.xorb_hash)
            .copied()
            .ok_or_else(|| {
            XetError::ParseError(format!(
                "File {} references missing xorb {}",
                file_hash, segment.xorb_hash
            ))
        })?;
        let local_start = segment.chunk_index_start as usize;
        let local_end = segment.chunk_index_end as usize;
        if local_end > xorb_num_entries {
            return Err(XetError::ParseError(format!(
                "File {} references xorb {} chunk range {}..{} beyond {} chunks",
                file_hash,
                segment.xorb_hash,
                segment.chunk_index_start,
                segment.chunk_index_end,
                xorb_num_entries
            )));
        }

        let mut segment_unpacked_bytes = 0u64;
        for local_chunk_index in local_start..local_end {
            let global_chunk_index = xorb_global_start
                .checked_add(local_chunk_index)
                .ok_or_else(|| XetError::ParseError("Global chunk index overflow".to_string()))?;
            let chunk_entry = shard
                .xorb_chunk_entries
                .get(global_chunk_index)
                .ok_or_else(|| {
                    XetError::ParseError(format!(
                        "Global chunk index {} out of bounds for xorb {}",
                        global_chunk_index, segment.xorb_hash
                    ))
                })?;
            let xorb_chunk_index = local_chunk_index as u32;
            let raw_chunk_hash = raw_chunk_lookup
                .get(&(segment.xorb_hash, xorb_chunk_index))
                .copied()
                .ok_or_else(|| {
                    XetError::ParseError(format!(
                        "Missing raw chunk hash for xorb {} chunk {}",
                        segment.xorb_hash, xorb_chunk_index
                    ))
                })?;

            segment_unpacked_bytes = segment_unpacked_bytes
                .checked_add(chunk_entry.unpacked_segment_bytes as u64)
                .ok_or_else(|| XetError::ParseError("Segment byte total overflow".to_string()))?;
            chunks.push(PlannedChunk {
                xorb_hash: segment.xorb_hash,
                xorb_chunk_index,
                chunk_byte_range_start: chunk_entry.chunk_byte_range_start,
                unpacked_segment_bytes: chunk_entry.unpacked_segment_bytes,
                serialized_chunk_hash: chunk_entry.chunk_hash,
                raw_chunk_hash,
            });
        }

        if segment_unpacked_bytes != segment.unpacked_segment_bytes as u64 {
            return Err(XetError::ParseError(format!(
                "File {} segment byte total mismatch for xorb {}: chunks sum to {}, segment declares {}",
                file_hash,
                segment.xorb_hash,
                segment_unpacked_bytes,
                segment.unpacked_segment_bytes
            )));
        }
        total_unpacked_bytes = total_unpacked_bytes
            .checked_add(segment_unpacked_bytes)
            .ok_or_else(|| XetError::ParseError("File byte total overflow".to_string()))?;
    }

    Ok(PlannedFile {
        file_hash: *file_hash,
        file_index,
        chunks,
        total_unpacked_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::compression::CompressionScheme;
    use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
    use crate::format::xorb_builder::XorbBuilder;
    use crate::hash::compute_data_hash;
    use crate::types::MerkleHash;

    fn build_two_file_shard() -> (MDBShardFile, MerkleHash, MerkleHash, Vec<MerkleHash>) {
        let chunks = [b"file-a".as_slice(), b"file-b".as_slice()];
        let mut xb = XorbBuilder::new(CompressionScheme::None);
        let mut xorb_chunks = Vec::new();
        let mut raw_hashes = Vec::new();
        let mut offset = 0u32;
        for raw in chunks {
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

        let mut sb = ShardBuilder::new();
        let xorb_index = sb
            .add_xorb_with_raw_chunk_hashes(
                xorb.xorb_hash,
                xorb.total_uncompressed_size as u32,
                xorb.total_compressed_size as u32,
                xorb_chunks,
                raw_hashes.clone(),
            )
            .unwrap();

        let file_a = MerkleHash::from([1u8; 32]);
        let file_b = MerkleHash::from([2u8; 32]);
        sb.add_file(
            file_a,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: 6,
            }],
        );
        sb.add_file(
            file_b,
            vec![FileSegment {
                xorb_hash: xorb.xorb_hash,
                xorb_index,
                chunk_index_start: 1,
                chunk_index_end: 2,
                unpacked_segment_bytes: 6,
            }],
        );

        let shard = MDBShardFile::parse(&sb.build().unwrap()).unwrap();
        (shard, file_a, file_b, raw_hashes)
    }

    fn build_one_file_two_xorb_shard() -> (MDBShardFile, MerkleHash, Vec<MerkleHash>) {
        let mut xb_a = XorbBuilder::new(CompressionScheme::None);
        let raw_a = b"xorb-a".as_slice();
        let raw_hash_a = compute_data_hash(raw_a);
        let (serialized_hash_a, _) = xb_a.add_chunk(raw_a).unwrap();
        let xorb_chunk_a = XorbChunkBuildEntry {
            chunk_hash: serialized_hash_a,
            chunk_byte_range_start: 0,
            unpacked_segment_bytes: raw_a.len() as u32,
        };
        let xorb_a = xb_a.build().unwrap();

        let mut xb_b = XorbBuilder::new(CompressionScheme::None);
        let raw_b = b"xorb-b".as_slice();
        let raw_hash_b = compute_data_hash(raw_b);
        let (serialized_hash_b, _) = xb_b.add_chunk(raw_b).unwrap();
        let xorb_chunk_b = XorbChunkBuildEntry {
            chunk_hash: serialized_hash_b,
            chunk_byte_range_start: 0,
            unpacked_segment_bytes: raw_b.len() as u32,
        };
        let xorb_b = xb_b.build().unwrap();

        let mut sb = ShardBuilder::new();
        let xorb_index_a = sb
            .add_xorb_with_raw_chunk_hashes(
                xorb_a.xorb_hash,
                xorb_a.total_uncompressed_size as u32,
                xorb_a.total_compressed_size as u32,
                vec![xorb_chunk_a],
                vec![raw_hash_a],
            )
            .unwrap();
        let xorb_index_b = sb
            .add_xorb_with_raw_chunk_hashes(
                xorb_b.xorb_hash,
                xorb_b.total_uncompressed_size as u32,
                xorb_b.total_compressed_size as u32,
                vec![xorb_chunk_b],
                vec![raw_hash_b],
            )
            .unwrap();

        let file_hash = MerkleHash::from([3u8; 32]);
        sb.add_file(
            file_hash,
            vec![
                FileSegment {
                    xorb_hash: xorb_a.xorb_hash,
                    xorb_index: xorb_index_a,
                    chunk_index_start: 0,
                    chunk_index_end: 1,
                    unpacked_segment_bytes: raw_a.len() as u32,
                },
                FileSegment {
                    xorb_hash: xorb_b.xorb_hash,
                    xorb_index: xorb_index_b,
                    chunk_index_start: 0,
                    chunk_index_end: 1,
                    unpacked_segment_bytes: raw_b.len() as u32,
                },
            ],
        );

        let shard = MDBShardFile::parse(&sb.build().unwrap()).unwrap();
        (shard, file_hash, vec![raw_hash_a, raw_hash_b])
    }

    fn assert_plan_err_contains(
        shard: &MDBShardFile,
        file_hash: &MerkleHash,
        expected_file_index: Option<usize>,
        expected: &str,
    ) {
        let err = build_file_chunk_plan(shard, file_hash, expected_file_index)
            .expect_err("expected planner to reject malformed shard");
        let message = err.to_string();
        assert!(
            message.contains(expected),
            "expected error containing {expected:?}, got {message:?}"
        );
    }

    #[test]
    fn test_build_file_chunk_plan_selects_only_requested_file() {
        let (shard, file_a, file_b, raw_hashes) = build_two_file_shard();

        let plan_a = build_file_chunk_plan(&shard, &file_a, Some(0)).unwrap();
        assert_eq!(plan_a.file_index, 0);
        assert_eq!(plan_a.chunks.len(), 1);
        assert_eq!(plan_a.chunks[0].raw_chunk_hash, raw_hashes[0]);
        assert_eq!(plan_a.total_unpacked_bytes, 6);

        let plan_b = build_file_chunk_plan(&shard, &file_b, Some(1)).unwrap();
        assert_eq!(plan_b.file_index, 1);
        assert_eq!(plan_b.chunks.len(), 1);
        assert_eq!(plan_b.chunks[0].raw_chunk_hash, raw_hashes[1]);
        assert_eq!(plan_b.total_unpacked_bytes, 6);
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_duplicate_xorb_hash() {
        let (mut shard, file_a, _, _) = build_two_file_shard();
        shard.xorb_entries.push(shard.xorb_entries[0].clone());
        shard
            .xorb_chunk_entries
            .extend(shard.xorb_chunk_entries.clone());

        assert_plan_err_contains(&shard, &file_a, Some(0), "duplicate xorb hash");
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_duplicate_raw_lookup_key() {
        let (mut shard, file_a, _, _) = build_two_file_shard();
        let mut duplicate = shard.chunk_lookup_entries[0].clone();
        duplicate.chunk_hash = MerkleHash::from([9u8; 32]);
        shard.chunk_lookup_entries.push(duplicate);

        assert_plan_err_contains(&shard, &file_a, Some(0), "duplicate raw chunk lookup");
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_expected_file_index_mismatch() {
        let (shard, file_a, _, _) = build_two_file_shard();

        assert_plan_err_contains(&shard, &file_a, Some(1), "Expected file index");
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_missing_xorb_reference() {
        let (mut shard, file_a, _, _) = build_two_file_shard();
        shard.file_data_entries[0].xorb_hash = MerkleHash::from([8u8; 32]);

        assert_plan_err_contains(&shard, &file_a, Some(0), "references missing xorb");
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_invalid_chunk_range() {
        let (mut shard, file_a, _, _) = build_two_file_shard();
        shard.file_data_entries[0].chunk_index_start = 2;
        shard.file_data_entries[0].chunk_index_end = 1;

        assert_plan_err_contains(&shard, &file_a, Some(0), "invalid chunk range");
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_chunk_range_out_of_bounds() {
        let (mut shard, file_a, _, _) = build_two_file_shard();
        shard.file_data_entries[0].chunk_index_end = 3;

        assert_plan_err_contains(&shard, &file_a, Some(0), "beyond 2 chunks");
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_missing_raw_lookup() {
        let (mut shard, file_a, _, _) = build_two_file_shard();
        let xorb_hash = shard.xorb_entries[0].xorb_hash;
        shard
            .chunk_lookup_entries
            .retain(|entry| !(entry.xorb_hash == xorb_hash && entry.chunk_index == 0));

        assert_plan_err_contains(&shard, &file_a, Some(0), "Missing raw chunk hash");
    }

    #[test]
    fn test_build_file_chunk_plan_rejects_segment_byte_total_mismatch() {
        let (mut shard, file_a, _, _) = build_two_file_shard();
        shard.file_data_entries[0].unpacked_segment_bytes = 7;

        assert_plan_err_contains(&shard, &file_a, Some(0), "segment byte total mismatch");
    }

    #[test]
    fn test_build_file_chunk_plan_maps_second_xorb_local_chunk_to_global_entry() {
        let (shard, file_hash, raw_hashes) = build_one_file_two_xorb_shard();

        let plan = build_file_chunk_plan(&shard, &file_hash, Some(0)).unwrap();

        assert_eq!(plan.file_index, 0);
        assert_eq!(plan.chunks.len(), 2);
        assert_eq!(plan.total_unpacked_bytes, 12);
        assert_eq!(plan.chunks[0].xorb_hash, shard.xorb_entries[0].xorb_hash);
        assert_eq!(plan.chunks[0].xorb_chunk_index, 0);
        assert_eq!(
            plan.chunks[0].serialized_chunk_hash,
            shard.xorb_chunk_entries[0].chunk_hash
        );
        assert_eq!(plan.chunks[0].raw_chunk_hash, raw_hashes[0]);
        assert_eq!(plan.chunks[1].xorb_hash, shard.xorb_entries[1].xorb_hash);
        assert_eq!(plan.chunks[1].xorb_chunk_index, 0);
        assert_eq!(
            plan.chunks[1].serialized_chunk_hash,
            shard.xorb_chunk_entries[1].chunk_hash
        );
        assert_eq!(plan.chunks[1].raw_chunk_hash, raw_hashes[1]);
    }
}
