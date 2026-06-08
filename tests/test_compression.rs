use xet_server::format::compression::{CompressionScheme, compress, decompress};

#[test]
fn test_compression_none() {
    let data = b"test data for compression";
    let compressed = compress(CompressionScheme::None, data).unwrap();
    let decompressed = decompress(CompressionScheme::None, &compressed, data.len()).unwrap();
    assert_eq!(decompressed, data);
}

#[test]
fn test_compression_lz4() {
    let data = b"test data for LZ4 compression".repeat(100);
    let compressed = compress(CompressionScheme::LZ4, &data).unwrap();

    // Compressed should be smaller
    assert!(compressed.len() < data.len());

    let decompressed = decompress(CompressionScheme::LZ4, &compressed, data.len()).unwrap();
    assert_eq!(decompressed, data.as_slice());
}

#[test]
fn test_compression_scheme_values() {
    assert_eq!(CompressionScheme::None as u8, 0);
    assert_eq!(CompressionScheme::LZ4 as u8, 1);
    assert_eq!(CompressionScheme::ByteGrouping4LZ4 as u8, 2);
}

#[test]
fn test_compression_empty_data() {
    let data = b"";
    let compressed = compress(CompressionScheme::None, data).unwrap();
    let decompressed = decompress(CompressionScheme::None, &compressed, 0).unwrap();
    assert_eq!(decompressed, data);
}

#[test]
fn test_compression_scheme_from_u8() {
    assert_eq!(CompressionScheme::try_from(0u8).unwrap(), CompressionScheme::None);
    assert_eq!(CompressionScheme::try_from(1u8).unwrap(), CompressionScheme::LZ4);
    assert_eq!(CompressionScheme::try_from(2u8).unwrap(), CompressionScheme::ByteGrouping4LZ4);
    assert!(CompressionScheme::try_from(99u8).is_err());
}