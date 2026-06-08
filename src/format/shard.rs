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