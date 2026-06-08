use std::io::{Read, Write, Result};
use crate::format::compression::CompressionScheme;
use crate::error::Result as XetResult;

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