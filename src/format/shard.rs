use crate::error::Result as XetResult;
use std::fs::File;
use std::io::{Cursor, Read, Result, Seek, SeekFrom, Write};
use std::path::Path;

const SHARD_HEADER_SIZE: u64 = 48;
const SHARD_FOOTER_SIZE: u64 = 208;
const SEQUENCE_HEADER_SIZE: u64 = 48;
const SEQUENCE_ENTRY_SIZE: u64 = 48;
const CHUNK_LOOKUP_ENTRY_SIZE: u64 = 68;

/// Shard file header (48 bytes)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MDBShardFileHeader {
    pub tag: [u8; 32],
    pub version: u64,
    pub footer_size: u64,
}

impl Default for MDBShardFileHeader {
    fn default() -> Self {
        Self {
            tag: [
                72, 70, 82, 101, 112, 111, 77, 101, 116, 97, 68, 97, 116, 97, 0, 85, 105, 103, 69,
                106, 123, 129, 87, 131, 165, 189, 217, 92, 205, 209, 74, 169,
            ],
            version: 2,
            footer_size: 208,
        }
    }
}

impl MDBShardFileHeader {
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.tag)?;
        writer.write_all(&self.version.to_le_bytes())?;
        writer.write_all(&self.footer_size.to_le_bytes())?;
        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let mut tag = [0u8; 32];
        reader.read_exact(&mut tag)?;

        let mut version_buf = [0u8; 8];
        reader.read_exact(&mut version_buf)?;
        let version = u64::from_le_bytes(version_buf);

        let mut footer_size_buf = [0u8; 8];
        reader.read_exact(&mut footer_size_buf)?;
        let footer_size = u64::from_le_bytes(footer_size_buf);

        Ok(Self {
            tag,
            version,
            footer_size,
        })
    }
}

/// Shard file footer (208 bytes)
#[derive(Debug, Clone, PartialEq)]
pub struct MDBShardFileFooter {
    pub version: u64,
    pub file_info_offset: u64,
    pub xorb_info_offset: u64,
    pub file_lookup_offset: u64,
    pub file_lookup_num_entry: u64,
    pub xorb_lookup_offset: u64,
    pub xorb_lookup_num_entry: u64,
    pub chunk_lookup_offset: u64,
    pub chunk_lookup_num_entry: u64,
    pub chunk_hash_hmac_key: [u8; 32],
    pub shard_creation_timestamp: u64,
    pub shard_key_expiry: u64,
    pub stored_bytes_on_disk: u64,
    pub materialized_bytes: u64,
    pub stored_bytes: u64,
    pub footer_offset: u64,
}

impl MDBShardFileFooter {
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.version.to_le_bytes())?;
        writer.write_all(&self.file_info_offset.to_le_bytes())?;
        writer.write_all(&self.xorb_info_offset.to_le_bytes())?;
        writer.write_all(&self.file_lookup_offset.to_le_bytes())?;
        writer.write_all(&self.file_lookup_num_entry.to_le_bytes())?;
        writer.write_all(&self.xorb_lookup_offset.to_le_bytes())?;
        writer.write_all(&self.xorb_lookup_num_entry.to_le_bytes())?;
        writer.write_all(&self.chunk_lookup_offset.to_le_bytes())?;
        writer.write_all(&self.chunk_lookup_num_entry.to_le_bytes())?;
        writer.write_all(&self.chunk_hash_hmac_key)?;
        writer.write_all(&self.shard_creation_timestamp.to_le_bytes())?;
        writer.write_all(&self.shard_key_expiry.to_le_bytes())?;
        writer.write_all(&[0u8; 56])?; // _buffer (7 * u64)
        writer.write_all(&self.stored_bytes_on_disk.to_le_bytes())?;
        writer.write_all(&self.materialized_bytes.to_le_bytes())?;
        writer.write_all(&self.stored_bytes.to_le_bytes())?;
        writer.write_all(&self.footer_offset.to_le_bytes())?;
        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let version = read_u64(reader)?;
        let file_info_offset = read_u64(reader)?;
        let xorb_info_offset = read_u64(reader)?;
        let file_lookup_offset = read_u64(reader)?;
        let file_lookup_num_entry = read_u64(reader)?;
        let xorb_lookup_offset = read_u64(reader)?;
        let xorb_lookup_num_entry = read_u64(reader)?;
        let chunk_lookup_offset = read_u64(reader)?;
        let chunk_lookup_num_entry = read_u64(reader)?;

        let mut chunk_hash_hmac_key = [0u8; 32];
        reader.read_exact(&mut chunk_hash_hmac_key)?;

        let shard_creation_timestamp = read_u64(reader)?;
        let shard_key_expiry = read_u64(reader)?;

        let mut buffer = [0u8; 56];
        reader.read_exact(&mut buffer)?;

        let stored_bytes_on_disk = read_u64(reader)?;
        let materialized_bytes = read_u64(reader)?;
        let stored_bytes = read_u64(reader)?;
        let footer_offset = read_u64(reader)?;

        Ok(Self {
            version,
            file_info_offset,
            xorb_info_offset,
            file_lookup_offset,
            file_lookup_num_entry,
            xorb_lookup_offset,
            xorb_lookup_num_entry,
            chunk_lookup_offset,
            chunk_lookup_num_entry,
            chunk_hash_hmac_key,
            shard_creation_timestamp,
            shard_key_expiry,
            stored_bytes_on_disk,
            materialized_bytes,
            stored_bytes,
            footer_offset,
        })
    }
}

fn read_u64<R: Read>(reader: &mut R) -> XetResult<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_u32<R: Read>(reader: &mut R) -> XetResult<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

use crate::types::MerkleHash;

/// File data sequence header (48 bytes)
///
/// Introduces a file's reconstruction info, followed by num_entries FileDataSequenceEntry structs.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDataSequenceHeader {
    pub file_hash: MerkleHash,
    pub file_flags: u32,
    pub num_entries: u32,
}

impl FileDataSequenceHeader {
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.file_hash.as_bytes())?; // 32 bytes
        writer.write_all(&self.file_flags.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.num_entries.to_le_bytes())?; // 4 bytes
        writer.write_all(&[0u8; 8])?; // _unused: 8 bytes
        // Total: 48 bytes
        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let mut hash_bytes = [0u8; 32];
        reader.read_exact(&mut hash_bytes)?;
        let file_hash = MerkleHash::from(hash_bytes);

        let file_flags = read_u32(reader)?;
        let num_entries = read_u32(reader)?;

        let mut unused = [0u8; 8];
        reader.read_exact(&mut unused)?;

        Ok(Self {
            file_hash,
            file_flags,
            num_entries,
        })
    }
}

/// File data sequence entry (48 bytes)
///
/// Maps a range of chunks in a xorb to a portion of a file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDataSequenceEntry {
    pub xorb_hash: MerkleHash,
    pub xorb_flags: u32,
    pub unpacked_segment_bytes: u32,
    pub chunk_index_start: u32,
    pub chunk_index_end: u32,
}

impl FileDataSequenceEntry {
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.xorb_hash.as_bytes())?; // 32 bytes
        writer.write_all(&self.xorb_flags.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.unpacked_segment_bytes.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.chunk_index_start.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.chunk_index_end.to_le_bytes())?; // 4 bytes
        // Total: 48 bytes
        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let mut hash_bytes = [0u8; 32];
        reader.read_exact(&mut hash_bytes)?;
        let xorb_hash = MerkleHash::from(hash_bytes);

        let xorb_flags = read_u32(reader)?;
        let unpacked_segment_bytes = read_u32(reader)?;
        let chunk_index_start = read_u32(reader)?;
        let chunk_index_end = read_u32(reader)?;

        Ok(Self {
            xorb_hash,
            xorb_flags,
            unpacked_segment_bytes,
            chunk_index_start,
            chunk_index_end,
        })
    }
}

/// Xorb chunk sequence header (48 bytes)
///
/// Introduces a xorb's chunk info, followed by num_entries XorbChunkSequenceEntry structs.
#[derive(Debug, Clone, PartialEq)]
pub struct XorbChunkSequenceHeader {
    pub xorb_hash: MerkleHash,
    pub xorb_flags: u32,
    pub num_entries: u32,
    pub num_bytes_in_xorb: u32,
    pub num_bytes_on_disk: u32,
}

impl XorbChunkSequenceHeader {
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.xorb_hash.as_bytes())?; // 32 bytes
        writer.write_all(&self.xorb_flags.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.num_entries.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.num_bytes_in_xorb.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.num_bytes_on_disk.to_le_bytes())?; // 4 bytes
        // Total: 48 bytes
        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let mut hash_bytes = [0u8; 32];
        reader.read_exact(&mut hash_bytes)?;
        let xorb_hash = MerkleHash::from(hash_bytes);

        let xorb_flags = read_u32(reader)?;
        let num_entries = read_u32(reader)?;
        let num_bytes_in_xorb = read_u32(reader)?;
        let num_bytes_on_disk = read_u32(reader)?;

        Ok(Self {
            xorb_hash,
            xorb_flags,
            num_entries,
            num_bytes_in_xorb,
            num_bytes_on_disk,
        })
    }
}

/// Xorb chunk sequence entry (48 bytes)
///
/// Describes a single chunk within a xorb.
#[derive(Debug, Clone, PartialEq)]
pub struct XorbChunkSequenceEntry {
    pub chunk_hash: MerkleHash,
    pub chunk_byte_range_start: u32,
    pub unpacked_segment_bytes: u32,
    pub flags: u32,
}

impl XorbChunkSequenceEntry {
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.chunk_hash.as_bytes())?; // 32 bytes
        writer.write_all(&self.chunk_byte_range_start.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.unpacked_segment_bytes.to_le_bytes())?; // 4 bytes
        writer.write_all(&self.flags.to_le_bytes())?; // 4 bytes
        writer.write_all(&[0u8; 4])?; // _unused: 4 bytes
        // Total: 48 bytes
        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let mut hash_bytes = [0u8; 32];
        reader.read_exact(&mut hash_bytes)?;
        let chunk_hash = MerkleHash::from(hash_bytes);

        let chunk_byte_range_start = read_u32(reader)?;
        let unpacked_segment_bytes = read_u32(reader)?;
        let flags = read_u32(reader)?;

        let mut unused = [0u8; 4];
        reader.read_exact(&mut unused)?;

        Ok(Self {
            chunk_hash,
            chunk_byte_range_start,
            unpacked_segment_bytes,
            flags,
        })
    }
}

/// Chunk lookup entry (68 bytes)
///
/// Persists the raw content chunk hash used by global deduplication. The
/// xorb chunk sequence continues to store the serialized chunk hash used for
/// xorb integrity verification.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkLookupEntry {
    pub chunk_hash: MerkleHash,
    pub xorb_hash: MerkleHash,
    pub chunk_index: u32,
}

impl ChunkLookupEntry {
    pub const SIZE: usize = 68;

    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.chunk_hash.as_bytes())?;
        writer.write_all(&self.xorb_hash.as_bytes())?;
        writer.write_all(&self.chunk_index.to_le_bytes())?;
        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let mut chunk_hash_bytes = [0u8; 32];
        reader.read_exact(&mut chunk_hash_bytes)?;
        let chunk_hash = MerkleHash::from(chunk_hash_bytes);

        let mut xorb_hash_bytes = [0u8; 32];
        reader.read_exact(&mut xorb_hash_bytes)?;
        let xorb_hash = MerkleHash::from(xorb_hash_bytes);

        let chunk_index = read_u32(reader)?;

        Ok(Self {
            chunk_hash,
            xorb_hash,
            chunk_index,
        })
    }
}

/// High-level shard file representation
///
/// Contains parsed metadata from a shard file for indexing and querying.
#[derive(Debug, Clone, PartialEq)]
pub struct MDBShardFile {
    pub header: MDBShardFileHeader,
    pub footer: MDBShardFileFooter,
    pub file_entries: Vec<FileDataSequenceHeader>,
    pub file_data_entries: Vec<FileDataSequenceEntry>,
    pub xorb_entries: Vec<XorbChunkSequenceHeader>,
    pub xorb_chunk_entries: Vec<XorbChunkSequenceEntry>,
    pub chunk_lookup_entries: Vec<ChunkLookupEntry>,
    pub file_hashes: Vec<MerkleHash>,
    pub chunk_mappings: Vec<(MerkleHash, MerkleHash, u32)>,
    hash: String,
}

impl MDBShardFile {
    /// Parse a shard file from binary data without retaining a copy of the input.
    pub fn parse(data: &[u8]) -> XetResult<Self> {
        let file_len = u64::try_from(data.len()).map_err(|_| {
            crate::error::XetError::ParseError("Shard length does not fit in u64".to_string())
        })?;
        let hash = crate::hash::compute_data_hash(data).to_hex();
        Self::parse_reader(&mut Cursor::new(data), file_len, hash)
    }

    /// Parse a complete shard from disk while keeping memory bounded to parsed
    /// metadata plus a 64 KiB hashing buffer.
    pub fn parse_from_file(path: &Path) -> XetResult<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let hash = Self::hash_reader(&mut file, file_len)?;
        Self::parse_reader(&mut file, file_len, hash)
    }

    /// Compute the BLAKE3 hash of a shard file incrementally.
    pub fn compute_hash_from_file(path: &Path) -> XetResult<String> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        Self::hash_reader(&mut file, file_len)
    }

    /// Return the hash computed while parsing the shard.
    pub fn compute_hash(&self) -> String {
        self.hash.clone()
    }

    /// Get file hashes contained in this shard.
    pub fn file_hashes(&self) -> &[MerkleHash] {
        &self.file_hashes
    }

    /// Get chunk-to-xorb mappings.
    pub fn chunk_mappings(&self) -> &[(MerkleHash, MerkleHash, u32)] {
        &self.chunk_mappings
    }

    fn parse_reader<R: Read + Seek>(
        reader: &mut R,
        file_len: u64,
        hash: String,
    ) -> XetResult<Self> {
        let minimum_size = SHARD_HEADER_SIZE + SHARD_FOOTER_SIZE;
        if file_len < minimum_size {
            return Err(crate::error::XetError::ParseError(format!(
                "Shard is too small: {} bytes, minimum is {}",
                file_len, minimum_size
            )));
        }
        let footer_start = file_len.checked_sub(SHARD_FOOTER_SIZE).ok_or_else(|| {
            crate::error::XetError::ParseError("Shard footer offset underflow".to_string())
        })?;

        reader.seek(SeekFrom::Start(0))?;
        let header = MDBShardFileHeader::deserialize(reader)?;
        let expected_header = MDBShardFileHeader::default();
        if header.tag != expected_header.tag {
            return Err(crate::error::XetError::ParseError(
                "Invalid shard magic tag".to_string(),
            ));
        }
        if header.version != expected_header.version {
            return Err(crate::error::XetError::ParseError(format!(
                "Unsupported shard version: {}",
                header.version
            )));
        }
        if header.footer_size != SHARD_FOOTER_SIZE {
            return Err(crate::error::XetError::ParseError(format!(
                "Invalid footer size: expected {}, got {}",
                SHARD_FOOTER_SIZE, header.footer_size
            )));
        }

        reader.seek(SeekFrom::Start(footer_start))?;
        let footer = MDBShardFileFooter::deserialize(reader)?;
        if footer.version != header.version {
            return Err(crate::error::XetError::ParseError(format!(
                "Shard version mismatch: header={}, footer={}",
                header.version, footer.version
            )));
        }
        if footer.footer_offset != footer_start {
            return Err(crate::error::XetError::ParseError(format!(
                "Invalid footer offset: declared {}, physical {}",
                footer.footer_offset, footer_start
            )));
        }

        let mut next_offset = SHARD_HEADER_SIZE;
        let (file_entries, file_data_entries, file_hashes) = if footer.file_info_offset == 0 {
            if footer.file_lookup_offset != 0 || footer.file_lookup_num_entry != 0 {
                return Err(crate::error::XetError::ParseError(
                    "File section metadata is inconsistent".to_string(),
                ));
            }
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            if footer.file_info_offset != next_offset {
                return Err(crate::error::XetError::ParseError(format!(
                    "File section must start at {}, got {}",
                    next_offset, footer.file_info_offset
                )));
            }
            Self::validate_section_bounds(
                "file",
                footer.file_info_offset,
                footer.file_lookup_offset,
                footer_start,
            )?;
            let parsed = Self::parse_file_section(
                reader,
                footer.file_info_offset,
                footer.file_lookup_offset,
            )?;
            if u64::try_from(parsed.0.len()).ok() != Some(footer.file_lookup_num_entry) {
                return Err(crate::error::XetError::ParseError(format!(
                    "File sequence count mismatch: declared {}, parsed {}",
                    footer.file_lookup_num_entry,
                    parsed.0.len()
                )));
            }
            next_offset = footer.file_lookup_offset;
            parsed
        };

        let (xorb_entries, xorb_chunk_entries) = if footer.xorb_info_offset == 0 {
            if footer.xorb_lookup_offset != 0 || footer.xorb_lookup_num_entry != 0 {
                return Err(crate::error::XetError::ParseError(
                    "Xorb section metadata is inconsistent".to_string(),
                ));
            }
            (Vec::new(), Vec::new())
        } else {
            if footer.xorb_info_offset != next_offset {
                return Err(crate::error::XetError::ParseError(format!(
                    "Xorb section must start at {}, got {}",
                    next_offset, footer.xorb_info_offset
                )));
            }
            Self::validate_section_bounds(
                "xorb",
                footer.xorb_info_offset,
                footer.xorb_lookup_offset,
                footer_start,
            )?;
            let parsed = Self::parse_xorb_section(
                reader,
                footer.xorb_info_offset,
                footer.xorb_lookup_offset,
            )?;
            if u64::try_from(parsed.0.len()).ok() != Some(footer.xorb_lookup_num_entry) {
                return Err(crate::error::XetError::ParseError(format!(
                    "Xorb sequence count mismatch: declared {}, parsed {}",
                    footer.xorb_lookup_num_entry,
                    parsed.0.len()
                )));
            }
            next_offset = footer.xorb_lookup_offset;
            parsed
        };

        let chunk_lookup_entries = if footer.chunk_lookup_offset == 0 {
            if footer.chunk_lookup_num_entry != 0 {
                return Err(crate::error::XetError::ParseError(
                    "Chunk lookup count is non-zero but its offset is zero".to_string(),
                ));
            }
            if !xorb_chunk_entries.is_empty() {
                return Err(crate::error::XetError::ParseError(
                    "Shard missing required raw chunk lookup section".to_string(),
                ));
            }
            Vec::new()
        } else {
            if footer.chunk_lookup_num_entry == 0 {
                return Err(crate::error::XetError::ParseError(
                    "Chunk lookup offset is non-zero but its count is zero".to_string(),
                ));
            }
            if footer.chunk_lookup_offset != next_offset {
                return Err(crate::error::XetError::ParseError(format!(
                    "Chunk lookup section must start at {}, got {}",
                    next_offset, footer.chunk_lookup_offset
                )));
            }
            let byte_len = footer
                .chunk_lookup_num_entry
                .checked_mul(CHUNK_LOOKUP_ENTRY_SIZE)
                .ok_or_else(|| {
                    crate::error::XetError::ParseError(
                        "Chunk lookup byte length overflow".to_string(),
                    )
                })?;
            let end = footer
                .chunk_lookup_offset
                .checked_add(byte_len)
                .ok_or_else(|| {
                    crate::error::XetError::ParseError(
                        "Chunk lookup end offset overflow".to_string(),
                    )
                })?;
            if end != footer_start {
                return Err(crate::error::XetError::ParseError(format!(
                    "Chunk lookup section ends at {}, expected footer at {}",
                    end, footer_start
                )));
            }

            let count = usize::try_from(footer.chunk_lookup_num_entry).map_err(|_| {
                crate::error::XetError::ParseError(
                    "Chunk lookup count does not fit in usize".to_string(),
                )
            })?;
            let mut entries = Vec::new();
            entries.try_reserve_exact(count).map_err(|e| {
                crate::error::XetError::ParseError(format!(
                    "Unable to allocate chunk lookup entries: {}",
                    e
                ))
            })?;
            reader.seek(SeekFrom::Start(footer.chunk_lookup_offset))?;
            for _ in 0..count {
                entries.push(ChunkLookupEntry::deserialize(reader)?);
            }
            next_offset = end;
            entries
        };

        if next_offset != footer_start {
            return Err(crate::error::XetError::ParseError(format!(
                "Unaccounted shard bytes between offset {} and footer {}",
                next_offset, footer_start
            )));
        }
        if chunk_lookup_entries.len() != xorb_chunk_entries.len() {
            return Err(crate::error::XetError::ParseError(format!(
                "Shard chunk lookup count mismatch: got {}, expected {}",
                chunk_lookup_entries.len(),
                xorb_chunk_entries.len()
            )));
        }

        let chunk_mappings = chunk_lookup_entries
            .iter()
            .map(|entry| (entry.chunk_hash, entry.xorb_hash, entry.chunk_index))
            .collect();

        Ok(Self {
            header,
            footer,
            file_entries,
            file_data_entries,
            xorb_entries,
            xorb_chunk_entries,
            chunk_lookup_entries,
            file_hashes,
            chunk_mappings,
            hash,
        })
    }

    fn parse_file_section<R: Read + Seek>(
        reader: &mut R,
        start: u64,
        end: u64,
    ) -> XetResult<(
        Vec<FileDataSequenceHeader>,
        Vec<FileDataSequenceEntry>,
        Vec<MerkleHash>,
    )> {
        reader.seek(SeekFrom::Start(start))?;
        let mut headers = Vec::new();
        let mut entries = Vec::new();
        let mut hashes = Vec::new();

        while reader.stream_position()? < end {
            let position = reader.stream_position()?;
            Self::ensure_record_fits("file sequence header", position, SEQUENCE_HEADER_SIZE, end)?;
            let header = FileDataSequenceHeader::deserialize(reader)?;
            let entries_len = u64::from(header.num_entries)
                .checked_mul(SEQUENCE_ENTRY_SIZE)
                .ok_or_else(|| {
                    crate::error::XetError::ParseError(
                        "File sequence entry byte length overflow".to_string(),
                    )
                })?;
            let entries_start = reader.stream_position()?;
            Self::ensure_record_fits("file sequence entries", entries_start, entries_len, end)?;

            hashes.push(header.file_hash);
            for _ in 0..header.num_entries {
                entries.push(FileDataSequenceEntry::deserialize(reader)?);
            }
            headers.push(header);
        }

        Ok((headers, entries, hashes))
    }

    fn parse_xorb_section<R: Read + Seek>(
        reader: &mut R,
        start: u64,
        end: u64,
    ) -> XetResult<(Vec<XorbChunkSequenceHeader>, Vec<XorbChunkSequenceEntry>)> {
        reader.seek(SeekFrom::Start(start))?;
        let mut headers = Vec::new();
        let mut entries = Vec::new();

        while reader.stream_position()? < end {
            let position = reader.stream_position()?;
            Self::ensure_record_fits("xorb sequence header", position, SEQUENCE_HEADER_SIZE, end)?;
            let header = XorbChunkSequenceHeader::deserialize(reader)?;
            let entries_len = u64::from(header.num_entries)
                .checked_mul(SEQUENCE_ENTRY_SIZE)
                .ok_or_else(|| {
                    crate::error::XetError::ParseError(
                        "Xorb sequence entry byte length overflow".to_string(),
                    )
                })?;
            let entries_start = reader.stream_position()?;
            Self::ensure_record_fits("xorb sequence entries", entries_start, entries_len, end)?;

            for _ in 0..header.num_entries {
                entries.push(XorbChunkSequenceEntry::deserialize(reader)?);
            }
            headers.push(header);
        }

        Ok((headers, entries))
    }

    fn validate_section_bounds(
        name: &str,
        start: u64,
        end: u64,
        footer_start: u64,
    ) -> XetResult<()> {
        if start < SHARD_HEADER_SIZE || end < start || end > footer_start {
            return Err(crate::error::XetError::ParseError(format!(
                "Invalid {} section bounds: {}..{} (footer at {})",
                name, start, end, footer_start
            )));
        }
        Ok(())
    }

    fn ensure_record_fits(name: &str, start: u64, len: u64, end: u64) -> XetResult<()> {
        let record_end = start.checked_add(len).ok_or_else(|| {
            crate::error::XetError::ParseError(format!("{} end offset overflow", name))
        })?;
        if record_end > end {
            return Err(crate::error::XetError::ParseError(format!(
                "Truncated {} at offset {}: needs {} bytes before section end {}",
                name, start, len, end
            )));
        }
        Ok(())
    }

    fn hash_reader<R: Read + Seek>(reader: &mut R, file_len: u64) -> XetResult<String> {
        use crate::util::StreamingHasher;

        reader.seek(SeekFrom::Start(0))?;
        let mut hasher = StreamingHasher::new();
        let mut remaining = file_len;
        let mut buffer = [0u8; 64 * 1024];
        while remaining > 0 {
            let requested = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
                crate::error::XetError::ParseError(
                    "Shard read size does not fit in usize".to_string(),
                )
            })?;
            let read = reader.read(&mut buffer[..requested])?;
            if read == 0 {
                return Err(crate::error::XetError::IoError(
                    std::io::ErrorKind::UnexpectedEof.into(),
                ));
            }
            hasher.update(&buffer[..read]);
            remaining -= u64::try_from(read).map_err(|_| {
                crate::error::XetError::ParseError(
                    "Shard read size does not fit in u64".to_string(),
                )
            })?;
        }
        Ok(hasher.finalize().to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
    use crate::hash::compute_data_hash;
    use tempfile::tempdir;

    fn test_hash(value: u8) -> MerkleHash {
        let mut bytes = [0u8; 32];
        bytes[0] = value;
        MerkleHash::from(bytes)
    }

    fn valid_shard() -> Vec<u8> {
        let xorb_hash = test_hash(1);
        let mut builder = ShardBuilder::new();
        let xorb_index = builder
            .add_xorb_with_raw_chunk_hashes(
                xorb_hash,
                32,
                24,
                vec![XorbChunkBuildEntry {
                    chunk_hash: test_hash(2),
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: 32,
                }],
                vec![test_hash(3)],
            )
            .unwrap();
        builder.add_file(
            test_hash(4),
            vec![FileSegment {
                xorb_hash,
                xorb_index,
                unpacked_segment_bytes: 32,
                chunk_index_start: 0,
                chunk_index_end: 1,
            }],
        );
        builder.build().unwrap()
    }

    fn overwrite_u64(data: &mut [u8], offset: usize, value: u64) {
        data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn parse_from_file_matches_in_memory_parse_and_hash() {
        let data = valid_shard();
        let dir = tempdir().unwrap();
        let path = dir.path().join("shard");
        std::fs::write(&path, &data).unwrap();

        let memory = MDBShardFile::parse(&data).unwrap();
        let disk = MDBShardFile::parse_from_file(&path).unwrap();

        assert_eq!(disk, memory);
        assert_eq!(disk.compute_hash(), compute_data_hash(&data).to_hex());
        assert_eq!(
            MDBShardFile::compute_hash_from_file(&path).unwrap(),
            disk.compute_hash()
        );
    }

    #[test]
    fn rejects_footer_offset_that_does_not_match_physical_layout() {
        let mut data = valid_shard();
        let footer_start = data.len() - SHARD_FOOTER_SIZE as usize;
        overwrite_u64(&mut data, footer_start + 200, 1);

        let error = MDBShardFile::parse(&data).unwrap_err();
        assert!(error.to_string().contains("Invalid footer offset"));
    }

    #[test]
    fn rejects_section_offset_beyond_footer() {
        let mut data = valid_shard();
        let footer_start = data.len() - SHARD_FOOTER_SIZE as usize;
        overwrite_u64(
            &mut data,
            footer_start + 8,
            u64::try_from(footer_start + 1).unwrap(),
        );

        let error = MDBShardFile::parse(&data).unwrap_err();
        assert!(error.to_string().contains("File section must start"));
    }

    #[test]
    fn rejects_chunk_lookup_count_overflow() {
        let mut data = valid_shard();
        let footer_start = data.len() - SHARD_FOOTER_SIZE as usize;
        overwrite_u64(&mut data, footer_start + 64, u64::MAX);

        let error = MDBShardFile::parse(&data).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Chunk lookup byte length overflow")
        );
    }

    #[test]
    fn rejects_truncated_file_sequence_entries() {
        let mut data = valid_shard();
        let file_num_entries_offset = SHARD_HEADER_SIZE as usize + 36;
        data[file_num_entries_offset..file_num_entries_offset + 4]
            .copy_from_slice(&2_u32.to_le_bytes());

        let error = MDBShardFile::parse(&data).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Truncated file sequence entries")
        );
    }
}
