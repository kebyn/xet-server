use xet_server::format::shard::{MDBShardFileHeader, MDBShardFileFooter};
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