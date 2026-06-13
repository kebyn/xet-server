//! Integration tests for streaming upload handlers.
//!
//! Verifies that uploads work via actix-web's streaming Payload extractor,
//! and that content integrity checks reject tampered data.

mod common;

use actix_web::{test, web, App};
use bytes::Bytes;
use tempfile::tempdir;

use common::{test_token_for_keypair, TestContext};
use xet_server::api::auth::{AuthVerifier, KeyPair};
use xet_server::format::xorb::XorbObjectInfoV1;
use xet_server::hash::compute_data_hash;
use xet_server::index::MetadataIndex;
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

fn create_test_config_with_temp_dir(temp_dir: &str) -> TestContext {
    // Generate a key pair first
    let kp = KeyPair::generate();

    // Write public key to a temp file inside a tempdir
    let key_temp_dir = tempfile::tempdir().unwrap();
    let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();
    let pub_key_path = key_temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
    std::fs::write(&pub_key_path, &public_key_pem).unwrap();

    let auth_config = xet_server::config::AuthConfig {
        public_key_path: pub_key_path.to_str().unwrap().to_string(),
        trusted_kids: vec![kp.kid()],
    };

    let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();

    let config = xet_server::config::ServerConfig {
        server: xet_server::config::ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 8080,
            public_base_url: None,
            max_body_size_mb: 2048,
        },
        storage: xet_server::config::StorageConfig {
            backend: "local".to_string(),
            s3_bucket: None,
            s3_region: None,
            s3_endpoint: None,
            local_path: Some("./data".to_string()),
            upload_temp_dir: Some(temp_dir.to_string()),
            verify_download_integrity: false,
        },
        auth: auth_config,
        conversion: xet_server::config::ConversionConfig::default(),
        gc: xet_server::config::GcConfig::default(),
        index: xet_server::config::IndexConfig::default(),
    };

    TestContext {
        config,
        keypair: kp,
        auth_verifier,
        temp_dir: key_temp_dir,
    }
}

fn create_test_config_small_limit(temp_dir: &str) -> TestContext {
    let ctx = create_test_config_with_temp_dir(temp_dir);
    let config = xet_server::config::ServerConfig {
        server: xet_server::config::ServerSettings {
            max_body_size_mb: 1, // 1 MB limit for testing 413
            ..ctx.config.server
        },
        ..ctx.config
    };
    TestContext {
        config,
        keypair: ctx.keypair,
        auth_verifier: ctx.auth_verifier,
        temp_dir: ctx.temp_dir,
    }
}

/// Helper to create a valid xorb with proper structure and hash
fn create_valid_xorb(content: &[u8]) -> (Vec<u8>, String) {
    let chunk_hash = compute_data_hash(content);

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

    let xorb_hash = compute_data_hash(&xorb_data);
    (xorb_data, xorb_hash.to_hex())
}

#[actix_web::test]
async fn test_streaming_lfs_upload() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_with_temp_dir(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let content = b"hello streaming world";
    let oid = compute_data_hash(content).to_hex();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/lfs/objects/{oid}",
                web::put().to(xet_server::api::lfs::upload_lfs_object),
            ),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(content.to_vec()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "LFS upload should succeed");
}

/// Test that Git LFS clients can upload using SHA-256 OIDs.
/// The server computes both BLAKE3 and SHA-256 during streaming upload
/// and accepts the upload if either matches the OID.
#[actix_web::test]
async fn test_streaming_lfs_upload_sha256_oid() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_with_temp_dir(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let content = b"hello git lfs sha256 world";

    // Compute SHA-256 hash (what Git LFS clients send as OID)
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(content);
    let sha256_oid = format!("{:x}", hasher.finalize());

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/lfs/objects/{oid}",
                web::put().to(xet_server::api::lfs::upload_lfs_object),
            ),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", sha256_oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(content.to_vec()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(), 200,
        "LFS upload with SHA-256 OID should succeed (Git LFS client compatibility)"
    );
}

#[actix_web::test]
async fn test_streaming_lfs_hash_mismatch() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_with_temp_dir(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    // Use a random valid-looking oid that doesn't match the content
    let wrong_oid = "a".repeat(64);

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/lfs/objects/{oid}",
                web::put().to(xet_server::api::lfs::upload_lfs_object),
            ),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", wrong_oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(b"some content that doesn't match the oid".to_vec()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400, "Hash mismatch should return 400");

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap().contains("Hash mismatch"),
        "Error should mention hash mismatch: {}",
        body["error"]
    );
    // Verify error message contains both computed hashes (BLAKE3 and SHA-256)
    let error_msg = body["error"].as_str().unwrap();
    assert!(
        error_msg.contains("BLAKE3") && error_msg.contains("SHA-256"),
        "Error should mention both hash algorithms: {}",
        error_msg
    );
}

#[actix_web::test]
async fn test_streaming_lfs_oversized_rejected() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_small_limit(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    // Create content larger than 1MB
    let large_content = vec![0u8; 2 * 1024 * 1024]; // 2 MB
    let oid = compute_data_hash(&large_content).to_hex();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/lfs/objects/{oid}",
                web::put().to(xet_server::api::lfs::upload_lfs_object),
            ),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(large_content))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        413,
        "Oversized upload should return 413 Payload Too Large"
    );
}

#[actix_web::test]
async fn test_streaming_xorb_upload() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_with_temp_dir(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let content = b"xorb streaming test data";
    let (xorb_data, xorb_hash) = create_valid_xorb(content);

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/v1/xorbs/{prefix}/{hash}",
                web::post().to(xet_server::api::xorb::upload_xorb),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "Xorb upload should succeed: {:?}", resp);
}

#[actix_web::test]
async fn test_streaming_xorb_invalid_structure() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_with_temp_dir(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    // Create data whose overall hash matches but has no valid footer
    let bogus_data = b"this is not a valid xorb at all, just some bytes";
    let hash = compute_data_hash(bogus_data).to_hex();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/v1/xorbs/{prefix}/{hash}",
                web::post().to(xet_server::api::xorb::upload_xorb),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(bogus_data.to_vec()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        400,
        "Invalid xorb structure should return 400"
    );
}

#[actix_web::test]
async fn test_streaming_shard_upload() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_with_temp_dir(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let index = MetadataIndex::new();

    // Create a valid shard by serializing header + padding + footer
    use xet_server::format::shard::{MDBShardFileFooter, MDBShardFileHeader};
    let header = MDBShardFileHeader::default();
    let footer = MDBShardFileFooter {
        version: 1,
        file_info_offset: 48,
        xorb_info_offset: 1000,
        file_lookup_offset: 2000,
        file_lookup_num_entry: 0,
        xorb_lookup_offset: 2100,
        xorb_lookup_num_entry: 0,
        chunk_lookup_offset: 2200,
        chunk_lookup_num_entry: 0,
        chunk_hash_hmac_key: [0u8; 32],
        shard_creation_timestamp: 1700000000,
        shard_key_expiry: u64::MAX,
        stored_bytes_on_disk: 0,
        materialized_bytes: 0,
        stored_bytes: 0,
        footer_offset: 0, // will be set after we know the size
    };

    let mut shard_data = Vec::new();
    header.serialize(&mut shard_data).unwrap();
    // Pad between header and footer
    let padding_size = 208; // ensure enough space
    shard_data.resize(shard_data.len() + padding_size, 0);
    // Set footer_offset to actual position
    let footer_offset = shard_data.len();
    let footer = MDBShardFileFooter {
        footer_offset: footer_offset as u64,
        ..footer
    };
    footer.serialize(&mut shard_data).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/v1/shards",
                web::post().to(xet_server::api::shard::upload_shard),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/v1/shards")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(shard_data))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "Shard upload should succeed: {:?}", resp);
}

#[actix_web::test]
async fn test_streaming_lfs_idempotent() {
    let storage_dir = tempdir().unwrap();
    let temp_dir = tempdir().unwrap();

    let ctx = create_test_config_with_temp_dir(temp_dir.path().to_str().unwrap());
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let content = b"idempotent upload test";
    let oid = compute_data_hash(content).to_hex();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier))
            .app_data(web::Data::new(ctx.config))
            .route(
                "/lfs/objects/{oid}",
                web::put().to(xet_server::api::lfs::upload_lfs_object),
            ),
    )
    .await;

    // First upload
    let req1 = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(content.to_vec()))
        .to_request();
    let resp1 = test::call_service(&app, req1).await;
    assert_eq!(resp1.status(), 200, "First upload should succeed");

    // Second upload of same content
    let req2 = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(content.to_vec()))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), 200, "Duplicate upload should return 200 (idempotent)");

    let body: serde_json::Value = test::read_body_json(resp2).await;
    assert_eq!(body["message"], "Object already exists");
}
