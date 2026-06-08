use xet_server::format::xorb::XorbChunkHeader;
use xet_server::format::compression::CompressionScheme;
use std::io::Cursor;

#[test]
fn test_xorb_chunk_header_size() {
    assert_eq!(XorbChunkHeader::SIZE, 8);
}

#[test]
fn test_xorb_chunk_header_serialize() {
    let header = XorbChunkHeader {
        version: 0,
        compressed_length: 1000,
        compression_scheme: CompressionScheme::LZ4,
        uncompressed_length: 2000,
    };

    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();

    assert_eq!(buf.len(), 8);
}

#[test]
fn test_xorb_chunk_header_roundtrip() {
    let original = XorbChunkHeader {
        version: 0,
        compressed_length: 12345,
        compression_scheme: CompressionScheme::LZ4,
        uncompressed_length: 65536,
    };

    let mut buf = Vec::new();
    original.serialize(&mut buf).unwrap();

    let mut cursor = Cursor::new(&buf);
    let parsed = XorbChunkHeader::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.version, original.version);
    assert_eq!(parsed.compressed_length, original.compressed_length);
    assert_eq!(parsed.compression_scheme, original.compression_scheme);
    assert_eq!(parsed.uncompressed_length, original.uncompressed_length);
}

#[test]
fn test_xorb_chunk_header_max_lengths() {
    // Max compressed length: 16MB (3 bytes max value = 16777215)
    let header = XorbChunkHeader {
        version: 0,
        compressed_length: 16_777_215,
        compression_scheme: CompressionScheme::LZ4,
        uncompressed_length: 131071, // 128KB - 1
    };

    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();

    let mut cursor = Cursor::new(&buf);
    let parsed = XorbChunkHeader::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.compressed_length, header.compressed_length);
    assert_eq!(parsed.uncompressed_length, header.uncompressed_length);
}

#[test]
fn test_xorb_chunk_header_byte_layout() {
    let header = XorbChunkHeader {
        version: 0,
        compressed_length: 0x123456,
        compression_scheme: CompressionScheme::LZ4,
        uncompressed_length: 0xABCDEF,
    };

    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();

    // Expected layout (8 bytes):
    // [version:1] [compressed_length:3 LE] [scheme:1] [uncompressed_length:3 LE]
    assert_eq!(buf[0], 0); // version
    assert_eq!(&buf[1..4], &[0x56, 0x34, 0x12]); // compressed_length LE
    assert_eq!(buf[4], 1); // LZ4
    assert_eq!(&buf[5..8], &[0xEF, 0xCD, 0xAB]); // uncompressed_length LE
}