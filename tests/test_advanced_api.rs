//! Integration tests for Xet Storage server
//!
//! Tests the complete workflow:
//! 1. Upload xorbs
//! 2. Upload shards
//! 3. Query reconstructions
//! 4. Query global dedup

mod common;

use actix_web::{App, test, web};
use bytes::Bytes;
use tempfile::tempdir;

use common::{TestContext, test_config_with_new_key, test_token_for_keypair};
use xet_server::format::compression::CompressionScheme;
use xet_server::format::shard_builder::{FileSegment, ShardBuilder, XorbChunkBuildEntry};
use xet_server::format::xorb_builder::XorbBuilder;
use xet_server::hash::compute_data_hash;
use xet_server::index::MetadataIndex;
use xet_server::storage::StorageBackend;
use xet_server::storage::local::LocalStorage;
use xet_server::types::MerkleHash;

/// Helper to create a valid xorb with proper structure and hash
fn create_valid_xorb(content: &[u8]) -> (Vec<u8>, String) {
    let mut builder = XorbBuilder::new(CompressionScheme::None);
    builder.add_chunk(content).unwrap();
    let xorb = builder.build().unwrap();
    (xorb.data, xorb.xorb_hash.to_hex())
}

fn sha256_merkle_hash(data: &[u8]) -> MerkleHash {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(data);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    MerkleHash::from(bytes)
}

#[actix_web::test]
async fn test_full_upload_workflow() {
    // Setup
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
    let storage_arc: std::sync::Arc<Box<dyn StorageBackend>> = std::sync::Arc::new(storage);

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::from(storage_arc))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/v1/xorbs/{prefix}/{hash}",
                web::post().to(xet_server::api::xorb::upload_xorb),
            )
            .route(
                "/v1/shards",
                web::post().to(xet_server::api::shard::upload_shard),
            )
            .route(
                "/v2/reconstructions/{file_id}",
                web::get().to(xet_server::api::reconstruction::get_reconstruction),
            )
            .route(
                "/v1/chunks/{prefix}/{hash}",
                web::get().to(xet_server::api::global_dedup::query_chunk_dedup),
            ),
    )
    .await;

    // Step 1: Upload a xorb through the public API.
    let raw_chunk = b"test xorb data with some content";
    let mut xorb_builder = XorbBuilder::new(CompressionScheme::None);
    let (serialized_chunk_hash, _compressed_len) = xorb_builder.add_chunk(raw_chunk).unwrap();
    let xorb = xorb_builder.build().unwrap();
    let xorb_hash = xorb.xorb_hash.to_hex();
    let raw_chunk_hash = compute_data_hash(raw_chunk);

    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb.data.clone()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 2: Try to upload the same xorb again (should be idempotent)
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb.data.clone()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 3: Try to upload with invalid prefix
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/invalid/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    // Step 4: Upload a shard that references the public xorb.
    let mut shard_builder = ShardBuilder::new();
    let xorb_index = shard_builder
        .add_xorb_with_raw_chunk_hashes(
            xorb.xorb_hash,
            xorb.total_uncompressed_size as u32,
            xorb.total_compressed_size as u32,
            vec![XorbChunkBuildEntry {
                chunk_hash: serialized_chunk_hash,
                chunk_byte_range_start: 0,
                unpacked_segment_bytes: raw_chunk.len() as u32,
            }],
            vec![raw_chunk_hash],
        )
        .unwrap();
    shard_builder.add_file(
        sha256_merkle_hash(raw_chunk),
        vec![FileSegment {
            xorb_hash: xorb.xorb_hash,
            xorb_index,
            chunk_index_start: 0,
            chunk_index_end: 1,
            unpacked_segment_bytes: raw_chunk.len() as u32,
        }],
    );
    let shard_data = shard_builder.build().unwrap();
    let req = test::TestRequest::post()
        .uri("/v1/shards")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(shard_data))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 5: Query global dedup for the raw chunk hash registered by the shard.
    let req = test::TestRequest::get()
        .uri(&format!("/v1/chunks/default/{}", raw_chunk_hash.to_hex()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["found"], true);
    assert_eq!(body["xorb_hash"], xorb_hash);
    assert_eq!(body["chunk_index"], 0);

    // Step 6: Query global dedup with invalid prefix
    let req = test::TestRequest::get()
        .uri(&format!("/v1/chunks/invalid/{}", raw_chunk_hash.to_hex()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn test_auth_workflow() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/v1/xorbs/{prefix}/{hash}",
                web::post().to(xet_server::api::xorb::upload_xorb),
            ),
    )
    .await;

    let xorb_hash = "a".repeat(64);

    // Test 1: No auth token
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);

    // Test 2: Invalid token format
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", "InvalidFormat"))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);

    // Test 3: Invalid token (not signed by our key)
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", "Bearer xet_invalid.token.here"))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);

    // Test 4: Valid token but insufficient scope
    let token = test_token_for_keypair(&ctx.keypair, "read"); // Only read scope

    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn test_hash_validation() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/v1/xorbs/{prefix}/{hash}",
                web::post().to(xet_server::api::xorb::upload_xorb),
            )
            .route(
                "/v2/reconstructions/{file_id}",
                web::get().to(xet_server::api::reconstruction::get_reconstruction),
            )
            .route(
                "/v1/chunks/{prefix}/{hash}",
                web::get().to(xet_server::api::global_dedup::query_chunk_dedup),
            ),
    )
    .await;

    // Test 1: Invalid hash length (too short)
    let req = test::TestRequest::post()
        .uri("/v1/xorbs/default/abc123")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    // Test 2: Invalid hash characters
    let invalid_hash = "g".repeat(64); // 'g' is not a valid hex character
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", invalid_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    // Test 3: Valid hash format (with valid xorb data)
    let (xorb_data, valid_hash) = create_valid_xorb(b"test data");
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", valid_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Test 4: Invalid file_id in reconstruction
    let req = test::TestRequest::get()
        .uri("/v2/reconstructions/short")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    // Test 5: Invalid hash in chunk query
    let req = test::TestRequest::get()
        .uri("/v1/chunks/default/invalid")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn test_idempotency() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/v1/xorbs/{prefix}/{hash}",
                web::post().to(xet_server::api::xorb::upload_xorb),
            ),
    )
    .await;

    let (xorb_data, xorb_hash) = create_valid_xorb(b"test xorb data");

    // Upload first time
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data.clone()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["was_inserted"], true);

    // Upload second time with same data
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["was_inserted"], false);

    // Upload third time with different data (should fail hash validation)
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("different data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400); // Hash mismatch
}
