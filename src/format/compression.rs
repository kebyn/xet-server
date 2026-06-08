use lz4_flex::{compress_prepend_size, decompress_size_prepended};
use crate::error::{XetError, Result};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionScheme {
    None = 0,
    LZ4 = 1,
    ByteGrouping4LZ4 = 2,
}

impl TryFrom<u8> for CompressionScheme {
    type Error = XetError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(CompressionScheme::None),
            1 => Ok(CompressionScheme::LZ4),
            2 => Ok(CompressionScheme::ByteGrouping4LZ4),
            _ => Err(XetError::ParseError(format!("Invalid compression scheme: {}", value))),
        }
    }
}

pub fn compress(scheme: CompressionScheme, data: &[u8]) -> Result<Vec<u8>> {
    match scheme {
        CompressionScheme::None => Ok(data.to_vec()),
        CompressionScheme::LZ4 => Ok(compress_prepend_size(data)),
        CompressionScheme::ByteGrouping4LZ4 => {
            // BG4-LZ4 is complex; for now just use LZ4
            // TODO: Implement proper BG4 byte grouping
            Ok(compress_prepend_size(data))
        }
    }
}

pub fn decompress(scheme: CompressionScheme, data: &[u8], _original_size: usize) -> Result<Vec<u8>> {
    match scheme {
        CompressionScheme::None => Ok(data.to_vec()),
        CompressionScheme::LZ4 => {
            decompress_size_prepended(data)
                .map_err(|e| XetError::ParseError(format!("LZ4 decompression failed: {}", e)))
        }
        CompressionScheme::ByteGrouping4LZ4 => {
            // TODO: Implement proper BG4 regrouping
            decompress_size_prepended(data)
                .map_err(|e| XetError::ParseError(format!("BG4-LZ4 decompression failed: {}", e)))
        }
    }
}