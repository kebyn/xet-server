use std::io::{Read, Result, Write, Seek, SeekFrom, Cursor};
use std::fs::File;
use std::path::Path;
use crate::error::Result as XetResult;

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
                72, 70, 82, 101, 112, 111, 77, 101, 116, 97, 68, 97, 116, 97, 0, 85,
                105, 103, 69, 106, 123, 129, 87, 131, 165, 189, 217, 92, 205, 209, 74, 169,
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

        Ok(Self { file_hash, file_flags, num_entries })
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

/// High-level shard file representation
///
/// Contains parsed metadata from a shard file for indexing and querying.
#[derive(Debug, Clone)]
pub struct MDBShardFile {
    pub header: MDBShardFileHeader,
    pub footer: MDBShardFileFooter,
    pub file_entries: Vec<FileDataSequenceHeader>,
    pub file_data_entries: Vec<FileDataSequenceEntry>,
    pub xorb_entries: Vec<XorbChunkSequenceHeader>,
    pub xorb_chunk_entries: Vec<XorbChunkSequenceEntry>,
    pub file_hashes: Vec<MerkleHash>,
    pub chunk_mappings: Vec<(MerkleHash, MerkleHash, u32)>, // (chunk_hash, xorb_hash, chunk_index)
    raw_data: Vec<u8>,
}

impl MDBShardFile {
    /// Parse a shard file from binary data
    pub fn parse(data: &[u8]) -> XetResult<Self> {
        let mut cursor = Cursor::new(data);

        // Parse header
        let header = MDBShardFileHeader::deserialize(&mut cursor)?;

        // Verify magic tag
        let expected_tag = MDBShardFileHeader::default().tag;
        if header.tag != expected_tag {
            return Err(crate::error::XetError::ParseError(
                "Invalid shard magic tag".to_string(),
            ));
        }

        // Verify footer offset
        if header.footer_size != 208 {
            return Err(crate::error::XetError::ParseError(
                format!("Invalid footer size: expected 208, got {}", header.footer_size),
            ));
        }

        // Parse footer (at end of file)
        // Validate minimum size before subtraction to prevent panic
        if data.len() < 208 {
            return Err(crate::error::XetError::ParseError(
                format!("Shard data too small: {} bytes, minimum 208 bytes required", data.len())
            ));
        }
        let footer_start = data.len() - 208;
        let mut footer_cursor = Cursor::new(&data[footer_start..]);
        let footer = MDBShardFileFooter::deserialize(&mut footer_cursor)?;

        // Parse file info section
        let mut file_entries = Vec::new();
        let mut file_data_entries = Vec::new();
        let mut file_hashes = Vec::new();
        if footer.file_info_offset > 0 && footer.file_info_offset < data.len() as u64 {
            let mut file_cursor = Cursor::new(&data[footer.file_info_offset as usize..]);

            // Parse all file entries
            loop {
                let pos = file_cursor.position() as usize;
                if pos + 48 > data.len() - footer_start {
                    break; // Reached end of file info section
                }

                match FileDataSequenceHeader::deserialize(&mut file_cursor) {
                    Ok(file_header) => {
                        file_hashes.push(file_header.file_hash);

                        // Parse file entries for this file
                        for _ in 0..file_header.num_entries {
                            match FileDataSequenceEntry::deserialize(&mut file_cursor) {
                                Ok(entry) => file_data_entries.push(entry),
                                Err(_) => break,
                            }
                        }

                        file_entries.push(file_header);
                    }
                    Err(_) => break,
                }
            }
        }

        // Parse xorb info section
        let mut xorb_entries = Vec::new();
        let mut xorb_chunk_entries = Vec::new();
        let mut chunk_mappings = Vec::new();
        if footer.xorb_info_offset > 0 && footer.xorb_info_offset < data.len() as u64 {
            let mut xorb_cursor = Cursor::new(&data[footer.xorb_info_offset as usize..]);

            // Parse all xorb entries
            loop {
                let pos = xorb_cursor.position() as usize;
                if pos + 48 > data.len() - footer_start {
                    break; // Reached end of xorb info section
                }

                match XorbChunkSequenceHeader::deserialize(&mut xorb_cursor) {
                    Ok(xorb_header) => {
                        let xorb_hash = xorb_header.xorb_hash;
                        let num_chunks = xorb_header.num_entries;
                        xorb_entries.push(xorb_header);

                        // Parse chunk entries for this xorb
                        for chunk_index in 0..num_chunks {
                            match XorbChunkSequenceEntry::deserialize(&mut xorb_cursor) {
                                Ok(chunk_entry) => {
                                    // Add to chunk mappings: (chunk_hash, xorb_hash, chunk_index)
                                    chunk_mappings.push((
                                        chunk_entry.chunk_hash,
                                        xorb_hash,
                                        chunk_index,
                                    ));
                                    xorb_chunk_entries.push(chunk_entry);
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }

        Ok(Self {
            header,
            footer,
            file_entries,
            file_data_entries,
            xorb_entries,
            xorb_chunk_entries,
            file_hashes,
            chunk_mappings,
            raw_data: data.to_vec(),
        })
    }

    /// Parse a shard file from a file on disk without loading the entire file into RAM.
    ///
    /// Reads only the header (start) and footer (end) from the file.
    /// `raw_data` is left empty — the hash must be computed externally (e.g., during
    /// streaming upload) since this method does not retain the file contents.
    ///
    /// Use `compute_hash_from_file(path)` or a streaming hasher to get the shard hash.
    pub fn parse_from_file(path: &Path) -> XetResult<Self> {
        let mut file = File::open(path).map_err(|e| {
            crate::error::XetError::IoError(std::io::Error::other(
                format!("Failed to open shard file {}: {}", path.display(), e),
            ))
        })?;

        let file_len = file.metadata().map_err(|e| {
            crate::error::XetError::IoError(std::io::Error::other(
                format!("Failed to get file metadata: {}", e),
            ))
        })?.len();

        if file_len < 256 {
            return Err(crate::error::XetError::ParseError(
                format!("Shard file too small: {} bytes, minimum 256 bytes required (48-byte header + 208-byte footer)", file_len)
            ));
        }

        // Read header from start of file
        // Read enough bytes for the header (48 bytes: 32 tag + 8 version + 8 footer_size)
        let mut header_buf = [0u8; 48];
        file.read_exact(&mut header_buf).map_err(|e| {
            crate::error::XetError::IoError(std::io::Error::other(
                format!("Failed to read shard header: {}", e),
            ))
        })?;
        let mut header_cursor = Cursor::new(&header_buf[..]);
        let header = MDBShardFileHeader::deserialize(&mut header_cursor)?;

        // Verify magic tag
        let expected_tag = MDBShardFileHeader::default().tag;
        if header.tag != expected_tag {
            return Err(crate::error::XetError::ParseError(
                "Invalid shard magic tag".to_string(),
            ));
        }

        // Verify footer size
        if header.footer_size != 208 {
            return Err(crate::error::XetError::ParseError(
                format!("Invalid footer size: expected 208, got {}", header.footer_size),
            ));
        }

        // Read footer from end of file
        file.seek(SeekFrom::End(-208)).map_err(|e| {
            crate::error::XetError::IoError(std::io::Error::other(
                format!("Failed to seek to shard footer: {}", e),
            ))
        })?;
        let mut footer_buf = [0u8; 208];
        file.read_exact(&mut footer_buf).map_err(|e| {
            crate::error::XetError::IoError(std::io::Error::other(
                format!("Failed to read shard footer: {}", e),
            ))
        })?;
        let mut footer_cursor = Cursor::new(&footer_buf[..]);
        let footer = MDBShardFileFooter::deserialize(&mut footer_cursor)?;

        // Simplified parse — same as parse() above
        let file_hashes = Vec::new();
        let chunk_mappings = Vec::new();
        let file_entries = Vec::new();
        let file_data_entries = Vec::new();
        let xorb_entries = Vec::new();
        let xorb_chunk_entries = Vec::new();

        Ok(Self {
            header,
            footer,
            file_entries,
            file_data_entries,
            xorb_entries,
            xorb_chunk_entries,
            file_hashes,
            chunk_mappings,
            raw_data: Vec::new(), // Hash computed externally via streaming
        })
    }

    /// Compute the BLAKE3 hash of a shard file on disk.
    /// Reads the file incrementally to bound memory usage.
    pub fn compute_hash_from_file(path: &Path) -> XetResult<String> {
        use crate::util::StreamingHasher;
        let mut file = File::open(path).map_err(|e| {
            crate::error::XetError::IoError(std::io::Error::other(
                format!("Failed to open shard file for hashing: {}", e),
            ))
        })?;

        let mut hasher = StreamingHasher::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(|e| {
                crate::error::XetError::IoError(std::io::Error::other(
                    format!("Failed to read shard file for hashing: {}", e),
                ))
            })?;
            if n == 0 { break; }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().to_hex())
    }

    /// Compute hash of the shard (using the raw data)
    pub fn compute_hash(&self) -> String {
        use crate::hash::compute_data_hash;
        let hash = compute_data_hash(&self.raw_data);
        hash.to_hex()
    }

    /// Get file hashes contained in this shard
    pub fn file_hashes(&self) -> &[MerkleHash] {
        &self.file_hashes
    }

    /// Get chunk-to-xorb mappings
    pub fn chunk_mappings(&self) -> &[(MerkleHash, MerkleHash, u32)] {
        &self.chunk_mappings
    }
}