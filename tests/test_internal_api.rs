//! Tests for internal API endpoints (Hub-to-CAS communication).
//!
//! These tests use the stateless endpoint design where:
//! - MetadataIndex determines xet_only vs raw_only
//! - Storage presence determines raw blob availability

mod common;

use actix_web::{App, http::Method, test, web};
use bytes::Bytes;
use tempfile::tempdir;

use common::{TestContext, test_token_for_keypair};
use xet_server::hash::compute_data_hash;
use xet_server::index::MetadataIndex;
use xet_server::storage::StorageBackend;
use xet_server::storage::local::LocalStorage;

fn create_test_context() -> TestContext {
    common::test_config_with_new_key()
}

/// Test that GET /internal/state/{oid} returns raw_only for a blob in storage.
#[actix_web::test]
async fn test_internal_get_state_raw() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();

    // Store a raw blob
    let content = b"test content for state query";
    let oid = compute_data_hash(content).to_hex();
    let object_key = format!("lfs/objects/{}", oid);
    storage
        .put(&object_key, Bytes::from(content.to_vec()))
        .await
        .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route(
                "/internal/state/{oid}",
                web::get().to(xet_server::api::internal::get_blob_state),
            ),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/internal/state/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["state"], "raw_only");
    assert!(body["xet_file_id"].is_null());
    assert!(body["converted_at"].is_null());
}

/// Test that GET /internal/state/{oid} returns 404 for unknown oid.
#[actix_web::test]
async fn test_internal_get_state_not_found() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route(
                "/internal/state/{oid}",
                web::get().to(xet_server::api::internal::get_blob_state),
            ),
    )
    .await;

    // Use a fake oid that doesn't exist
    let unknown_oid = "a".repeat(64);

    let req = test::TestRequest::get()
        .uri(&format!("/internal/state/{}", unknown_oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(body["error"].as_str().unwrap().contains("not found"));
}

/// Test that HEAD /internal/blob/{oid} returns X-Storage-State: raw_only.
#[actix_web::test]
async fn test_internal_head_blob_raw() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();

    // Store a raw blob
    let content = b"test content for head";
    let oid = compute_data_hash(content).to_hex();
    let object_key = format!("lfs/objects/{}", oid);
    storage
        .put(&object_key, Bytes::from(content.to_vec()))
        .await
        .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route(
                "/internal/blob/{oid}",
                web::head().to(xet_server::api::internal::head_blob),
            ),
    )
    .await;

    let req = test::TestRequest::default()
        .method(Method::HEAD)
        .uri(&format!("/internal/blob/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Check headers
    let storage_state = resp.headers().get("X-Storage-State").unwrap();
    assert_eq!(storage_state.to_str().unwrap(), "raw_only");
}

/// Test that HEAD /internal/blob/{oid} returns X-Storage-State: xet_only for blobs in MetadataIndex.
#[actix_web::test]
async fn test_internal_head_blob_xet() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();

    // Register a xet_only blob in MetadataIndex
    let oid = "b".repeat(64);
    index.register_shard("shard-test".to_string(), vec![oid.clone()], vec![]);

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route(
                "/internal/blob/{oid}",
                web::head().to(xet_server::api::internal::head_blob),
            ),
    )
    .await;

    let req = test::TestRequest::default()
        .method(Method::HEAD)
        .uri(&format!("/internal/blob/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Check headers
    let storage_state = resp.headers().get("X-Storage-State").unwrap();
    assert_eq!(storage_state.to_str().unwrap(), "xet_only");

    let file_id_header = resp.headers().get("X-File-Id").unwrap();
    assert_eq!(file_id_header.to_str().unwrap(), oid.as_str());
}

/// Test that internal endpoints reject tokens without "internal" scope.
#[actix_web::test]
async fn test_internal_rejects_non_internal_scope() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    // Use "read" scope instead of "internal"
    let token = test_token_for_keypair(&ctx.keypair, "read");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route(
                "/internal/state/{oid}",
                web::get().to(xet_server::api::internal::get_blob_state),
            )
            .route(
                "/internal/blob/{oid}",
                web::head().to(xet_server::api::internal::head_blob),
            ),
    )
    .await;

    let oid = "a".repeat(64);

    // Test GET /internal/state
    let req = test::TestRequest::get()
        .uri(&format!("/internal/state/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(body["error"].as_str().unwrap().contains("internal"));

    // Test HEAD /internal/blob
    let req = test::TestRequest::default()
        .method(Method::HEAD)
        .uri(&format!("/internal/blob/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

/// Test that HEAD /internal/blob/{oid} returns 404 when blob doesn't exist anywhere.
#[actix_web::test]
async fn test_internal_head_blob_not_found() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());

    let index = MetadataIndex::new();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route(
                "/internal/blob/{oid}",
                web::head().to(xet_server::api::internal::head_blob),
            ),
    )
    .await;

    // Use an oid that doesn't exist in index or storage
    let unknown_oid = "c".repeat(64);

    let req = test::TestRequest::default()
        .method(Method::HEAD)
        .uri(&format!("/internal/blob/{}", unknown_oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

/// Test that LFS upload succeeds (stateless — no state registration needed).
#[actix_web::test]
async fn test_lfs_upload_stores_blob() {
    let storage_dir = tempdir().unwrap();
    let upload_temp_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());

    // Create config with upload temp dir
    let config = xet_server::config::ServerConfig {
        storage: xet_server::config::StorageConfig {
            upload_temp_dir: Some(upload_temp_dir.path().to_str().unwrap().to_string()),
            ..ctx.config.storage
        },
        ..ctx.config
    };

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(config))
            .route(
                "/lfs/objects/{oid}",
                web::put().to(xet_server::api::lfs::upload_lfs_object),
            ),
    )
    .await;

    let content = b"upload test content";
    let oid = compute_data_hash(content).to_hex();

    let req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(content.to_vec()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Verify blob was stored
    let verify_storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());
    let object_key = format!("lfs/objects/{}", oid);
    assert!(verify_storage.exists(&object_key).await.unwrap());
}

/// Test that LFS download returns same data via raw path.
#[actix_web::test]
async fn test_lfs_download_raw_only() {
    let storage_dir = tempdir().unwrap();
    let upload_temp_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap());
    let storage_arc: std::sync::Arc<Box<dyn StorageBackend>> = std::sync::Arc::new(storage);

    let index = MetadataIndex::new();

    // Create config with upload temp dir
    let config = xet_server::config::ServerConfig {
        storage: xet_server::config::StorageConfig {
            upload_temp_dir: Some(upload_temp_dir.path().to_str().unwrap().to_string()),
            ..ctx.config.storage
        },
        ..ctx.config
    };

    let converting = std::sync::Arc::new(xet_server::conversion::ConvertingOids::new());
    let conversion_config = xet_server::config::ConversionConfig::default();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::from(storage_arc))
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(converting))
            .app_data(web::Data::new(conversion_config))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(config))
            .route(
                "/lfs/objects/{oid}",
                web::put().to(xet_server::api::lfs::upload_lfs_object),
            )
            .route(
                "/lfs/objects/{oid}",
                web::get().to(xet_server::api::lfs::download_lfs_object),
            ),
    )
    .await;

    let content = b"download test content";
    let oid = compute_data_hash(content).to_hex();

    // Upload first
    let upload_req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(content.to_vec()))
        .to_request();

    let upload_resp = test::call_service(&app, upload_req).await;
    assert_eq!(upload_resp.status(), 200);

    // Now download
    let download_req = test::TestRequest::get()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let download_resp = test::call_service(&app, download_req).await;
    assert_eq!(download_resp.status(), 200);

    let body = test::read_body(download_resp).await;
    assert_eq!(body.as_ref(), content);
}
