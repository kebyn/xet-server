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

/// BG4 (Byte Grouping 4) byte rearrangement
///
/// Rearranges data by grouping bytes at positions 0, 4, 8, ... together,
/// then bytes at positions 1, 5, 9, ... together, etc.
/// This improves compression for numerical data where bytes at the same
/// position within multi-byte values tend to be similar.
///
/// For example: [A0, A1, A2, A3, B0, B1, B2, B3] becomes [A0, B0, A1, B1, A2, B2, A3, B3]
fn bg4_split(data: &[u8]) -> Vec<u8> {
    let len = data.len();
    if len == 0 {
        return Vec::new();
    }

    // Calculate sizes for each of the 4 groups
    let base_size = len / 4;
    let remainder = len % 4;

    let mut result = Vec::with_capacity(len);

    // Group 0: bytes at positions 0, 4, 8, ...
    for i in 0..(base_size + if remainder > 0 { 1 } else { 0 }) {
        let pos = i * 4;
        if pos < len {
            result.push(data[pos]);
        }
    }

    // Group 1: bytes at positions 1, 5, 9, ...
    for i in 0..(base_size + if remainder > 1 { 1 } else { 0 }) {
        let pos = i * 4 + 1;
        if pos < len {
            result.push(data[pos]);
        }
    }

    // Group 2: bytes at positions 2, 6, 10, ...
    for i in 0..(base_size + if remainder > 2 { 1 } else { 0 }) {
        let pos = i * 4 + 2;
        if pos < len {
            result.push(data[pos]);
        }
    }

    // Group 3: bytes at positions 3, 7, 11, ...
    for i in 0..base_size {
        let pos = i * 4 + 3;
        if pos < len {
            result.push(data[pos]);
        }
    }

    result
}

/// Reverse BG4 byte rearrangement
///
/// Restores the original byte order from BG4-grouped data.
/// Returns an error if the data length doesn't match the expected size.
fn bg4_regroup(data: &[u8], original_len: usize) -> Result<Vec<u8>> {
    if original_len == 0 {
        if !data.is_empty() {
            return Err(XetError::ParseError("BG4 regroup: expected empty data".into()));
        }
        return Ok(Vec::new());
    }

    let mut result = vec![0u8; original_len];

    // Calculate group sizes
    let base_size = original_len / 4;
    let remainder = original_len % 4;

    let group0_size = base_size + if remainder > 0 { 1 } else { 0 };
    let group1_size = base_size + if remainder > 1 { 1 } else { 0 };
    let group2_size = base_size + if remainder > 2 { 1 } else { 0 };
    let group3_size = base_size;

    // Validate data length upfront - BG4 is a permutation, so sizes must match
    let expected_len = group0_size + group1_size + group2_size + group3_size;
    if data.len() != expected_len {
        return Err(XetError::ParseError(format!(
            "BG4 regroup: data length mismatch - expected {} bytes, got {}",
            expected_len,
            data.len()
        )));
    }

    let mut offset = 0;

    // Restore Group 0: positions 0, 4, 8, ...
    for i in 0..group0_size {
        let pos = i * 4;
        if pos < original_len {
            result[pos] = data[offset];
            offset += 1;
        }
    }

    // Restore Group 1: positions 1, 5, 9, ...
    for i in 0..group1_size {
        let pos = i * 4 + 1;
        if pos < original_len {
            result[pos] = data[offset];
            offset += 1;
        }
    }

    // Restore Group 2: positions 2, 6, 10, ...
    for i in 0..group2_size {
        let pos = i * 4 + 2;
        if pos < original_len {
            result[pos] = data[offset];
            offset += 1;
        }
    }

    // Restore Group 3: positions 3, 7, 11, ...
    for i in 0..group3_size {
        let pos = i * 4 + 3;
        if pos < original_len {
            result[pos] = data[offset];
            offset += 1;
        }
    }

    // Final validation - all data should be consumed
    if offset != data.len() {
        return Err(XetError::ParseError(format!(
            "BG4 regroup: data consumption error - processed {} of {} bytes",
            offset,
            data.len()
        )));
    }

    Ok(result)
}

pub fn compress(scheme: CompressionScheme, data: &[u8]) -> Result<Vec<u8>> {
    match scheme {
        CompressionScheme::None => Ok(data.to_vec()),
        CompressionScheme::LZ4 => Ok(compress_prepend_size(data)),
        CompressionScheme::ByteGrouping4LZ4 => {
            // Apply BG4 byte grouping, then LZ4 compression
            let grouped = bg4_split(data);
            Ok(compress_prepend_size(&grouped))
        }
    }
}

pub fn decompress(scheme: CompressionScheme, data: &[u8], original_size: usize) -> Result<Vec<u8>> {
    match scheme {
        CompressionScheme::None => {
            if data.len() != original_size {
                return Err(XetError::ParseError(format!(
                    "Uncompressed data size {} != expected {}", data.len(), original_size
                )));
            }
            Ok(data.to_vec())
        }
        CompressionScheme::LZ4 => {
            check_lz4_prefix(data, original_size)?;
            let out = decompress_size_prepended(data)
                .map_err(|e| XetError::ParseError(format!("LZ4 decompression failed: {}", e)))?;
            if out.len() != original_size {
                return Err(XetError::ParseError(format!(
                    "LZ4 output size {} != expected {}", out.len(), original_size
                )));
            }
            Ok(out)
        }
        CompressionScheme::ByteGrouping4LZ4 => {
            check_lz4_prefix(data, original_size)?;
            let decompressed = decompress_size_prepended(data)
                .map_err(|e| XetError::ParseError(format!("BG4-LZ4 decompression failed: {}", e)))?;
            bg4_regroup(&decompressed, original_size)
        }
    }
}

/// lz4_flex 在压缩数据头部写入 4 字节小端 u32 表示解压后长度。
/// 解压前用可信的 `original_size`(来自 chunk header 的 uncompressed_length)
/// 比对该前缀,防止恶意/损坏前缀触发巨量内存分配(解压炸弹)。
fn check_lz4_prefix(data: &[u8], original_size: usize) -> Result<()> {
    if data.len() < 4 {
        return Err(XetError::ParseError(
            "LZ4 data too short for size prefix".to_string(),
        ));
    }
    let prefix = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if prefix != original_size {
        return Err(XetError::ParseError(format!(
            "LZ4 size prefix {} does not match expected uncompressed size {}",
            prefix, original_size
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decompress_lz4_rejects_oversized_prefix() {
        // 前缀谎称 ~4GB,实际数据极小 → 必须在分配前拒绝。
        let mut data = vec![0xFFu8, 0xFF, 0xFF, 0xFF]; // LE u32 = 4294967295
        data.extend_from_slice(&[0u8; 4]);
        let result = decompress(CompressionScheme::LZ4, &data, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_decompress_lz4_roundtrip_ok() {
        let original = b"hello world, this is some test data".repeat(10);
        let compressed = compress(CompressionScheme::LZ4, &original).unwrap();
        let out = decompress(CompressionScheme::LZ4, &compressed, original.len()).unwrap();
        assert_eq!(out, original);
    }

    #[test]
    fn test_bg4_split_empty() {
        let data = b"";
        let result = bg4_split(data);
        assert_eq!(result, Vec::<u8>::new());
    }

    #[test]
    fn test_bg4_split_small() {
        let data = b"ABCD";
        let result = bg4_split(data);
        assert_eq!(result, b"ABCD");
    }

    #[test]
    fn test_bg4_split_8bytes() {
        let data = b"ABCDEFGH";
        let result = bg4_split(data);
        // Group 0: A, E (positions 0, 4)
        // Group 1: B, F (positions 1, 5)
        // Group 2: C, G (positions 2, 6)
        // Group 3: D, H (positions 3, 7)
        assert_eq!(result, b"AEBFCGDH");
    }

    #[test]
    fn test_bg4_split_9bytes() {
        let data = b"ABCDEFGHI";
        let result = bg4_split(data);
        // Group 0: A, E, I (positions 0, 4, 8)
        // Group 1: B, F (positions 1, 5)
        // Group 2: C, G (positions 2, 6)
        // Group 3: D, H (positions 3, 7)
        assert_eq!(result, b"AEIBFCGDH");
    }

    #[test]
    fn test_bg4_regroup_empty() {
        let data = b"";
        let result = bg4_regroup(data, 0).unwrap();
        assert_eq!(result, Vec::<u8>::new());
    }

    #[test]
    fn test_bg4_roundtrip_8bytes() {
        let original = b"ABCDEFGH";
        let grouped = bg4_split(original);
        let restored = bg4_regroup(&grouped, original.len()).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn test_bg4_roundtrip_9bytes() {
        let original = b"ABCDEFGHI";
        let grouped = bg4_split(original);
        let restored = bg4_regroup(&grouped, original.len()).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn test_bg4_roundtrip_large() {
        let original: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let grouped = bg4_split(&original);
        let restored = bg4_regroup(&grouped, original.len()).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn test_bg4_regroup_truncated_data() {
        // Test that truncated data returns an error instead of silently producing wrong output
        let original = b"ABCDEFGH";
        let grouped = bg4_split(original);
        let truncated = &grouped[..grouped.len() - 2]; // Remove last 2 bytes
        let result = bg4_regroup(truncated, original.len());
        assert!(result.is_err());
    }

    #[test]
    fn test_bg4lz4_compress_decompress() {
        let original = b"Test data for BG4-LZ4 compression testing";
        let compressed = compress(CompressionScheme::ByteGrouping4LZ4, original).unwrap();
        let decompressed = decompress(CompressionScheme::ByteGrouping4LZ4, &compressed, original.len()).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_bg4lz4_numerical_data() {
        // BG4 should work well with numerical data
        let original: Vec<u8> = (0..100u32)
            .flat_map(|i| i.to_le_bytes())
            .collect();

        let compressed = compress(CompressionScheme::ByteGrouping4LZ4, &original).unwrap();
        let decompressed = decompress(CompressionScheme::ByteGrouping4LZ4, &compressed, original.len()).unwrap();

        assert_eq!(decompressed, original);
    }
}
