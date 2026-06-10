//! Tests for internal API endpoints (Hub-to-CAS communication).

mod common;

use actix_web::{test, web, App, http::Method};
use bytes::Bytes;
use tempfile::tempdir;
use std::sync::Arc;

use common::{test_token_for_keypair, TestContext};
use xet_server::hash::compute_data_hash;
use xet_server::state::{SqliteStateManager, StorageState, StorageStateManager};
use xet_server::storage::local::LocalStorage;
use xet_server::storage::StorageBackend;

fn create_test_context() -> TestContext {
    common::test_config_with_new_key()
}

/// Test that GET /internal/state/{oid} returns raw_only for a registered blob.
#[actix_web::test]
async fn test_internal_get_state_raw() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

    // Register a raw blob
    let content = b"test content for state query";
    let oid = compute_data_hash(content).to_hex();
    state_mgr.register_raw_blob(&oid, content.len() as u64).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route("/internal/state/{oid}", web::get().to(xet_server::api::internal::get_blob_state)),
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
    assert_eq!(body["size"], content.len() as u64);
    assert!(body["xet_file_id"].is_null());
    assert!(body["converted_at"].is_null());
}

/// Test that GET /internal/state/{oid} returns 404 for unknown oid.
#[actix_web::test]
async fn test_internal_get_state_not_found() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route("/internal/state/{oid}", web::get().to(xet_server::api::internal::get_blob_state)),
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
    assert!(body["error"].as_str().unwrap().contains("No state found"));
}

/// Test that HEAD /internal/blob/{oid} returns X-Storage-State: raw_only.
#[actix_web::test]
async fn test_internal_head_blob_raw() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

    // Register a raw blob and store it
    let content = b"test content for head";
    let oid = compute_data_hash(content).to_hex();
    state_mgr.register_raw_blob(&oid, content.len() as u64).await.unwrap();

    // Store the blob
    let object_key = format!("lfs/objects/{}", oid);
    storage.put(&object_key, Bytes::from(content.to_vec())).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route("/internal/blob/{oid}", web::head().to(xet_server::api::internal::head_blob)),
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

/// Test that HEAD /internal/blob/{oid} returns X-Storage-State: xet_only for XetOnly blobs.
#[actix_web::test]
async fn test_internal_head_blob_xet() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "internal");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

    // Register a xet_only blob
    let oid = "b".repeat(64);
    let file_id = "file_001";
    state_mgr.register_xet_only(&oid, file_id, 1024).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route("/internal/blob/{oid}", web::head().to(xet_server::api::internal::head_blob)),
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
    assert_eq!(file_id_header.to_str().unwrap(), "file_001");
}

/// Test that internal endpoints reject tokens without "internal" scope.
#[actix_web::test]
async fn test_internal_rejects_non_internal_scope() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    // Use "read" scope instead of "internal"
    let token = test_token_for_keypair(&ctx.keypair, "read");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route("/internal/state/{oid}", web::get().to(xet_server::api::internal::get_blob_state))
            .route("/internal/blob/{oid}", web::head().to(xet_server::api::internal::head_blob)),
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

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route("/internal/blob/{oid}", web::head().to(xet_server::api::internal::head_blob)),
    )
    .await;

    // Use an oid that doesn't exist in state or storage
    let unknown_oid = "c".repeat(64);

    let req = test::TestRequest::default()
        .method(Method::HEAD)
        .uri(&format!("/internal/blob/{}", unknown_oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

/// Test that LFS upload registers state in state manager.
#[actix_web::test]
async fn test_lfs_upload_registers_state() {
    let storage_dir = tempdir().unwrap();
    let upload_temp_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

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
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(config))
            .route("/lfs/objects/{oid}", web::put().to(xet_server::api::lfs::upload_lfs_object)),
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

    // Verify state was registered
    let state = state_mgr.get_state(&oid).await.unwrap();
    assert!(state.is_some());

    let file_state = state.unwrap();
    assert_eq!(file_state.state, StorageState::RawOnly);
    assert_eq!(file_state.size, content.len() as u64);
}

/// Test that LFS download returns same data via raw path for raw_only blobs.
#[actix_web::test]
async fn test_lfs_download_raw_only() {
    let storage_dir = tempdir().unwrap();
    let upload_temp_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "read write");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

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
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(config))
            .route("/lfs/objects/{oid}", web::put().to(xet_server::api::lfs::upload_lfs_object))
            .route("/lfs/objects/{oid}", web::get().to(xet_server::api::lfs::download_lfs_object)),
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

/// Test that LFS download returns 501 for XetOnly blobs.
#[actix_web::test]
async fn test_lfs_download_xet_only_not_implemented() {
    let storage_dir = tempdir().unwrap();

    let ctx = create_test_context();
    let token = test_token_for_keypair(&ctx.keypair, "read");

    let storage: Box<dyn StorageBackend> = Box::new(
        LocalStorage::new(storage_dir.path().to_str().unwrap()).unwrap(),
    );

    let state_mgr: Arc<dyn StorageStateManager> = Arc::new(
        SqliteStateManager::new_in_memory().unwrap(),
    );

    // Register a xet_only blob
    let oid = "d".repeat(64);
    let file_id = "file_002";
    state_mgr.register_xet_only(&oid, file_id, 2048).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(state_mgr.clone()))
            .app_data(web::Data::new(ctx.config.clone()))
            .route("/lfs/objects/{oid}", web::get().to(xet_server::api::lfs::download_lfs_object)),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 501);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(body["error"].as_str().unwrap().contains("reconstruction"));
    assert_eq!(body["file_id"], "file_002");
}