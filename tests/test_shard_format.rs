use xet_server::format::shard::{MDBShardFileHeader, MDBShardFileFooter, FileDataSequenceHeader, FileDataSequenceEntry, XorbChunkSequenceHeader, XorbChunkSequenceEntry};
use xet_server::types::MerkleHash;
use std::io::Cursor;

#[test]
fn test_shard_header_magic() {
    let header = MDBShardFileHeader::default();
    assert_eq!(header.version, 2);
    assert_eq!(header.footer_size, 208);
}

#[test]
fn test_shard_header_roundtrip() {
    let header = MDBShardFileHeader::default();

    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();

    assert_eq!(buf.len(), 48);

    let mut cursor = Cursor::new(&buf);
    let parsed = MDBShardFileHeader::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.version, header.version);
    assert_eq!(parsed.footer_size, header.footer_size);
    assert_eq!(parsed.tag, header.tag);
}

#[test]
fn test_shard_footer_roundtrip() {
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

    let mut buf = Vec::new();
    footer.serialize(&mut buf).unwrap();

    assert_eq!(buf.len(), 208);

    let mut cursor = Cursor::new(&buf);
    let parsed = MDBShardFileFooter::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.version, footer.version);
    assert_eq!(parsed.file_info_offset, footer.file_info_offset);
    assert_eq!(parsed.xorb_info_offset, footer.xorb_info_offset);
    assert_eq!(parsed.file_lookup_offset, footer.file_lookup_offset);
    assert_eq!(parsed.file_lookup_num_entry, footer.file_lookup_num_entry);
    assert_eq!(parsed.xorb_lookup_offset, footer.xorb_lookup_offset);
    assert_eq!(parsed.xorb_lookup_num_entry, footer.xorb_lookup_num_entry);
    assert_eq!(parsed.chunk_lookup_offset, footer.chunk_lookup_offset);
    assert_eq!(parsed.chunk_lookup_num_entry, footer.chunk_lookup_num_entry);
    assert_eq!(parsed.shard_creation_timestamp, footer.shard_creation_timestamp);
    assert_eq!(parsed.shard_key_expiry, footer.shard_key_expiry);
    assert_eq!(parsed.stored_bytes_on_disk, footer.stored_bytes_on_disk);
    assert_eq!(parsed.materialized_bytes, footer.materialized_bytes);
    assert_eq!(parsed.stored_bytes, footer.stored_bytes);
    assert_eq!(parsed.footer_offset, footer.footer_offset);
}

#[test]
fn test_shard_header_magic_bytes() {
    let header = MDBShardFileHeader::default();
    // Magic bytes should be the specific 32-byte sequence
    assert_eq!(header.tag[0], 72); // 'H'
    assert_eq!(header.tag[1], 70); // 'F'
}

#[test]
fn test_file_data_sequence_header_roundtrip() {
    let header = FileDataSequenceHeader {
        file_hash: MerkleHash::from([0xAB; 32]),
        file_flags: 0,
        num_entries: 5,
    };

    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();

    assert_eq!(buf.len(), 48);

    let mut cursor = Cursor::new(&buf);
    let parsed = FileDataSequenceHeader::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.file_hash, header.file_hash);
    assert_eq!(parsed.file_flags, header.file_flags);
    assert_eq!(parsed.num_entries, header.num_entries);
}

#[test]
fn test_file_data_sequence_entry_roundtrip() {
    let entry = FileDataSequenceEntry {
        xorb_hash: MerkleHash::from([0xCD; 32]),
        xorb_flags: 0,
        unpacked_segment_bytes: 65536,
        chunk_index_start: 0,
        chunk_index_end: 5,
    };

    let mut buf = Vec::new();
    entry.serialize(&mut buf).unwrap();

    assert_eq!(buf.len(), 48);

    let mut cursor = Cursor::new(&buf);
    let parsed = FileDataSequenceEntry::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.xorb_hash, entry.xorb_hash);
    assert_eq!(parsed.unpacked_segment_bytes, entry.unpacked_segment_bytes);
    assert_eq!(parsed.chunk_index_start, entry.chunk_index_start);
    assert_eq!(parsed.chunk_index_end, entry.chunk_index_end);
}

#[test]
fn test_xorb_chunk_sequence_header_roundtrip() {
    let header = XorbChunkSequenceHeader {
        xorb_hash: MerkleHash::from([0xEF; 32]),
        xorb_flags: 0,
        num_entries: 10,
        num_bytes_in_xorb: 1024 * 1024,
        num_bytes_on_disk: 512 * 1024,
    };

    let mut buf = Vec::new();
    header.serialize(&mut buf).unwrap();

    let mut cursor = Cursor::new(&buf);
    let parsed = XorbChunkSequenceHeader::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.xorb_hash, header.xorb_hash);
    assert_eq!(parsed.num_entries, header.num_entries);
    assert_eq!(parsed.num_bytes_in_xorb, header.num_bytes_in_xorb);
    assert_eq!(parsed.num_bytes_on_disk, header.num_bytes_on_disk);
}

#[test]
fn test_xorb_chunk_sequence_entry_roundtrip() {
    let entry = XorbChunkSequenceEntry {
        chunk_hash: MerkleHash::from([0x11; 32]),
        chunk_byte_range_start: 0,
        unpacked_segment_bytes: 65536,
        flags: 0,
    };

    let mut buf = Vec::new();
    entry.serialize(&mut buf).unwrap();

    let mut cursor = Cursor::new(&buf);
    let parsed = XorbChunkSequenceEntry::deserialize(&mut cursor).unwrap();

    assert_eq!(parsed.chunk_hash, entry.chunk_hash);
    assert_eq!(parsed.chunk_byte_range_start, entry.chunk_byte_range_start);
    assert_eq!(parsed.unpacked_segment_bytes, entry.unpacked_segment_bytes);
    assert_eq!(parsed.flags, entry.flags);
}

#[test]
fn test_multiple_entries() {
    // Test serializing/deserializing multiple entries in sequence
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

    let mut buf = Vec::new();
    for entry in &entries {
        entry.serialize(&mut buf).unwrap();
    }

    let mut cursor = Cursor::new(&buf);
    for expected in &entries {
        let parsed = FileDataSequenceEntry::deserialize(&mut cursor).unwrap();
        assert_eq!(parsed.xorb_hash, expected.xorb_hash);
        assert_eq!(parsed.chunk_index_start, expected.chunk_index_start);
    }
}