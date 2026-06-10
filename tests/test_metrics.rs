//! Tests for metrics monitoring and API

use actix_web::{test, web, App};
use xet_server::api::xorb::upload_xorb;
use xet_server::api::auth::{AuthVerifier, KeyPair};
use xet_server::server::{health_check, metrics_endpoint};
use xet_server::config::{AuthConfig, ServerConfig};
use xet_server::storage::local::LocalStorage;
use xet_server::metrics::GLOBAL_METRICS;
use tempfile::tempdir;

// Use serial test execution to avoid flaky tests with shared global state
use serial_test::serial;

#[actix_web::test]
#[serial]
async fn test_metrics_endpoint() {
    // Record some metrics first
    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_request(404);
    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_upload_bytes(1024);

    let app = test::init_service(
        App::new()
            .route("/metrics", web::get().to(metrics_endpoint))
    ).await;

    let req = test::TestRequest::get()
        .uri("/metrics")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body = test::read_body(resp).await;
    let body_str = std::str::from_utf8(&body).unwrap();

    // Verify Prometheus format metrics output
    assert!(body_str.contains("http_requests_total"));
    assert!(body_str.contains("storage_operations_total"));
    assert!(body_str.contains("upload_bytes_total"));
    // Verify new latency metrics format (total and count separated)
    assert!(body_str.contains("request_latency_us_total"));
    assert!(body_str.contains("request_latency_count"));
    assert!(body_str.contains("# HELP"));
    assert!(body_str.contains("# TYPE"));
}

#[actix_web::test]
#[serial]
async fn test_upload_records_metrics() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn xet_server::storage::StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    // Create a key and auth verifier for the test
    let kp = KeyPair::generate();
    let key_temp_dir = tempdir().unwrap();
    let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();
    let pub_key_path = key_temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
    std::fs::write(&pub_key_path, &public_key_pem).unwrap();

    let auth_config = AuthConfig {
        public_key_path: pub_key_path.to_str().unwrap().to_string(),
        trusted_kids: vec![kp.kid()],
    };

    let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();
    let config = ServerConfig {
        auth: auth_config,
        ..Default::default()
    };

    // Keep key_temp_dir alive by forgetting it (test scope is short)
    std::mem::forget(key_temp_dir);

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(auth_verifier))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(upload_xorb))
    ).await;

    // Record initial metrics
    let initial_requests = GLOBAL_METRICS.http_requests_total.load(std::sync::atomic::Ordering::Relaxed);

    // Send a request (will fail because no auth, but will record metrics)
    let hash = "0".repeat(64);
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .set_payload(vec![0u8; 100])
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401); // Unauthorized

    // Verify metrics increased
    let final_requests = GLOBAL_METRICS.http_requests_total.load(std::sync::atomic::Ordering::Relaxed);
    assert!(final_requests > initial_requests, "Request count should have increased");
}

#[actix_web::test]
async fn test_health_check() {
    let app = test::init_service(
        App::new()
            .route("/health", web::get().to(health_check))
    ).await;

    let req = test::TestRequest::get()
        .uri("/health")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "ok");
}
