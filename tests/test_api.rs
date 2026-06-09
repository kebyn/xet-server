//! Integration tests for API endpoints

use actix_web::{test, web, App};
use bytes::Bytes;
use tempfile::tempdir;

use xet_server::api::auth::{create_jwt, JwtClaims};
use xet_server::config::ServerConfig;
use xet_server::format::xorb::XorbObjectInfoV1;
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
async fn test_upload_xorb() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let config = ServerConfig::default();
    let token = create_jwt(
        &JwtClaims {
            sub: "test".to_string(),
            scope: "read write".to_string(),
            exp: 9999999999,
        },
        &config.auth.jwt_secret,
    ).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
    ).await;

    let (xorb_data, hash) = create_valid_xorb(b"test xorb data");
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["was_inserted"], true);
}

#[actix_web::test]
async fn test_upload_xorb_duplicate() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let config = ServerConfig::default();
    let token = create_jwt(
        &JwtClaims {
            sub: "test".to_string(),
            scope: "read write".to_string(),
            exp: 9999999999,
        },
        &config.auth.jwt_secret,
    ).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
    ).await;

    let (xorb_data, hash) = create_valid_xorb(b"test xorb data for duplicate");

    // First upload
    let req1 = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data.clone()))
        .to_request();

    let resp1 = test::call_service(&app, req1).await;
    assert!(resp1.status().is_success());

    // Second upload (duplicate)
    let req2 = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(xorb_data))
        .to_request();

    let resp2 = test::call_service(&app, req2).await;
    assert!(resp2.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp2).await;
    assert_eq!(body["was_inserted"], false);
}

#[actix_web::test]
async fn test_upload_xorb_invalid_hash() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let config = ServerConfig::default();
    let token = create_jwt(
        &JwtClaims {
            sub: "test".to_string(),
            scope: "read write".to_string(),
            exp: 9999999999,
        },
        &config.auth.jwt_secret,
    ).unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
    ).await;

    // Invalid hash (not 64 chars)
    let req = test::TestRequest::post()
        .uri("/v1/xorbs/default/invalid_hash")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from("test xorb data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn test_upload_xorb_no_auth() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let config = ServerConfig::default();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(xet_server::api::xorb::upload_xorb))
    ).await;

    let hash = "c".repeat(64);
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .set_payload(Bytes::from("test xorb data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}
