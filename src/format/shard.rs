use std::io::{Read, Result, Write};
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
        writer.write_all(self.file_hash.as_bytes())?; // 32 bytes
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
        writer.write_all(self.xorb_hash.as_bytes())?; // 32 bytes
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
        writer.write_all(self.xorb_hash.as_bytes())?; // 32 bytes
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
        writer.write_all(self.chunk_hash.as_bytes())?; // 32 bytes
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