//! Regression tests for fresh-deployment upload behavior.
//!
//! The disk-space pre-check calls statvfs on the upload temp dir, which
//! requires the path to exist. Before the fix, the dir was created only
//! lazily by `TempFile::create` — *after* the check — so the first xorb/LFS
//! upload on a fresh deployment failed with 507 InsufficientStorage.
//! Existing tests masked this by pointing `upload_temp_dir` (or the storage
//! root) at directories that already existed on disk. These tests pin the
//! fix: uploads must succeed when the resolved temp dir does not exist yet.

mod common;

use actix_web::{App, test, web};
use bytes::Bytes;
use tempfile::tempdir;
use xet_server::api::lfs::upload_lfs_object;
use xet_server::config::ServerConfig;
use xet_server::hash::compute_data_hash;
use xet_server::storage::StorageBackend;
use xet_server::storage::local::LocalStorage;

use common::{TestContext, test_token_for_keypair};

/// Build a config whose resolved upload temp dir (`{local_path}/.tmp`)
/// does **not** exist yet, mimicking a freshly deployed server.
fn fresh_deployment_config(ctx: &TestContext, storage_root: &tempfile::TempDir) -> ServerConfig {
    ServerConfig {
        storage: xet_server::config::StorageConfig {
            backend: "local".to_string(),
            local_path: Some(storage_root.path().to_str().unwrap().to_string()),
            upload_temp_dir: None,
            reconstruction_temp_dir: None,
            ..ctx.config.storage.clone()
        },
        ..ctx.config.clone()
    }
}

/// The first LFS upload on a fresh deployment must succeed (not 507).
#[actix_web::test]
async fn first_lfs_upload_on_fresh_deployment_succeeds() {
    let storage_root = tempdir().unwrap();
    // Sanity: the resolved temp dir does not exist before the request.
    assert!(!storage_root.path().join(".tmp").exists());

    let ctx = common::test_config_with_new_key();
    let token = test_token_for_keypair(&ctx.keypair, "read write");
    let config = fresh_deployment_config(&ctx, &storage_root);

    let storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_root.path().to_str().unwrap()).unwrap());

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(ctx.auth_verifier.clone()))
            .app_data(web::Data::new(config))
            .route("/lfs/objects/{oid}", web::put().to(upload_lfs_object)),
    )
    .await;

    let content = b"fresh deployment upload";
    let oid = compute_data_hash(content).to_hex();

    let req = test::TestRequest::put()
        .uri(&format!("/lfs/objects/{}", oid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(Bytes::from(content.to_vec()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "first upload must not fail with 507");

    // The temp dir now exists and the object is durably stored.
    assert!(storage_root.path().join(".tmp").exists());
    let verify_storage: Box<dyn StorageBackend> =
        Box::new(LocalStorage::new(storage_root.path().to_str().unwrap()).unwrap());
    assert!(
        verify_storage
            .exists(&format!("lfs/objects/{}", oid))
            .await
            .unwrap()
    );
}
