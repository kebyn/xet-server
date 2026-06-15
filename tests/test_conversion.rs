//! Integration tests for the conversion pipeline.
//!
//! Covers: small file conversion, byte-identical reconstruction, size guard
//! errors, disabled config, index rebuild, ConvertingOids lock semantics, and
//! dedup counting across two files that share a common prefix.

use std::sync::Arc;

use bytes::Bytes;
use rand::{Rng, SeedableRng};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use xet_server::config::ConversionConfig;
use xet_server::conversion::{ConvertingOids, ConversionError, ConversionPipeline};
use xet_server::format::compression;
use xet_server::format::shard::MDBShardFile;
use xet_server::format::xorb::{XorbChunkHeader, XorbObjectInfoV1};
use xet_server::index::MetadataIndex;
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a test environment: storage backed by `tempdir`, an empty index, and
/// the default conversion config. The `TempDir` is returned so the caller can
/// keep it alive for the duration of the test.
fn setup_test_env(
) -> (Arc<Box<dyn StorageBackend>>, Arc<MetadataIndex>, ConversionConfig, TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(tempdir.path().to_str().unwrap()).unwrap());
    let storage = Arc::new(storage);
    let index = Arc::new(MetadataIndex::new());
    // Override min_conversion_size for tests — test files are smaller than the
    // production default (64KB). Tests here verify conversion mechanics, not thresholds.
    let config = ConversionConfig {
        min_conversion_size: 0,
        ..Default::default()
    };
    (storage, index, config, tempdir)
}

/// SHA-256 hex digest of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Build deterministic test data of `size` bytes from a seeded RNG.
fn make_test_data(seed: u64, size: usize) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);
    data
}

/// Upload `data` as a raw LFS blob and return its SHA-256 OID.
async fn upload_raw_blob(
    storage: &Arc<Box<dyn StorageBackend>>,
    data: &[u8],
) -> String {
    let oid = sha256_hex(data);
    let key = format!("lfs/objects/{}", oid);
    storage.put(&key, Bytes::from(data.to_vec())).await.unwrap();
    oid
}

/// Assert that `result` is an `Err` matching the given pattern.
/// We cannot use `unwrap_err()` because `ConversionResult` does not impl `Debug`.
macro_rules! assert_conversion_err {
    ($result:expr, $pattern:pat => $body:block) => {
        match $result {
            Ok(_) => panic!("expected conversion error, got Ok"),
            Err(e) => {
                match e {
                    $pattern => $body,
                    other => panic!(
                        "unexpected ConversionError variant: {}",
                        other
                    ),
                }
            }
        }
    };
}

/// Reconstruct the original file bytes from the xorb + shard chain.
///
/// This mirrors the server-side reconstruction logic: look up the shard(s) for
/// `file_id`, parse each shard, then for every xorb referenced by the shard's
/// file segments, fetch and parse the xorb, decompress each chunk, and
/// concatenate the result.
async fn reconstruct_from_xet(
    file_id: &str,
    index: &MetadataIndex,
    storage: &Arc<Box<dyn StorageBackend>>,
) -> Vec<u8> {
    let shard_ids = index
        .get_shards_for_file(file_id)
        .expect("file_id should have shard mappings");

    let mut output = Vec::new();

    for shard_id in &shard_ids {
        let shard_key = format!("shards/{}", shard_id);
        let shard_data = storage.get(&shard_key).await.unwrap();
        let shard = MDBShardFile::parse(&shard_data).unwrap();

        // Walk file entries and their segments to reassemble in order.
        // For each segment we need to pull chunks out of the referenced xorb.
        let mut file_data_idx = 0usize;
        for file_header in &shard.file_entries {
            if file_header.file_hash.to_hex() != file_id {
                // Skip entries for other files in this shard.
                file_data_idx += file_header.num_entries as usize;
                continue;
            }
            for _ in 0..file_header.num_entries {
                let seg = &shard.file_data_entries[file_data_idx];
                file_data_idx += 1;

                // Find the corresponding xorb entry.
                let xorb_entry_pos = shard
                    .xorb_entries
                    .iter()
                    .position(|x| x.xorb_hash == seg.xorb_hash)
                    .expect("xorb referenced by segment must exist in shard");

                // Compute the starting position in xorb_chunk_entries for this xorb.
                let chunk_offset: usize = shard.xorb_entries[..xorb_entry_pos]
                    .iter()
                    .map(|x| x.num_entries as usize)
                    .sum();

                // Fetch and parse the xorb.
                let xorb_key = format!("xorbs/{}", seg.xorb_hash.to_hex());
                let xorb_data = storage.get(&xorb_key).await.unwrap();

                // Parse xorb footer to locate chunk boundaries.
                let xorb_footer = parse_xorb_footer(&xorb_data);

                for chunk_idx in seg.chunk_index_start..seg.chunk_index_end {
                    let _ci = chunk_offset + chunk_idx as usize;

                    // Determine the byte range of this chunk inside the xorb.
                    let chunk_start: usize = if chunk_idx == 0 {
                        0
                    } else {
                        xorb_footer.chunk_boundary_offsets[chunk_idx as usize - 1] as usize
                    };
                    let chunk_end: usize =
                        xorb_footer.chunk_boundary_offsets[chunk_idx as usize] as usize;

                    let chunk_bytes = &xorb_data[chunk_start..chunk_end];

                    // Parse the 8-byte chunk header to discover compression
                    // scheme, compressed length, and uncompressed length.
                    let header = parse_chunk_header(chunk_bytes);
                    let compressed = &chunk_bytes[XorbChunkHeader::SIZE
                        ..XorbChunkHeader::SIZE + header.compressed_length as usize];

                    let decompressed = compression::decompress(
                        header.compression_scheme,
                        compressed,
                        header.uncompressed_length as usize,
                    )
                    .unwrap();

                    output.extend_from_slice(&decompressed);
                }
            }
            // Only one file entry per shard in our pipeline; break after first match.
            break;
        }
    }

    output
}

/// Parse an `XorbChunkHeader` from the beginning of `chunk_bytes`.
fn parse_chunk_header(chunk_bytes: &[u8]) -> XorbChunkHeader {
    let mut cursor = std::io::Cursor::new(chunk_bytes);
    XorbChunkHeader::deserialize(&mut cursor).unwrap()
}

/// Parse the xorb footer (XorbObjectInfoV1) from the tail of xorb bytes.
fn parse_xorb_footer(xorb_data: &[u8]) -> XorbObjectInfoV1 {
    // Find IDENT_HASHES at the start of the footer.
    let ident = XorbObjectInfoV1::IDENT_HASHES;
    let pos = xorb_data
        .windows(7)
        .rposition(|w| w == ident)
        .expect("xorb footer IDENT_HASHES not found");
    XorbObjectInfoV1::from_bytes(&xorb_data[pos..]).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_convert_small_file() {
    let (storage, index, config, _tempdir) = setup_test_env();

    // ~10 KB of random data — well above min_conversion_size (1 KB).
    let data = make_test_data(42, 10 * 1024);
    let original_size = data.len() as u64;
    let oid = upload_raw_blob(&storage, &data).await;

    let pipeline = ConversionPipeline::new(storage.clone(), index.clone(), config);
    let result = pipeline.convert(&oid).await.unwrap();

    // -- Assertions --------------------------------------------------------
    assert!(result.num_chunks > 0, "should produce at least one chunk");
    assert_eq!(
        result.num_deduped_chunks, 0,
        "first conversion has nothing to dedup"
    );
    assert_eq!(result.raw_size, original_size);

    // Xorb and shard exist in storage.
    let xorb_exists = storage.exists(&format!("xorbs/{}", result.xorb_hash)).await.unwrap();
    assert!(xorb_exists, "xorb should exist in storage");

    let shard_exists = storage.exists(&format!("shards/{}", result.shard_hash)).await.unwrap();
    assert!(shard_exists, "shard should exist in storage");

    // Raw blob was deleted (default config: delete_raw_after_conversion = true).
    let raw_exists = storage.exists(&format!("lfs/objects/{}", oid)).await.unwrap();
    assert!(!raw_exists, "raw blob should be deleted after conversion");

    // Index was updated.
    let shards = index.get_shards_for_file(&oid);
    assert!(shards.is_some(), "index should have file→shard mapping");
    let shards = shards.unwrap();
    assert!(
        shards.contains(&result.shard_hash),
        "shard list should contain the newly created shard"
    );
}

#[tokio::test]
async fn test_convert_preserves_data() {
    let (storage, index, config, _tempdir) = setup_test_env();

    // 32 KB of deterministic random data.
    let data = make_test_data(7, 32 * 1024);
    let oid = upload_raw_blob(&storage, &data).await;

    let pipeline = ConversionPipeline::new(storage.clone(), index.clone(), config);
    let _result = pipeline.convert(&oid).await.unwrap();

    // Reconstruct and compare byte-for-byte.
    let reconstructed = reconstruct_from_xet(&oid, &index, &storage).await;
    assert_eq!(
        reconstructed.len(),
        data.len(),
        "reconstructed length must match original"
    );
    assert_eq!(
        reconstructed, data,
        "reconstructed bytes must be identical to original"
    );
}

#[tokio::test]
async fn test_convert_too_small() {
    let (storage, index, mut config, _tempdir) = setup_test_env();

    // Set min_conversion_size above the test file size to trigger TooSmall.
    config.min_conversion_size = 1024;

    // 500 bytes — below min_conversion_size of 1024.
    let data = vec![0xABu8; 500];
    let oid = upload_raw_blob(&storage, &data).await;

    let pipeline = ConversionPipeline::new(storage.clone(), index.clone(), config);
    let result = pipeline.convert(&oid).await;

    assert_conversion_err!(result, ConversionError::TooSmall(size) => {
        assert_eq!(size, 500, "reported size should match the blob size");
    });
}

#[tokio::test]
async fn test_convert_too_large() {
    let (storage, index, mut config, _tempdir) = setup_test_env();

    // Lower min so that the TooLarge check triggers before TooSmall.
    config.min_conversion_size = 0;
    // Set max_conversion_size very low.
    config.max_conversion_size = 100;

    let data = vec![0xCDu8; 200];
    let oid = upload_raw_blob(&storage, &data).await;

    let pipeline = ConversionPipeline::new(storage.clone(), index.clone(), config);
    let result = pipeline.convert(&oid).await;

    assert_conversion_err!(result, ConversionError::TooLarge(size) => {
        assert_eq!(size, 200, "reported size should match the blob size");
    });
}

#[tokio::test]
async fn test_convert_disabled() {
    let (storage, index, mut config, _tempdir) = setup_test_env();
    config.enabled = false;

    let data = make_test_data(99, 5 * 1024);
    let oid = upload_raw_blob(&storage, &data).await;

    let pipeline = ConversionPipeline::new(storage.clone(), index.clone(), config);
    let result = pipeline.convert(&oid).await;

    assert_conversion_err!(result, ConversionError::Disabled => {});
}

#[tokio::test]
async fn test_rebuild_from_storage() {
    let (storage, index, config, _tempdir) = setup_test_env();

    let data = make_test_data(123, 16 * 1024);
    let oid = upload_raw_blob(&storage, &data).await;

    // Convert — this writes xorbs + shards and updates `index`.
    let pipeline = ConversionPipeline::new(storage.clone(), index.clone(), config);
    let result = pipeline.convert(&oid).await.unwrap();

    // Create a fresh, empty index and rebuild it from storage.
    let fresh_index = MetadataIndex::new();
    let count = fresh_index.rebuild_from_storage(storage.clone()).await.unwrap();
    assert!(count >= 1, "should have rebuilt at least 1 shard");

    // The rebuilt index should know about the file.
    let shards = fresh_index.get_shards_for_file(&oid);
    assert!(
        shards.is_some(),
        "rebuilt index should have mapping for the converted file"
    );
    let shards = shards.unwrap();
    assert!(
        shards.contains(&result.shard_hash),
        "rebuilt index shard list should match the original"
    );
}

#[tokio::test]
async fn test_converting_oids_guard() {
    let guard = ConvertingOids::new();

    // First acquire succeeds.
    assert!(guard.try_acquire("abc"));

    // Second acquire of the same OID fails.
    assert!(!guard.try_acquire("abc"));

    // Release, then re-acquire succeeds.
    guard.release("abc");
    assert!(guard.try_acquire("abc"));

    // A different OID can be acquired concurrently.
    assert!(guard.try_acquire("def"));

    // Clean up.
    guard.release("abc");
    guard.release("def");
}

#[tokio::test]
async fn test_convert_dedup_counting() {
    let (storage, index, config, _tempdir) = setup_test_env();

    // The default CDC config has min=8KB, target=64KB, max=128KB.
    // Use a 512KB shared prefix so that several chunk boundaries fall entirely
    // within the shared region, guaranteeing dedup between the two conversions.
    let shared_prefix = make_test_data(1000, 512 * 1024);

    // File 1: shared prefix + unique suffix A.
    let mut file1_data = shared_prefix.clone();
    let suffix_a = make_test_data(2001, 128 * 1024);
    file1_data.extend_from_slice(&suffix_a);

    // File 2: shared prefix + unique suffix B.
    let mut file2_data = shared_prefix.clone();
    let suffix_b = make_test_data(3001, 128 * 1024);
    file2_data.extend_from_slice(&suffix_b);

    let oid1 = upload_raw_blob(&storage, &file1_data).await;
    let oid2 = upload_raw_blob(&storage, &file2_data).await;

    let pipeline = ConversionPipeline::new(storage.clone(), index.clone(), config);

    // Convert file 1 first — nothing to dedup.
    let r1 = pipeline.convert(&oid1).await.unwrap();
    assert_eq!(
        r1.num_deduped_chunks, 0,
        "first conversion should have zero deduped chunks"
    );

    // Convert file 2 — shared chunks should be detected.
    let r2 = pipeline.convert(&oid2).await.unwrap();
    assert!(
        r2.num_deduped_chunks > 0,
        "second conversion should dedup at least the shared prefix chunks, got {}",
        r2.num_deduped_chunks
    );
}
