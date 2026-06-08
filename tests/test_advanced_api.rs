//! Integration tests for Xet Storage server
//!
//! Tests the complete workflow:
//! 1. Upload xorbs
//! 2. Upload shards
//! 3. Query reconstructions
//! 4. Query global dedup

use actix_web::{test, web, App};
use bytes::Bytes;
use tempfile::tempdir;

use xet_server::api::auth::{create_jwt, JwtClaims};
use xet_server::config::{ServerConfig, StorageConfig, AuthConfig, ServerSettings};
use xet_server::index::MetadataIndex;
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

fn create_test_config() -> ServerConfig {
    ServerConfig {
        storage: StorageConfig {
            backend: "local".to_string(),
            s3_bucket: None,
            s3_region: None,
            s3_endpoint: None,
            local_path: None,
        },
        auth: AuthConfig {
            jwt_secret: "test-secret-key".to_string(),
        },
        server: ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 8080,
        },
    }
}

#[actix_web::test]
async fn test_full_upload_workflow() {
    // Setup
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let config = create_test_config();

    let token = create_jwt(
        &JwtClaims {
            sub: "test-user".to_string(),
            scope: "read write".to_string(),
            exp: 9999999999,
        },
        &config.auth.jwt_secret,
    ).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::server::upload_xorb))
            .route("/v1/shards", web::post().to(xet_server::api::shard::upload_shard))
            .route("/v2/reconstructions/{file_id}", web::get().to(xet_server::api::reconstruction::get_reconstruction))
            .route("/v1/chunks/{prefix}/{hash}", web::get().to(xet_server::api::global_dedup::query_chunk_dedup))
    ).await;

    // Step 1: Upload a xorb
    let xorb_hash = "a".repeat(64);
    let xorb_data = Bytes::from("test xorb data with some content");

    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(xorb_data)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Step 2: Try to upload the same xorb again (should be idempotent)
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test xorb data with some content"))
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
    let config = create_test_config();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::server::upload_xorb))
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

    // Test 3: Invalid token signature
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", "Bearer invalid.token.here"))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);

    // Test 4: Valid token but insufficient scope
    let token = create_jwt(
        &JwtClaims {
            sub: "test-user".to_string(),
            scope: "read".to_string(),  // Only read scope
            exp: 9999999999,
        },
        &create_test_config().auth.jwt_secret,
    ).unwrap();

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
    let config = create_test_config();

    let token = create_jwt(
        &JwtClaims {
            sub: "test-user".to_string(),
            scope: "read write".to_string(),
            exp: 9999999999,
        },
        &config.auth.jwt_secret,
    ).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::server::upload_xorb))
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
    let invalid_hash = "g".repeat(64);  // 'g' is not a valid hex character
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", invalid_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    // Test 3: Valid hash format
    let valid_hash = "a".repeat(64);
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", valid_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test data"))
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
    let config = create_test_config();

    let token = create_jwt(
        &JwtClaims {
            sub: "test-user".to_string(),
            scope: "read write".to_string(),
            exp: 9999999999,
        },
        &config.auth.jwt_secret,
    ).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::server::upload_xorb))
    ).await;

    let xorb_hash = "a".repeat(64);
    let xorb_data = Bytes::from("test xorb data");

    // Upload first time
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(xorb_data.clone())
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["was_inserted"], true);

    // Upload second time with same data
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(xorb_data.clone())
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["was_inserted"], false);

    // Upload third time with different data (should still succeed but not insert)
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", xorb_hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("different data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["was_inserted"], false);
}
