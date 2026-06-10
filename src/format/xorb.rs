use std::io::{Read, Write, Result, Seek, SeekFrom};
use std::fs::File;
use std::path::Path;
use crate::format::compression::CompressionScheme;
use crate::error::Result as XetResult;
use crate::error::XetError;
use crate::types::MerkleHash;

/// Xorb chunk header (8 bytes, packed)
///
/// Layout:
/// - version: u8 (1 byte)
/// - compressed_length: 3 bytes LE (max 16MB)
/// - compression_scheme: u8 (1 byte)
/// - uncompressed_length: 3 bytes LE (max 128KB)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XorbChunkHeader {
    pub version: u8,
    pub compressed_length: u32,
    pub compression_scheme: CompressionScheme,
    pub uncompressed_length: u32,
}

impl XorbChunkHeader {
    pub const SIZE: usize = 8;

    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&[self.version])?;

        // Write compressed_length as 3 bytes LE
        let cl_bytes = self.compressed_length.to_le_bytes();
        writer.write_all(&cl_bytes[0..3])?;

        writer.write_all(&[self.compression_scheme as u8])?;

        // Write uncompressed_length as 3 bytes LE
        let ul_bytes = self.uncompressed_length.to_le_bytes();
        writer.write_all(&ul_bytes[0..3])?;

        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        let mut version_buf = [0u8; 1];
        reader.read_exact(&mut version_buf)?;
        let version = version_buf[0];

        let mut cl_buf = [0u8; 3];
        reader.read_exact(&mut cl_buf)?;
        let compressed_length = u32::from_le_bytes([cl_buf[0], cl_buf[1], cl_buf[2], 0]);

        let mut scheme_buf = [0u8; 1];
        reader.read_exact(&mut scheme_buf)?;
        let compression_scheme = CompressionScheme::try_from(scheme_buf[0])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let mut ul_buf = [0u8; 3];
        reader.read_exact(&mut ul_buf)?;
        let uncompressed_length = u32::from_le_bytes([ul_buf[0], ul_buf[1], ul_buf[2], 0]);

        Ok(Self {
            version,
            compressed_length,
            compression_scheme,
            uncompressed_length,
        })
    }
}

/// Xorb object footer (XorbObjectInfoV1)
///
/// Contains chunk hashes and boundary information for range queries.
/// Stored at the end of each Xorb object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XorbObjectInfoV1 {
    pub xorb_hash: MerkleHash,
    pub chunk_hashes: Vec<MerkleHash>,
    pub chunk_boundary_offsets: Vec<u32>,
    pub unpacked_chunk_offsets: Vec<u32>,
}

impl XorbObjectInfoV1 {
    pub const IDENT_MAIN: [u8; 7] = *b"XETBLOB";
    pub const IDENT_HASHES: [u8; 7] = *b"XBLBHSH";
    pub const IDENT_BOUNDARIES: [u8; 7] = *b"XBLBBND";

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.serialize(&mut buf).unwrap();
        buf
    }

    /// Deserialize from bytes
    pub fn from_bytes(data: &[u8]) -> XetResult<Self> {
        let mut cursor = std::io::Cursor::new(data);
        Self::deserialize(&mut cursor)
    }

    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        let num_chunks = self.chunk_hashes.len() as u32;

        // Hashes section
        writer.write_all(&Self::IDENT_HASHES)?;
        writer.write_all(&[0u8])?; // hashes_version
        writer.write_all(&num_chunks.to_le_bytes())?; // Store num_chunks here for easier parsing
        for hash in &self.chunk_hashes {
            writer.write_all(&hash.as_bytes())?;
        }

        // Boundaries section
        writer.write_all(&Self::IDENT_BOUNDARIES)?;
        writer.write_all(&[1u8])?; // boundaries_version
        for offset in &self.chunk_boundary_offsets {
            writer.write_all(&offset.to_le_bytes())?;
        }
        for offset in &self.unpacked_chunk_offsets {
            writer.write_all(&offset.to_le_bytes())?;
        }

        // Header
        writer.write_all(&Self::IDENT_MAIN)?;
        writer.write_all(&[1u8])?; // version
        writer.write_all(&self.xorb_hash.as_bytes())?;

        Ok(())
    }

    pub fn deserialize<R: Read>(reader: &mut R) -> XetResult<Self> {
        // Read hashes section
        let mut ident = [0u8; 7];
        reader.read_exact(&mut ident)?;
        if ident != Self::IDENT_HASHES {
            return Err(XetError::ParseError("Expected hashes section".into()));
        }

        let mut version = [0u8; 1];
        reader.read_exact(&mut version)?;

        let mut num_buf = [0u8; 4];
        reader.read_exact(&mut num_buf)?;
        let num_chunks = u32::from_le_bytes(num_buf);

        // Validate num_chunks to prevent unbounded allocation (DoS protection)
        // A 16MB xorb with 8KB min chunks gives max ~2048 chunks
        const MAX_CHUNKS_PER_XORB: u32 = 1_000_000;
        if num_chunks > MAX_CHUNKS_PER_XORB {
            return Err(XetError::ParseError(format!(
                "Too many chunks: {} exceeds maximum {}",
                num_chunks, MAX_CHUNKS_PER_XORB
            )));
        }

        let mut chunk_hashes = Vec::with_capacity(num_chunks as usize);
        for _ in 0..num_chunks {
            let mut hash_bytes = [0u8; 32];
            reader.read_exact(&mut hash_bytes)?;
            chunk_hashes.push(MerkleHash::from(hash_bytes));
        }

        // Read boundaries section
        reader.read_exact(&mut ident)?;
        if ident != Self::IDENT_BOUNDARIES {
            return Err(XetError::ParseError("Expected boundaries section".into()));
        }

        reader.read_exact(&mut version)?;

        let mut chunk_boundary_offsets = Vec::with_capacity(num_chunks as usize);
        for _ in 0..num_chunks {
            reader.read_exact(&mut num_buf)?;
            chunk_boundary_offsets.push(u32::from_le_bytes(num_buf));
        }

        let mut unpacked_chunk_offsets = Vec::with_capacity(num_chunks as usize);
        for _ in 0..num_chunks {
            reader.read_exact(&mut num_buf)?;
            unpacked_chunk_offsets.push(u32::from_le_bytes(num_buf));
        }

        // Read header
        reader.read_exact(&mut ident)?;
        if ident != Self::IDENT_MAIN {
            return Err(XetError::ParseError("Expected main ident".into()));
        }

        reader.read_exact(&mut version)?;

        let mut hash_bytes = [0u8; 32];
        reader.read_exact(&mut hash_bytes)?;
        let xorb_hash = MerkleHash::from(hash_bytes);

        Ok(Self {
            xorb_hash,
            chunk_hashes,
            chunk_boundary_offsets,
            unpacked_chunk_offsets,
        })
    }
}

/// Verify xorb integrity by checking chunk hashes and xorb hash
///
/// This function:
/// 1. Parses the xorb footer to extract chunk hashes and xorb hash
/// 2. Extracts each chunk from the xorb data and verifies its hash
/// 3. Computes the aggregated xorb hash and verifies it matches
///
/// Returns Ok(()) if verification passes, or an error if any check fails.
pub fn verify_xorb(data: &[u8]) -> XetResult<()> {
    // Find the footer by searching for the hashes section ident
    let hashes_ident = XorbObjectInfoV1::IDENT_HASHES;
    let mut footer_start = None;

    for i in 0..data.len().saturating_sub(7) {
        if &data[i..i+7] == &hashes_ident {
            footer_start = Some(i);
            break;
        }
    }

    let footer_start = footer_start.ok_or_else(|| {
        XetError::ParseError("Could not find xorb footer".into())
    })?;

    // Parse the footer
    let footer = XorbObjectInfoV1::from_bytes(&data[footer_start..])?;

    // Verify chunk hashes and collect chunk info for xorb hash computation
    let mut chunk_info: Vec<(MerkleHash, u64)> = Vec::with_capacity(footer.chunk_hashes.len());
    let mut current_offset = 0;

    for (i, expected_hash) in footer.chunk_hashes.iter().enumerate() {
        // Get chunk end offset from boundary offsets
        let chunk_end = if i < footer.chunk_boundary_offsets.len() {
            footer.chunk_boundary_offsets[i] as usize
        } else {
            // Last chunk ends at footer start
            footer_start
        };

        if chunk_end > data.len() || chunk_end < current_offset {
            return Err(XetError::ParseError(format!(
                "Invalid chunk boundary: chunk {} at {}-{} exceeds data bounds",
                i, current_offset, chunk_end
            )));
        }

        let chunk_data = &data[current_offset..chunk_end];
        let chunk_size = chunk_data.len() as u64;

        // Verify chunk hash
        let actual_hash = crate::hash::compute_data_hash(chunk_data);
        if actual_hash != *expected_hash {
            return Err(XetError::ParseError(format!(
                "Chunk {} hash mismatch: expected {}, got {}",
                i,
                expected_hash.to_hex(),
                actual_hash.to_hex()
            )));
        }

        chunk_info.push((actual_hash, chunk_size));
        current_offset = chunk_end;
    }

    // Verify xorb hash (computed from chunk hashes and sizes)
    let computed_xorb_hash = crate::hash::xorb_hash(&chunk_info);
    if computed_xorb_hash != footer.xorb_hash {
        return Err(XetError::ParseError(format!(
            "Xorb hash mismatch: expected {}, got {}",
            footer.xorb_hash.to_hex(),
            computed_xorb_hash.to_hex()
        )));
    }

    Ok(())
}

/// Verify xorb integrity from a file on disk without loading the entire file into RAM.
///
/// Peak memory usage: O(64KB) regardless of xorb or chunk size.
///
/// This function:
/// 1. Reads the tail of the file to locate and parse the footer
/// 2. For each chunk: seeks to offset, reads incrementally, hashes, verifies
/// 3. Computes the aggregated xorb hash and verifies it matches
pub fn verify_xorb_from_file(path: &Path) -> XetResult<()> {
    let mut file = File::open(path).map_err(|e| {
        XetError::IoError(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to open xorb file {}: {}", path.display(), e),
        ))
    })?;
    let file_len = file.metadata().map_err(|e| {
        XetError::IoError(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to get file metadata: {}", e),
        ))
    })?.len();

    if file_len < 8 {
        return Err(XetError::ParseError(format!(
            "Xorb file too small: {} bytes", file_len
        )));
    }

    // Read last 64KB (or entire file if smaller) to find the footer
    let scan_size = std::cmp::min(file_len, 64 * 1024) as usize;
    let mut tail_buf = vec![0u8; scan_size];
    file.seek(SeekFrom::End(-(scan_size as i64))).map_err(|e| {
        XetError::IoError(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to seek to file tail: {}", e),
        ))
    })?;
    file.read_exact(&mut tail_buf).map_err(|e| {
        XetError::IoError(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to read file tail: {}", e),
        ))
    })?;

    // The tail was read from offset (file_len - scan_size) in the file.
    // Search for IDENT_HASHES in tail_buf, scanning from the END backwards.
    // The footer is at the very end of the xorb, so the LAST occurrence of
    // IDENT_HASHES in the tail buffer is the real footer. Searching from the
    // end also prevents false positives if chunk data happens to contain the
    // IDENT_HASHES magic bytes — those would appear before the real footer.
    let tail_file_offset = file_len - scan_size as u64;
    let hashes_ident = XorbObjectInfoV1::IDENT_HASHES;
    let mut footer_start_in_file: Option<u64> = None;

    // Iterate backwards from the last possible start position
    let max_start = tail_buf.len().saturating_sub(7);
    for i in (0..=max_start).rev() {
        if &tail_buf[i..i + 7] == &hashes_ident {
            footer_start_in_file = Some(tail_file_offset + i as u64);
            break;
        }
    }

    let footer_start = footer_start_in_file.ok_or_else(|| {
        XetError::ParseError("Could not find xorb footer in file".into())
    })?;

    // Parse footer from tail_buf at the position where IDENT_HASHES was found
    let footer_offset_in_tail = (footer_start - tail_file_offset) as usize;
    let footer = XorbObjectInfoV1::from_bytes(&tail_buf[footer_offset_in_tail..])?;

    // Verify chunk hashes by reading each chunk from the file incrementally
    let mut chunk_info: Vec<(MerkleHash, u64)> = Vec::with_capacity(footer.chunk_hashes.len());
    let mut current_offset: u64 = 0;
    let mut read_buf = [0u8; 64 * 1024]; // 64KB read buffer

    for (i, expected_hash) in footer.chunk_hashes.iter().enumerate() {
        let chunk_end = if i < footer.chunk_boundary_offsets.len() {
            footer.chunk_boundary_offsets[i] as u64
        } else {
            // Last chunk ends at footer start
            footer_start
        };

        if chunk_end > file_len || chunk_end < current_offset {
            return Err(XetError::ParseError(format!(
                "Invalid chunk boundary: chunk {} at {}-{} exceeds file bounds (len={})",
                i, current_offset, chunk_end, file_len
            )));
        }

        let chunk_size = chunk_end - current_offset;

        // Seek to chunk start and hash incrementally
        file.seek(SeekFrom::Start(current_offset)).map_err(|e| {
            XetError::IoError(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to seek to chunk {}: {}", i, e),
            ))
        })?;

        let mut hasher = blake3::Hasher::new_keyed(&crate::hash::DATA_KEY);
        let mut remaining = chunk_size;
        while remaining > 0 {
            let to_read = std::cmp::min(remaining, read_buf.len() as u64) as usize;
            let n = file.read(&mut read_buf[..to_read]).map_err(|e| {
                XetError::IoError(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to read chunk {} data: {}", i, e),
                ))
            })?;
            if n == 0 {
                return Err(XetError::ParseError(format!(
                    "Unexpected EOF reading chunk {} (remaining={})",
                    i, remaining
                )));
            }
            hasher.update(&read_buf[..n]);
            remaining -= n as u64;
        }

        let actual_hash = MerkleHash::from(*hasher.finalize().as_bytes());
        if actual_hash != *expected_hash {
            return Err(XetError::ParseError(format!(
                "Chunk {} hash mismatch: expected {}, got {}",
                i,
                expected_hash.to_hex(),
                actual_hash.to_hex()
            )));
        }

        chunk_info.push((actual_hash, chunk_size));
        current_offset = chunk_end;
    }

    // Verify xorb hash (computed from chunk hashes and sizes)
    let computed_xorb_hash = crate::hash::xorb_hash(&chunk_info);
    if computed_xorb_hash != footer.xorb_hash {
        return Err(XetError::ParseError(format!(
            "Xorb hash mismatch: expected {}, got {}",
            footer.xorb_hash.to_hex(),
            computed_xorb_hash.to_hex()
        )));
    }

    Ok(())
}