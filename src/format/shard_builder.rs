//! ShardBuilder — assembles file-to-xorb mapping metadata into a valid shard binary.
//!
//! The resulting binary can be parsed by [`MDBShardFile::parse`].
//!
//! # Binary Layout
//!
//! ```text
//! [MDBShardFileHeader: 48 bytes]
//! [File Info Section: per-file FileDataSequenceHeader + FileDataSequenceEntry entries]
//! [Xorb Info Section: per-xorb XorbChunkSequenceHeader + XorbChunkSequenceEntry entries]
//! [Chunk Lookup Section: optional ChunkLookupEntry entries for raw chunk hashes]
//! [MDBShardFileFooter: 208 bytes]
//! ```

use std::io::Cursor;

use crate::error::{Result, XetError};
use crate::format::shard::{
    ChunkLookupEntry, FileDataSequenceEntry, FileDataSequenceHeader, MDBShardFile,
    MDBShardFileFooter, MDBShardFileHeader, XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crate::types::MerkleHash;

/// Size in bytes of the fixed-width shard structures.
const HEADER_SIZE: usize = 48;
const FOOTER_SIZE: usize = 208;
const FILE_HEADER_SIZE: usize = 48;
const FILE_ENTRY_SIZE: usize = 48;
const XORB_HEADER_SIZE: usize = 48;
const XORB_ENTRY_SIZE: usize = 48;
const CHUNK_LOOKUP_ENTRY_SIZE: usize = ChunkLookupEntry::SIZE;

// ---------------------------------------------------------------------------
// Public builder types
// ---------------------------------------------------------------------------

/// Builder that assembles file-to-xorb mapping metadata into a valid shard binary.
///
/// # Usage
///
/// 1. Call [`add_xorb`](Self::add_xorb) for each unique xorb to register its chunks.
/// 2. Call [`add_file`](Self::add_file) for each file, referencing xorbs by index.
/// 3. Call [`build`](Self::build) to produce the final shard binary.
pub struct ShardBuilder {
    files: Vec<FileBuildEntry>,
    xorbs: Vec<XorbBuildEntry>,
    chunk_lookup_entries: Vec<ChunkLookupEntry>,
}

/// A file registered with the builder, consisting of one or more segments
/// that each reference a xorb.
#[derive(Clone, Debug)]
pub struct FileBuildEntry {
    pub file_hash: MerkleHash,
    pub segments: Vec<FileSegment>,
}

/// A mapping from a contiguous range of chunks in a xorb to a portion of a file.
#[derive(Clone, Debug)]
pub struct FileSegment {
    pub xorb_hash: MerkleHash,
    /// Index into the builder's xorbs vec (returned by [`ShardBuilder::add_xorb`]).
    pub xorb_index: usize,
    pub chunk_index_start: u32,
    pub chunk_index_end: u32,
    pub unpacked_segment_bytes: u32,
}

/// A xorb registered with the builder, containing its chunk descriptions.
#[derive(Clone, Debug)]
pub struct XorbBuildEntry {
    pub xorb_hash: MerkleHash,
    /// Total uncompressed bytes stored in this xorb.
    pub num_bytes_in_xorb: u32,
    /// Total compressed (on-disk) bytes for this xorb.
    pub num_bytes_on_disk: u32,
    pub chunks: Vec<XorbChunkBuildEntry>,
}

/// A single chunk within a xorb.
#[derive(Clone, Debug)]
pub struct XorbChunkBuildEntry {
    pub chunk_hash: MerkleHash,
    pub chunk_byte_range_start: u32,
    pub unpacked_segment_bytes: u32,
}

// ---------------------------------------------------------------------------
// Builder implementation
// ---------------------------------------------------------------------------

impl ShardBuilder {
    /// Create a new, empty builder.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            xorbs: Vec::new(),
            chunk_lookup_entries: Vec::new(),
        }
    }

    fn add_xorb_unchecked(
        &mut self,
        xorb_hash: MerkleHash,
        num_bytes_in_xorb: u32,
        num_bytes_on_disk: u32,
        chunks: Vec<XorbChunkBuildEntry>,
    ) -> usize {
        let index = self.xorbs.len();
        self.xorbs.push(XorbBuildEntry {
            xorb_hash,
            num_bytes_in_xorb,
            num_bytes_on_disk,
            chunks,
        });
        index
    }

    /// Register a xorb and persist raw chunk hashes for global deduplication.
    ///
    /// `XorbChunkBuildEntry::chunk_hash` is the serialized chunk hash used by
    /// xorb integrity verification. `raw_chunk_hashes` are hashes of the
    /// original uncompressed chunk bytes and are stored in the shard's chunk
    /// lookup section so the metadata index can be rebuilt with the same dedup
    /// keys after restart.
    pub fn add_xorb_with_raw_chunk_hashes(
        &mut self,
        xorb_hash: MerkleHash,
        num_bytes_in_xorb: u32,
        num_bytes_on_disk: u32,
        chunks: Vec<XorbChunkBuildEntry>,
        raw_chunk_hashes: Vec<MerkleHash>,
    ) -> Result<usize> {
        if chunks.len() != raw_chunk_hashes.len() {
            return Err(XetError::ParseError(format!(
                "raw chunk hash count mismatch: got {}, expected {}",
                raw_chunk_hashes.len(),
                chunks.len()
            )));
        }

        let index =
            self.add_xorb_unchecked(xorb_hash, num_bytes_in_xorb, num_bytes_on_disk, chunks);
        for (chunk_index, chunk_hash) in raw_chunk_hashes.into_iter().enumerate() {
            self.chunk_lookup_entries.push(ChunkLookupEntry {
                chunk_hash,
                xorb_hash,
                chunk_index: chunk_index as u32,
            });
        }

        Ok(index)
    }

    /// Register a file and its xorb-to-segment mapping.
    pub fn add_file(&mut self, file_hash: MerkleHash, segments: Vec<FileSegment>) {
        self.files.push(FileBuildEntry {
            file_hash,
            segments,
        });
    }

    /// Build the complete shard binary.
    ///
    /// The returned bytes can be parsed by [`MDBShardFile::parse`] and all fields
    /// round-trip correctly.
    pub fn build(self) -> Result<Vec<u8>> {
        // ---- calculate section byte sizes ----
        let file_section_size: usize = self
            .files
            .iter()
            .map(|f| FILE_HEADER_SIZE + f.segments.len() * FILE_ENTRY_SIZE)
            .sum();

        let xorb_section_size: usize = self
            .xorbs
            .iter()
            .map(|x| XORB_HEADER_SIZE + x.chunks.len() * XORB_ENTRY_SIZE)
            .sum();
        let chunk_lookup_section_size = self.chunk_lookup_entries.len() * CHUNK_LOOKUP_ENTRY_SIZE;

        // ---- section offsets ----
        let file_info_offset: u64 = HEADER_SIZE as u64;
        let xorb_info_offset: u64 = (HEADER_SIZE + file_section_size) as u64;
        let chunk_lookup_offset: u64 = (HEADER_SIZE + file_section_size + xorb_section_size) as u64;
        let footer_offset: u64 =
            (HEADER_SIZE + file_section_size + xorb_section_size + chunk_lookup_section_size)
                as u64;
        let total_size: usize = HEADER_SIZE
            + file_section_size
            + xorb_section_size
            + chunk_lookup_section_size
            + FOOTER_SIZE;

        let mut buf: Vec<u8> = Vec::with_capacity(total_size);

        // ---- (a) header ----
        let header = MDBShardFileHeader::default();
        header.serialize(&mut buf).map_err(XetError::IoError)?;

        // ---- (b) file info section ----
        for file in &self.files {
            let fh = FileDataSequenceHeader {
                file_hash: file.file_hash,
                file_flags: 0,
                num_entries: file.segments.len() as u32,
            };
            fh.serialize(&mut buf).map_err(XetError::IoError)?;

            for seg in &file.segments {
                let fe = FileDataSequenceEntry {
                    xorb_hash: seg.xorb_hash,
                    xorb_flags: 0,
                    unpacked_segment_bytes: seg.unpacked_segment_bytes,
                    chunk_index_start: seg.chunk_index_start,
                    chunk_index_end: seg.chunk_index_end,
                };
                fe.serialize(&mut buf).map_err(XetError::IoError)?;
            }
        }

        // ---- (c) xorb info section ----
        for xorb in &self.xorbs {
            let xh = XorbChunkSequenceHeader {
                xorb_hash: xorb.xorb_hash,
                xorb_flags: 0,
                num_entries: xorb.chunks.len() as u32,
                num_bytes_in_xorb: xorb.num_bytes_in_xorb,
                num_bytes_on_disk: xorb.num_bytes_on_disk,
            };
            xh.serialize(&mut buf).map_err(XetError::IoError)?;

            for chunk in &xorb.chunks {
                let xe = XorbChunkSequenceEntry {
                    chunk_hash: chunk.chunk_hash,
                    chunk_byte_range_start: chunk.chunk_byte_range_start,
                    unpacked_segment_bytes: chunk.unpacked_segment_bytes,
                    flags: 0,
                };
                xe.serialize(&mut buf).map_err(XetError::IoError)?;
            }
        }

        // ---- (d) optional chunk lookup section ----
        for entry in &self.chunk_lookup_entries {
            entry.serialize(&mut buf).map_err(XetError::IoError)?;
        }

        // ---- (e) footer ----
        //
        // The file_info parse loop in MDBShardFile::parse() uses file_lookup_offset
        // as the file-section boundary.  Setting it to xorb_info_offset tells the
        // parser where file data ends and xorb data begins.
        //
        // Similarly, the xorb_info parse loop uses xorb_lookup_offset (the start of
        // the next section) as its upper boundary, so the xorb parser stops before
        // reading past the xorb section.
        let total_stored_bytes_on_disk: u64 =
            self.xorbs.iter().map(|x| x.num_bytes_on_disk as u64).sum();
        let total_materialized_bytes: u64 = self
            .files
            .iter()
            .flat_map(|f| f.segments.iter())
            .map(|s| s.unpacked_segment_bytes as u64)
            .sum();
        let total_stored_bytes: u64 = self.xorbs.iter().map(|x| x.num_bytes_in_xorb as u64).sum();

        let footer = MDBShardFileFooter {
            version: 2,
            file_info_offset: if self.files.is_empty() {
                0
            } else {
                file_info_offset
            },
            xorb_info_offset: if self.xorbs.is_empty() {
                0
            } else {
                xorb_info_offset
            },
            file_lookup_offset: if self.files.is_empty() {
                0
            } else {
                xorb_info_offset
            },
            file_lookup_num_entry: self.files.len() as u64,
            xorb_lookup_offset: if self.xorbs.is_empty() {
                0
            } else if self.chunk_lookup_entries.is_empty() {
                footer_offset
            } else {
                chunk_lookup_offset
            },
            xorb_lookup_num_entry: self.xorbs.len() as u64,
            chunk_lookup_offset: if self.chunk_lookup_entries.is_empty() {
                0
            } else {
                chunk_lookup_offset
            },
            chunk_lookup_num_entry: self.chunk_lookup_entries.len() as u64,
            chunk_hash_hmac_key: [0u8; 32],
            shard_creation_timestamp: 0,
            shard_key_expiry: 0,
            stored_bytes_on_disk: total_stored_bytes_on_disk,
            materialized_bytes: total_materialized_bytes,
            stored_bytes: total_stored_bytes,
            footer_offset,
        };

        // Serialize footer at the end of buf.
        let footer_pos = buf.len() as u64;
        let mut footer_cursor = Cursor::new(&mut buf);
        footer_cursor.set_position(footer_pos);
        footer
            .serialize(&mut footer_cursor)
            .map_err(XetError::IoError)?;

        // M8 fix: Use proper error instead of debug_assert_eq! which is a no-op in release builds.
        // Shard binary size correctness is critical for data integrity — detect mismatches
        // in all build configurations, not just debug.
        if buf.len() != total_size {
            return Err(XetError::ParseError(format!(
                "shard binary size mismatch: wrote {} bytes, expected {}",
                buf.len(),
                total_size
            )));
        }

        // ---- round-trip verification ----
        let _parsed = MDBShardFile::parse(&buf).map_err(|e| {
            XetError::ParseError(format!(
                "ShardBuilder::build() produced invalid shard data ({} bytes): {}",
                buf.len(),
                e,
            ))
        })?;

        Ok(buf)
    }
}

impl Default for ShardBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::shard::MDBShardFile;

    /// Deterministic hash for testing — puts `val` in the first byte.
    fn test_hash(val: u8) -> MerkleHash {
        let mut bytes = [0u8; 32];
        bytes[0] = val;
        MerkleHash::from(bytes)
    }

    /// Build `n` chunk entries whose hashes start at `hash_start`.
    fn make_chunks(n: usize, hash_start: u8) -> Vec<XorbChunkBuildEntry> {
        (0..n)
            .map(|i| XorbChunkBuildEntry {
                chunk_hash: test_hash(hash_start.wrapping_add(i as u8)),
                chunk_byte_range_start: (i * 256) as u32,
                unpacked_segment_bytes: 256,
            })
            .collect()
    }

    fn make_hashes(n: usize, hash_start: u8) -> Vec<MerkleHash> {
        (0..n)
            .map(|i| test_hash(hash_start.wrapping_add(i as u8)))
            .collect()
    }

    // ---- test 1: single file, single xorb, single chunk ----

    #[test]
    fn test_shard_builder_single_file_single_xorb() {
        let mut builder = ShardBuilder::new();

        let xorb_hash = test_hash(1);
        let chunk_hash = test_hash(10);
        let raw_chunk_hash = test_hash(20);

        let xi = builder
            .add_xorb_with_raw_chunk_hashes(
                xorb_hash,
                256, // num_bytes_in_xorb
                128, // num_bytes_on_disk
                vec![XorbChunkBuildEntry {
                    chunk_hash,
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: 256,
                }],
                vec![raw_chunk_hash],
            )
            .unwrap();

        let file_hash = test_hash(100);
        builder.add_file(
            file_hash,
            vec![FileSegment {
                xorb_hash,
                xorb_index: xi,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: 256,
            }],
        );

        let data = builder.build().unwrap();

        // Expected size: header + (file hdr + 1 entry) + (xorb hdr + 1 chunk) + footer
        let expected_size = HEADER_SIZE
            + (FILE_HEADER_SIZE + FILE_ENTRY_SIZE)
            + (XORB_HEADER_SIZE + XORB_ENTRY_SIZE)
            + CHUNK_LOOKUP_ENTRY_SIZE
            + FOOTER_SIZE;
        assert_eq!(data.len(), expected_size);

        let parsed = MDBShardFile::parse(&data).unwrap();

        // Verify file entries
        assert_eq!(parsed.file_entries.len(), 1);
        assert_eq!(parsed.file_entries[0].file_hash, file_hash);
        assert_eq!(parsed.file_entries[0].num_entries, 1);
        assert_eq!(parsed.file_hashes.len(), 1);
        assert_eq!(parsed.file_hashes[0], file_hash);

        // Verify file data entries
        assert_eq!(parsed.file_data_entries.len(), 1);
        assert_eq!(parsed.file_data_entries[0].xorb_hash, xorb_hash);
        assert_eq!(parsed.file_data_entries[0].chunk_index_start, 0);
        assert_eq!(parsed.file_data_entries[0].chunk_index_end, 1);
        assert_eq!(parsed.file_data_entries[0].unpacked_segment_bytes, 256);

        // Verify xorb entries
        assert_eq!(parsed.xorb_entries.len(), 1);
        assert_eq!(parsed.xorb_entries[0].xorb_hash, xorb_hash);
        assert_eq!(parsed.xorb_entries[0].num_entries, 1);
        assert_eq!(parsed.xorb_entries[0].num_bytes_in_xorb, 256);
        assert_eq!(parsed.xorb_entries[0].num_bytes_on_disk, 128);

        // Verify xorb chunk entries
        assert_eq!(parsed.xorb_chunk_entries.len(), 1);
        assert_eq!(parsed.xorb_chunk_entries[0].chunk_hash, chunk_hash);
        assert_eq!(parsed.xorb_chunk_entries[0].chunk_byte_range_start, 0);
        assert_eq!(parsed.xorb_chunk_entries[0].unpacked_segment_bytes, 256);

        // Verify chunk mappings
        assert_eq!(parsed.chunk_mappings.len(), 1);
        assert_eq!(parsed.chunk_mappings[0], (raw_chunk_hash, xorb_hash, 0));
        assert_eq!(parsed.chunk_lookup_entries.len(), 1);
        assert_eq!(parsed.chunk_lookup_entries[0].chunk_hash, raw_chunk_hash);

        // Verify footer
        assert_eq!(parsed.footer.file_info_offset, HEADER_SIZE as u64);
        assert_eq!(
            parsed.footer.xorb_info_offset,
            (HEADER_SIZE + FILE_HEADER_SIZE + FILE_ENTRY_SIZE) as u64
        );
        assert_eq!(
            parsed.footer.chunk_lookup_offset,
            (HEADER_SIZE + FILE_HEADER_SIZE + FILE_ENTRY_SIZE + XORB_HEADER_SIZE + XORB_ENTRY_SIZE)
                as u64
        );
        assert_eq!(parsed.footer.chunk_lookup_num_entry, 1);
        assert_eq!(parsed.footer.stored_bytes_on_disk, 128);
        assert_eq!(parsed.footer.materialized_bytes, 256);
        assert_eq!(parsed.footer.stored_bytes, 256);
    }

    // ---- test 2: multiple files sharing one xorb ----

    #[test]
    fn test_shard_builder_multiple_files() {
        let mut builder = ShardBuilder::new();

        let xorb_hash = test_hash(1);
        let chunks = make_chunks(3, 10);
        let raw_hashes = make_hashes(3, 110);

        let xi = builder
            .add_xorb_with_raw_chunk_hashes(xorb_hash, 768, 384, chunks.clone(), raw_hashes.clone())
            .unwrap();

        let file_hash_a = test_hash(100);
        let file_hash_b = test_hash(101);

        // File A: uses chunk 0..1
        builder.add_file(
            file_hash_a,
            vec![FileSegment {
                xorb_hash,
                xorb_index: xi,
                chunk_index_start: 0,
                chunk_index_end: 1,
                unpacked_segment_bytes: 256,
            }],
        );

        // File B: uses chunks 1..3
        builder.add_file(
            file_hash_b,
            vec![FileSegment {
                xorb_hash,
                xorb_index: xi,
                chunk_index_start: 1,
                chunk_index_end: 3,
                unpacked_segment_bytes: 512,
            }],
        );

        let data = builder.build().unwrap();
        let parsed = MDBShardFile::parse(&data).unwrap();

        // 2 files
        assert_eq!(parsed.file_entries.len(), 2);
        assert_eq!(parsed.file_hashes, vec![file_hash_a, file_hash_b]);
        assert_eq!(parsed.file_entries[0].num_entries, 1);
        assert_eq!(parsed.file_entries[1].num_entries, 1);

        assert_eq!(parsed.file_data_entries.len(), 2);
        assert_eq!(parsed.file_data_entries[0].chunk_index_start, 0);
        assert_eq!(parsed.file_data_entries[0].chunk_index_end, 1);
        assert_eq!(parsed.file_data_entries[0].unpacked_segment_bytes, 256);
        assert_eq!(parsed.file_data_entries[1].chunk_index_start, 1);
        assert_eq!(parsed.file_data_entries[1].chunk_index_end, 3);
        assert_eq!(parsed.file_data_entries[1].unpacked_segment_bytes, 512);

        // 1 xorb with 3 chunks
        assert_eq!(parsed.xorb_entries.len(), 1);
        assert_eq!(parsed.xorb_entries[0].xorb_hash, xorb_hash);
        assert_eq!(parsed.xorb_entries[0].num_entries, 3);
        assert_eq!(parsed.xorb_entries[0].num_bytes_in_xorb, 768);
        assert_eq!(parsed.xorb_entries[0].num_bytes_on_disk, 384);

        assert_eq!(parsed.xorb_chunk_entries.len(), 3);
        for (i, entry) in parsed.xorb_chunk_entries.iter().enumerate() {
            assert_eq!(entry.chunk_hash, chunks[i].chunk_hash);
            assert_eq!(
                entry.chunk_byte_range_start,
                chunks[i].chunk_byte_range_start
            );
            assert_eq!(entry.unpacked_segment_bytes, 256);
        }

        // 3 chunk mappings
        assert_eq!(parsed.chunk_mappings.len(), 3);
        for (i, &(ch, xh, idx)) in parsed.chunk_mappings.iter().enumerate() {
            assert_eq!(ch, raw_hashes[i]);
            assert_eq!(xh, xorb_hash);
            assert_eq!(idx, i as u32);
        }
        assert_eq!(parsed.chunk_lookup_entries.len(), 3);

        // Footer totals
        assert_eq!(parsed.footer.stored_bytes_on_disk, 384);
        assert_eq!(parsed.footer.materialized_bytes, 256 + 512);
        assert_eq!(parsed.footer.stored_bytes, 768);
    }

    // ---- test 3: roundtrip verification ----

    #[test]
    fn test_shard_builder_roundtrip() {
        let mut builder = ShardBuilder::new();

        let xorb_hash_1 = test_hash(1);
        let xorb_hash_2 = test_hash(2);

        let chunks_1 = make_chunks(2, 10);
        let chunks_2 = make_chunks(3, 20);
        let raw_hashes_1 = make_hashes(2, 110);
        let raw_hashes_2 = make_hashes(3, 120);

        let xi1 = builder
            .add_xorb_with_raw_chunk_hashes(
                xorb_hash_1,
                512,
                256,
                chunks_1.clone(),
                raw_hashes_1.clone(),
            )
            .unwrap();
        let xi2 = builder
            .add_xorb_with_raw_chunk_hashes(
                xorb_hash_2,
                768,
                400,
                chunks_2.clone(),
                raw_hashes_2.clone(),
            )
            .unwrap();

        let file_hash = test_hash(50);
        builder.add_file(
            file_hash,
            vec![
                FileSegment {
                    xorb_hash: xorb_hash_1,
                    xorb_index: xi1,
                    chunk_index_start: 0,
                    chunk_index_end: 2,
                    unpacked_segment_bytes: 512,
                },
                FileSegment {
                    xorb_hash: xorb_hash_2,
                    xorb_index: xi2,
                    chunk_index_start: 0,
                    chunk_index_end: 3,
                    unpacked_segment_bytes: 768,
                },
            ],
        );

        let data = builder.build().unwrap();
        let parsed = MDBShardFile::parse(&data).unwrap();

        // -- Header --
        assert_eq!(parsed.header.version, 2);
        assert_eq!(parsed.header.footer_size, 208);
        assert_eq!(parsed.header.tag, MDBShardFileHeader::default().tag);

        // -- Footer offsets --
        assert_eq!(parsed.footer.version, 2);
        assert_eq!(parsed.footer.file_info_offset, HEADER_SIZE as u64);

        let expected_xorb_offset = (HEADER_SIZE + FILE_HEADER_SIZE + 2 * FILE_ENTRY_SIZE) as u64;
        assert_eq!(parsed.footer.xorb_info_offset, expected_xorb_offset);

        let expected_footer_offset = (HEADER_SIZE
            + FILE_HEADER_SIZE
            + 2 * FILE_ENTRY_SIZE
            + XORB_HEADER_SIZE
            + 2 * XORB_ENTRY_SIZE
            + XORB_HEADER_SIZE
            + 3 * XORB_ENTRY_SIZE
            + 5 * CHUNK_LOOKUP_ENTRY_SIZE) as u64;
        assert_eq!(parsed.footer.footer_offset, expected_footer_offset);
        assert_eq!(data.len(), expected_footer_offset as usize + FOOTER_SIZE);

        // -- Footer lookup fields --
        assert_eq!(parsed.footer.file_lookup_num_entry, 1);
        assert_eq!(parsed.footer.xorb_lookup_num_entry, 2);
        assert_eq!(
            parsed.footer.chunk_lookup_offset,
            expected_footer_offset - 5 * CHUNK_LOOKUP_ENTRY_SIZE as u64
        );
        assert_eq!(parsed.footer.chunk_lookup_num_entry, 5);

        // -- Footer byte totals --
        assert_eq!(parsed.footer.stored_bytes_on_disk, 256 + 400);
        assert_eq!(parsed.footer.materialized_bytes, 512 + 768);
        assert_eq!(parsed.footer.stored_bytes, 512 + 768);

        // -- File info --
        assert_eq!(parsed.file_entries.len(), 1);
        assert_eq!(parsed.file_entries[0].file_hash, file_hash);
        assert_eq!(parsed.file_entries[0].num_entries, 2);
        assert_eq!(parsed.file_hashes, vec![file_hash]);

        assert_eq!(parsed.file_data_entries.len(), 2);

        let fe0 = &parsed.file_data_entries[0];
        assert_eq!(fe0.xorb_hash, xorb_hash_1);
        assert_eq!(fe0.chunk_index_start, 0);
        assert_eq!(fe0.chunk_index_end, 2);
        assert_eq!(fe0.unpacked_segment_bytes, 512);

        let fe1 = &parsed.file_data_entries[1];
        assert_eq!(fe1.xorb_hash, xorb_hash_2);
        assert_eq!(fe1.chunk_index_start, 0);
        assert_eq!(fe1.chunk_index_end, 3);
        assert_eq!(fe1.unpacked_segment_bytes, 768);

        // -- Xorb info --
        assert_eq!(parsed.xorb_entries.len(), 2);
        assert_eq!(parsed.xorb_entries[0].xorb_hash, xorb_hash_1);
        assert_eq!(parsed.xorb_entries[0].num_entries, 2);
        assert_eq!(parsed.xorb_entries[0].num_bytes_in_xorb, 512);
        assert_eq!(parsed.xorb_entries[0].num_bytes_on_disk, 256);

        assert_eq!(parsed.xorb_entries[1].xorb_hash, xorb_hash_2);
        assert_eq!(parsed.xorb_entries[1].num_entries, 3);
        assert_eq!(parsed.xorb_entries[1].num_bytes_in_xorb, 768);
        assert_eq!(parsed.xorb_entries[1].num_bytes_on_disk, 400);

        // -- Xorb chunk entries --
        assert_eq!(parsed.xorb_chunk_entries.len(), 5); // 2 + 3

        for (i, entry) in parsed.xorb_chunk_entries[..2].iter().enumerate() {
            assert_eq!(entry.chunk_hash, chunks_1[i].chunk_hash);
            assert_eq!(
                entry.chunk_byte_range_start,
                chunks_1[i].chunk_byte_range_start
            );
        }
        for (i, entry) in parsed.xorb_chunk_entries[2..].iter().enumerate() {
            assert_eq!(entry.chunk_hash, chunks_2[i].chunk_hash);
            assert_eq!(
                entry.chunk_byte_range_start,
                chunks_2[i].chunk_byte_range_start
            );
        }

        // -- Chunk mappings --
        assert_eq!(parsed.chunk_mappings.len(), 5);
        assert_eq!(parsed.chunk_lookup_entries.len(), 5);
        for (i, (ch, xh, idx)) in parsed.chunk_mappings[..2].iter().enumerate() {
            assert_eq!(*ch, raw_hashes_1[i]);
            assert_eq!(*xh, xorb_hash_1);
            assert_eq!(*idx, i as u32);
        }
        for (i, (ch, xh, idx)) in parsed.chunk_mappings[2..].iter().enumerate() {
            assert_eq!(*ch, raw_hashes_2[i]);
            assert_eq!(*xh, xorb_hash_2);
            assert_eq!(*idx, i as u32);
        }
    }

    // ---- test 4: one file spanning multiple xorbs ----

    #[test]
    fn test_shard_builder_multiple_xorbs() {
        let mut builder = ShardBuilder::new();

        let xorb_hash_a = test_hash(1);
        let xorb_hash_b = test_hash(2);
        let xorb_hash_c = test_hash(3);

        let chunks_a = make_chunks(2, 10);
        let chunks_b = make_chunks(1, 20);
        let chunks_c = make_chunks(4, 30);
        let raw_hashes_a = make_hashes(2, 110);
        let raw_hashes_b = make_hashes(1, 120);
        let raw_hashes_c = make_hashes(4, 130);

        let xa = builder
            .add_xorb_with_raw_chunk_hashes(xorb_hash_a, 512, 200, chunks_a.clone(), raw_hashes_a)
            .unwrap();
        let xb = builder
            .add_xorb_with_raw_chunk_hashes(xorb_hash_b, 256, 100, chunks_b.clone(), raw_hashes_b)
            .unwrap();
        let xc = builder
            .add_xorb_with_raw_chunk_hashes(xorb_hash_c, 1024, 500, chunks_c.clone(), raw_hashes_c)
            .unwrap();

        let file_hash = test_hash(99);
        builder.add_file(
            file_hash,
            vec![
                FileSegment {
                    xorb_hash: xorb_hash_a,
                    xorb_index: xa,
                    chunk_index_start: 0,
                    chunk_index_end: 2,
                    unpacked_segment_bytes: 512,
                },
                FileSegment {
                    xorb_hash: xorb_hash_b,
                    xorb_index: xb,
                    chunk_index_start: 0,
                    chunk_index_end: 1,
                    unpacked_segment_bytes: 256,
                },
                FileSegment {
                    xorb_hash: xorb_hash_c,
                    xorb_index: xc,
                    chunk_index_start: 0,
                    chunk_index_end: 4,
                    unpacked_segment_bytes: 1024,
                },
            ],
        );

        let data = builder.build().unwrap();
        let parsed = MDBShardFile::parse(&data).unwrap();

        // 1 file with 3 segments
        assert_eq!(parsed.file_entries.len(), 1);
        assert_eq!(parsed.file_entries[0].num_entries, 3);
        assert_eq!(parsed.file_data_entries.len(), 3);

        assert_eq!(parsed.file_data_entries[0].xorb_hash, xorb_hash_a);
        assert_eq!(parsed.file_data_entries[0].chunk_index_end, 2);
        assert_eq!(parsed.file_data_entries[0].unpacked_segment_bytes, 512);

        assert_eq!(parsed.file_data_entries[1].xorb_hash, xorb_hash_b);
        assert_eq!(parsed.file_data_entries[1].chunk_index_end, 1);
        assert_eq!(parsed.file_data_entries[1].unpacked_segment_bytes, 256);

        assert_eq!(parsed.file_data_entries[2].xorb_hash, xorb_hash_c);
        assert_eq!(parsed.file_data_entries[2].chunk_index_end, 4);
        assert_eq!(parsed.file_data_entries[2].unpacked_segment_bytes, 1024);

        // 3 xorbs
        assert_eq!(parsed.xorb_entries.len(), 3);
        assert_eq!(parsed.xorb_entries[0].num_entries, 2);
        assert_eq!(parsed.xorb_entries[1].num_entries, 1);
        assert_eq!(parsed.xorb_entries[2].num_entries, 4);

        // 7 total chunk entries (2 + 1 + 4)
        assert_eq!(parsed.xorb_chunk_entries.len(), 7);
        assert_eq!(parsed.chunk_mappings.len(), 7);
        assert_eq!(parsed.chunk_lookup_entries.len(), 7);

        // Verify all chunks belong to the correct xorbs
        let expected_xorbs: Vec<MerkleHash> = vec![
            xorb_hash_a,
            xorb_hash_a,
            xorb_hash_b,
            xorb_hash_c,
            xorb_hash_c,
            xorb_hash_c,
            xorb_hash_c,
        ];
        let actual_xorbs: Vec<MerkleHash> =
            parsed.chunk_mappings.iter().map(|&(_, xh, _)| xh).collect();
        assert_eq!(actual_xorbs, expected_xorbs);

        // Footer totals
        assert_eq!(parsed.footer.stored_bytes_on_disk, 200 + 100 + 500);
        assert_eq!(parsed.footer.materialized_bytes, 512 + 256 + 1024);
        assert_eq!(parsed.footer.stored_bytes, 512 + 256 + 1024);
    }

    // ---- bonus: empty builder produces valid (but empty) shard ----

    #[test]
    fn test_shard_builder_empty() {
        let builder = ShardBuilder::new();
        let data = builder.build().unwrap();

        // Header (48) + Footer (208) only
        assert_eq!(data.len(), HEADER_SIZE + FOOTER_SIZE);

        let parsed = MDBShardFile::parse(&data).unwrap();
        assert_eq!(parsed.file_entries.len(), 0);
        assert_eq!(parsed.xorb_entries.len(), 0);
        assert_eq!(parsed.file_hashes.len(), 0);
        assert_eq!(parsed.chunk_mappings.len(), 0);
        assert_eq!(parsed.footer.stored_bytes_on_disk, 0);
        assert_eq!(parsed.footer.materialized_bytes, 0);
        assert_eq!(parsed.footer.stored_bytes, 0);
    }
}
