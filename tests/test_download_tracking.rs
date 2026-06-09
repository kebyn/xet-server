//! Integration tests for download byte tracking

use actix_web::{test, web, App};
use xet_server::api::reconstruction::get_reconstruction;
use xet_server::config::ServerConfig;
use xet_server::storage::local::LocalStorage;
use xet_server::index::MetadataIndex;
use xet_server::metrics::GLOBAL_METRICS;
use std::sync::atomic::Ordering;
use tempfile::tempdir;
use serial_test::serial;

#[actix_web::test]
#[serial]
async fn test_v2_reconstruction_tracks_download_bytes() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn xet_server::storage::StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let config = ServerConfig::default();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .route("/v2/reconstructions/{file_id}", web::get().to(get_reconstruction))
    ).await;

    // Record initial download bytes
    let initial_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);

    // Request non-existent file (should not increment download bytes)
    let file_id = "a".repeat(64);
    let req = test::TestRequest::get()
        .uri(&format!("/v2/reconstructions/{}", file_id))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);

    // Download bytes should not have increased (error case)
    let final_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);
    assert_eq!(final_bytes, initial_bytes, "Download bytes should not increment on 404");
}

#[actix_web::test]
#[serial]
async fn test_v1_reconstruction_tracks_download_bytes() {
    use xet_server::api::reconstruction::get_reconstruction_v1;

    let dir = tempdir().unwrap();
    let storage: Box<dyn xet_server::storage::StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let config = ServerConfig::default();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .route("/v1/reconstructions/{file_id}", web::get().to(get_reconstruction_v1))
    ).await;

    // Record initial download bytes
    let initial_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);

    // Request non-existent file (should not increment download bytes)
    let file_id = "a".repeat(64);
    let req = test::TestRequest::get()
        .uri(&format!("/v1/reconstructions/{}", file_id))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);

    // Download bytes should not have increased (error case)
    let final_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);
    assert_eq!(final_bytes, initial_bytes, "Download bytes should not increment on 404");
}
