//! Tests for xorb verification

use xet_server::format::xorb::XorbObjectInfoV1;
use xet_server::types::MerkleHash;

#[test]
fn test_verify_xorb_valid() {
    // Create a simple valid xorb structure
    let chunk_data = b"test chunk data";
    let chunk_hash = xet_server::hash::compute_data_hash(chunk_data);

    let footer = XorbObjectInfoV1 {
        xorb_hash: chunk_hash,
        chunk_hashes: vec![chunk_hash],
        chunk_boundary_offsets: vec![chunk_data.len() as u32],
        unpacked_chunk_offsets: vec![chunk_data.len() as u32],
    };

    let footer_bytes = footer.to_bytes();

    // Create a complete xorb with data + footer
    let mut xorb_data = Vec::new();
    xorb_data.extend_from_slice(chunk_data);
    xorb_data.extend_from_slice(&footer_bytes);

    // Verify should succeed
    let result = xet_server::format::xorb::verify_xorb(&xorb_data);
    assert!(result.is_ok(), "Valid xorb should pass verification");
}

#[test]
fn test_verify_xorb_invalid_hash() {
    // Create a xorb with zero hash (should fail)
    let footer = XorbObjectInfoV1 {
        xorb_hash: MerkleHash::from([0u8; 32]),
        chunk_hashes: vec![],
        chunk_boundary_offsets: vec![],
        unpacked_chunk_offsets: vec![],
    };

    let footer_bytes = footer.to_bytes();

    // Create xorb with just footer
    let xorb_data = footer_bytes;

    // Verify should fail
    let result = xet_server::format::xorb::verify_xorb(&xorb_data);
    assert!(result.is_err(), "Xorb with zero hash should fail verification");
}
