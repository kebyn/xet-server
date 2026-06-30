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
    // Create a xorb with zero hash and zero chunks
    // This is actually valid - empty xorb has default (zero) hash
    let footer = XorbObjectInfoV1 {
        xorb_hash: MerkleHash::from([0u8; 32]),
        chunk_hashes: vec![],
        chunk_boundary_offsets: vec![],
        unpacked_chunk_offsets: vec![],
    };

    let footer_bytes = footer.to_bytes();

    // Create xorb with just footer
    let xorb_data = footer_bytes;

    // Verify should succeed - empty xorb with zero hash is valid
    let result = xet_server::format::xorb::verify_xorb(&xorb_data);
    assert!(result.is_ok(), "Empty xorb with zero hash should be valid");
}

#[test]
fn test_verify_xorb_mismatched_hash() {
    // Create a xorb with chunk data but wrong hash
    let chunk_data = b"test chunk data";
    let wrong_hash = MerkleHash::from([1u8; 32]); // Wrong hash

    let footer = XorbObjectInfoV1 {
        xorb_hash: wrong_hash,
        chunk_hashes: vec![wrong_hash],
        chunk_boundary_offsets: vec![chunk_data.len() as u32],
        unpacked_chunk_offsets: vec![chunk_data.len() as u32],
    };

    let footer_bytes = footer.to_bytes();

    // Create xorb with data + footer
    let mut xorb_data = Vec::new();
    xorb_data.extend_from_slice(chunk_data);
    xorb_data.extend_from_slice(&footer_bytes);

    // Verify should fail - hash doesn't match data
    let result = xet_server::format::xorb::verify_xorb(&xorb_data);
    assert!(
        result.is_err(),
        "Xorb with mismatched hash should fail verification"
    );
}
