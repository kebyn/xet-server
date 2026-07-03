use std::io::Cursor;
use xet_server::format::compression::CompressionScheme;
use xet_server::format::xorb::XorbChunkHeader;

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

use xet_server::format::xorb::XorbObjectInfoV1;
use xet_server::types::MerkleHash;

#[test]
fn test_xorb_footer_idents() {
    assert_eq!(&XorbObjectInfoV1::IDENT_MAIN, b"XETBLOB");
    assert_eq!(&XorbObjectInfoV1::IDENT_HASHES, b"XBLBHSH");
    assert_eq!(&XorbObjectInfoV1::IDENT_BOUNDARIES, b"XBLBBND");
}

#[test]
fn test_xorb_footer_roundtrip() {
    let footer = XorbObjectInfoV1 {
        xorb_hash: MerkleHash::from([0xAB; 32]),
        chunk_hashes: vec![MerkleHash::from([0x11; 32]), MerkleHash::from([0x22; 32])],
        chunk_boundary_offsets: vec![1000, 2000],
        unpacked_chunk_offsets: vec![65536, 131072],
    };

    let mut buf = Vec::new();
    footer.serialize(&mut buf).unwrap();

    let mut cursor = std::io::Cursor::new(&buf);
    let parsed = XorbObjectInfoV1::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.xorb_hash, footer.xorb_hash);
    assert_eq!(parsed.chunk_hashes, footer.chunk_hashes);
    assert_eq!(parsed.chunk_boundary_offsets, footer.chunk_boundary_offsets);
    assert_eq!(parsed.unpacked_chunk_offsets, footer.unpacked_chunk_offsets);
}

#[test]
fn test_xorb_footer_rejects_unknown_section_versions() {
    let footer = XorbObjectInfoV1 {
        xorb_hash: MerkleHash::from([0xAB; 32]),
        chunk_hashes: vec![MerkleHash::from([0x11; 32])],
        chunk_boundary_offsets: vec![1000],
        unpacked_chunk_offsets: vec![0],
    };

    let mut buf = Vec::new();
    footer.serialize(&mut buf).unwrap();

    let mut bad_hashes_version = buf.clone();
    bad_hashes_version[7] = 99;
    assert!(XorbObjectInfoV1::from_bytes(&bad_hashes_version).is_err());

    let mut bad_boundaries_version = buf.clone();
    bad_boundaries_version[51] = 99;
    assert!(XorbObjectInfoV1::from_bytes(&bad_boundaries_version).is_err());

    let mut bad_main_version = buf;
    bad_main_version[67] = 99;
    assert!(XorbObjectInfoV1::from_bytes(&bad_main_version).is_err());
}

#[test]
fn test_xorb_footer_empty_chunks() {
    let footer = XorbObjectInfoV1 {
        xorb_hash: MerkleHash::default(),
        chunk_hashes: vec![],
        chunk_boundary_offsets: vec![],
        unpacked_chunk_offsets: vec![],
    };

    let mut buf = Vec::new();
    footer.serialize(&mut buf).unwrap();

    let mut cursor = std::io::Cursor::new(&buf);
    let parsed = XorbObjectInfoV1::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.chunk_hashes.len(), 0);
}

#[test]
fn test_xorb_footer_many_chunks() {
    let num_chunks = 100;
    let footer = XorbObjectInfoV1 {
        xorb_hash: MerkleHash::from([0xFF; 32]),
        chunk_hashes: (0..num_chunks)
            .map(|i| MerkleHash::from([i as u8; 32]))
            .collect(),
        chunk_boundary_offsets: (1..=num_chunks).map(|i| i * 1000).collect(),
        unpacked_chunk_offsets: (1..=num_chunks).map(|i| i * 65536).collect(),
    };

    let mut buf = Vec::new();
    footer.serialize(&mut buf).unwrap();

    let mut cursor = std::io::Cursor::new(&buf);
    let parsed = XorbObjectInfoV1::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.chunk_hashes.len(), num_chunks as usize);
    assert_eq!(parsed.chunk_boundary_offsets.len(), num_chunks as usize);
}
