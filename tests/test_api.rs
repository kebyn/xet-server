//! Integration tests for API endpoints

mod common;

use actix_web::{App, test, web};
use bytes::Bytes;
use tempfile::tempdir;

use common::{TestContext, test_config_with_new_key, test_token_for_keypair};
use xet_server::format::compression::CompressionScheme;
use xet_server::format::xorb_builder::XorbBuilder;
use xet_server::storage::StorageBackend;
use xet_server::storage::local::LocalStorage;

/// Helper to create a valid xorb with proper structure and hash
fn create_valid_xorb(content: &[u8]) -> (Vec<u8>, String) {
    let mut builder = XorbBuilder::new(CompressionScheme::None);
    builder.add_chunk(content).unwrap();
    let xorb = builder.build().unwrap();
    (xorb.data, xorb.xorb_hash.to_hex())
}

#[actix_web::test]
async fn test_upload_xorb() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

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
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

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
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

    let ctx: TestContext = test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

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
    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

    let ctx: TestContext = test_config_with_new_key();

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

    let hash = "c".repeat(64);
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .set_payload(Bytes::from("test xorb data"))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}
