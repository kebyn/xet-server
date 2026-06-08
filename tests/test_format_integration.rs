use xet_server::format::xorb::{XorbChunkHeader, XorbObjectInfoV1};
use xet_server::format::shard::{MDBShardFileHeader, MDBShardFileFooter};
use xet_server::format::compression::{CompressionScheme, compress, decompress};
use xet_server::hash::compute_data_hash;
use xet_server::types::MerkleHash;
use std::io::Cursor;

#[test]
fn test_xorb_chunk_with_compression() {
    // Simulate a chunk with LZ4 compression
    let original_data = b"This is test chunk data that will be compressed".repeat(10);
    let compressed_data = compress(CompressionScheme::LZ4, &original_data).unwrap();

    let header = XorbChunkHeader {
        version: 0,
        compressed_length: compressed_data.len() as u32,
        compression_scheme: CompressionScheme::LZ4,
        uncompressed_length: original_data.len() as u32,
    };

    // Serialize header + data
    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();
    buf.extend_from_slice(&compressed_data);

    // Deserialize
    let mut cursor = Cursor::new(&buf);
    let parsed_header = XorbChunkHeader::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed_header.compressed_length, header.compressed_length);
    assert_eq!(parsed_header.uncompressed_length, header.uncompressed_length);

    // Read compressed data
    let mut compressed_buf = vec![0u8; parsed_header.compressed_length as usize];
    std::io::Read::read_exact(&mut cursor, &mut compressed_buf).unwrap();

    // Decompress
    let decompressed = decompress(
        parsed_header.compression_scheme,
        &compressed_buf,
        parsed_header.uncompressed_length as usize,
    ).unwrap();

    assert_eq!(decompressed, original_data);
}

#[test]
fn test_complete_xorb_structure() {
    // Create a simple xorb with 3 chunks
    let chunks_data = vec![
        b"chunk1 data".to_vec(),
        b"chunk2 data longer".to_vec(),
        b"chunk3".to_vec(),
    ];

    let mut chunk_hashes = Vec::new();
    let mut chunk_boundaries = Vec::new();
    let mut unpacked_offsets = Vec::new();
    let mut offset = 0u32;

    for chunk_data in &chunks_data {
        chunk_hashes.push(compute_data_hash(chunk_data));
        offset += chunk_data.len() as u32;
        chunk_boundaries.push(offset);
        unpacked_offsets.push(offset);
    }

    let xorb_hash = compute_data_hash(b"test xorb");

    let footer = XorbObjectInfoV1 {
        xorb_hash,
        chunk_hashes: chunk_hashes.clone(),
        chunk_boundary_offsets: chunk_boundaries.clone(),
        unpacked_chunk_offsets: unpacked_offsets.clone(),
    };

    // Serialize footer
    let mut buf = Vec::new();
    footer.serialize(&mut buf).unwrap();

    // Deserialize
    let mut cursor = Cursor::new(&buf);
    let parsed = XorbObjectInfoV1::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.xorb_hash, xorb_hash);
    assert_eq!(parsed.chunk_hashes, chunk_hashes);
    assert_eq!(parsed.chunk_boundary_offsets, chunk_boundaries);
}

#[test]
fn test_shard_header_footer_integration() {
    let header = MDBShardFileHeader::default();
    let footer = MDBShardFileFooter {
        version: 1,
        file_info_offset: 48,
        xorb_info_offset: 1000,
        file_lookup_offset: 2000,
        file_lookup_num_entry: 10,
        xorb_lookup_offset: 2100,
        xorb_lookup_num_entry: 5,
        chunk_lookup_offset: 2200,
        chunk_lookup_num_entry: 100,
        chunk_hash_hmac_key: [0u8; 32],
        shard_creation_timestamp: 1700000000,
        shard_key_expiry: u64::MAX,
        stored_bytes_on_disk: 1024 * 1024,
        materialized_bytes: 2 * 1024 * 1024,
        stored_bytes: 1024 * 1024,
        footer_offset: 3000,
    };

    // Serialize both
    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();
    footer.serialize(&mut buf).unwrap();

    assert_eq!(buf.len(), 48 + 208);

    // Deserialize
    let mut cursor = Cursor::new(&buf);
    let parsed_header = MDBShardFileHeader::deserialize(&mut cursor).unwrap();
    let parsed_footer = MDBShardFileFooter::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed_header.version, header.version);
    assert_eq!(parsed_footer.file_info_offset, footer.file_info_offset);
}

#[test]
fn test_full_shard_with_entries() {
    use xet_server::format::shard::{FileDataSequenceHeader, FileDataSequenceEntry};

    // Create a minimal shard structure
    let header = MDBShardFileHeader::default();

    // File info section
    let file_header = FileDataSequenceHeader {
        file_hash: MerkleHash::from([0xAA; 32]),
        file_flags: 0,
        num_entries: 2,
    };

    let entries = vec![
        FileDataSequenceEntry {
            xorb_hash: MerkleHash::from([0x11; 32]),
            xorb_flags: 0,
            unpacked_segment_bytes: 1000,
            chunk_index_start: 0,
            chunk_index_end: 3,
        },
        FileDataSequenceEntry {
            xorb_hash: MerkleHash::from([0x22; 32]),
            xorb_flags: 0,
            unpacked_segment_bytes: 2000,
            chunk_index_start: 3,
            chunk_index_end: 7,
        },
    ];

    // Serialize
    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();
    file_header.serialize(&mut buf).unwrap();
    for entry in &entries {
        entry.serialize(&mut buf).unwrap();
    }

    // Deserialize and verify
    let mut cursor = Cursor::new(&buf);
    let parsed_header = MDBShardFileHeader::deserialize(&mut cursor).unwrap();
    assert_eq!(parsed_header.version, 2);

    let parsed_file_header = FileDataSequenceHeader::deserialize(&mut cursor).unwrap();
    assert_eq!(parsed_file_header.file_hash, file_header.file_hash);
    assert_eq!(parsed_file_header.num_entries, 2);

    for expected in &entries {
        let parsed = FileDataSequenceEntry::deserialize(&mut cursor).unwrap();
        assert_eq!(parsed.xorb_hash, expected.xorb_hash);
        assert_eq!(parsed.chunk_index_start, expected.chunk_index_start);
    }
}

#[test]
fn test_compression_roundtrip_with_header() {
    // Test that compression + header serialization work together
    let test_sizes = vec![100, 1000, 10000, 100000];

    for size in test_sizes {
        let data = vec![0xABu8; size];
        let compressed = compress(CompressionScheme::LZ4, &data).unwrap();

        let header = XorbChunkHeader {
            version: 0,
            compressed_length: compressed.len() as u32,
            compression_scheme: CompressionScheme::LZ4,
            uncompressed_length: size as u32,
        };

        let mut buf = Vec::new();
        header.serialize(&mut buf).unwrap();
        buf.extend_from_slice(&compressed);

        // Parse back
        let mut cursor = Cursor::new(&buf);
        let parsed_header = XorbChunkHeader::deserialize(&mut cursor).unwrap();

        let mut compressed_buf = vec![0u8; parsed_header.compressed_length as usize];
        std::io::Read::read_exact(&mut cursor, &mut compressed_buf).unwrap();

        let decompressed = decompress(
            parsed_header.compression_scheme,
            &compressed_buf,
            parsed_header.uncompressed_length as usize,
        ).unwrap();

        assert_eq!(decompressed.len(), size);
        assert_eq!(decompressed, data);
    }
}