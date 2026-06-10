//! Integration tests for Xet Storage server
//!
//! Tests the complete workflow:
//! 1. Upload xorbs
//! 2. Upload shards
//! 3. Query reconstructions
//! 4. Query global dedup

mod common;

use actix_web::{test, web, App};
use bytes::Bytes;
use tempfile::tempdir;

use common::{test_config_with_new_key, test_token_for_keypair, TestContext};
use xet_server::format::xorb::XorbObjectInfoV1;
use xet_server::index::MetadataIndex;
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

/// Helper to create a valid xorb with proper structure and hash
fn create_valid_xorb(content: &[u8]) -> (Vec<u8>, String) {
    let chunk_hash = xet_server::hash::compute_data_hash(content);

    let footer = XorbObjectInfoV1 {
        xorb_hash: chunk_hash,
        chunk_hashes: vec![chunk_hash],
        chunk_boundary_offsets: vec![content.len() as u32],
        unpacked_chunk_offsets: vec![content.len() as u32],
    };

    let footer_bytes = footer.to_bytes();
    let mut xorb_data = Vec::new();
    xorb_data.extend_from_slice(content);
    xorb_data.extend_from_slice(&footer_bytes);

    let xorb_hash = xet_server::hash::compute_data_hash(&xorb_data);
    (xorb_data, xorb_hash.to_hex())
}

#[actix_web::test]
async fn test_full_upload_workflow() {
    // Setup
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
            .route("/v1/shards", web::post().to(xet_server::api::shard::upload_shard))
            .route("/v2/reconstructions/{file_id}", web::get().to(xet_server::api::reconstruction::get_reconstruction))
            .route("/v1/chunks/{prefix}/{hash}", web::get().to(xet_server::api::global_dedup::query_chunk_dedup))
    ).await;

    // Step 1: Upload a xorb
    let (xorb_data, xorb_hash) = create_valid_xorb(b"test xorb data with some content");

    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data.clone()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 2: Try to upload the same xorb again (should be idempotent)
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data))
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

    // Step 4: Query reconstruction (should return 404 since no shard uploaded)
    let file_id = "b".repeat(64);
    let req = test::TestRequest::get()
        .uri(&format!("/v2/reconstructions/{}", file_id))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);

    // Step 5: Query global dedup for a chunk
    let chunk_hash = "c".repeat(64);
    let req = test::TestRequest::get()
        .uri(&format!("/v1/chunks/default/{}", chunk_hash))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 6: Query global dedup with invalid prefix
    let req = test::TestRequest::get()
        .uri(&format!("/v1/chunks/invalid/{}", chunk_hash))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn test_auth_workflow() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
    ).await;

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
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
            .route("/v2/reconstructions/{file_id}", web::get().to(xet_server::api::reconstruction::get_reconstruction))
            .route("/v1/chunks/{prefix}/{hash}", web::get().to(xet_server::api::global_dedup::query_chunk_dedup))
    ).await;

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
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    // Test 5: Invalid hash in chunk query
    let req = test::TestRequest::get()
        .uri("/v1/chunks/default/invalid")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn test_idempotency() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
    ).await;

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
