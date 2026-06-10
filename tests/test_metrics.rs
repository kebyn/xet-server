//! 测试性能监控和指标 API

use actix_web::{test, web, App};
use xet_server::api::xorb::upload_xorb;
use xet_server::server::{health_check, metrics_endpoint};
use xet_server::config::ServerConfig;
use xet_server::storage::local::LocalStorage;
use xet_server::metrics::GLOBAL_METRICS;
use tempfile::tempdir;

// Use serial test execution to avoid flaky tests with shared global state
use serial_test::serial;

#[actix_web::test]
#[serial]
async fn test_metrics_endpoint() {
    // 先记录一些指标
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

    // 验证 Prometheus 格式的指标输出
    assert!(body_str.contains("http_requests_total"));
    assert!(body_str.contains("storage_operations_total"));
    assert!(body_str.contains("upload_bytes_total"));
    // 验证新的延迟指标格式（总计和计数分离）
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

    let config = ServerConfig::default();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .route("/v1/xorbs/{prefix}/{hash}", web::post().to(upload_xorb))
    ).await;

    // 记录初始指标
    let initial_requests = GLOBAL_METRICS.http_requests_total.load(std::sync::atomic::Ordering::Relaxed);

    // 发送一个请求（会失败因为没有认证，但会记录指标）
    let hash = "0".repeat(64);
    let req = test::TestRequest::post()
        .uri(&format!("/v1/xorbs/default/{}", hash))
        .set_payload(vec![0u8; 100])
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401); // 未授权

    // 验证指标已增加
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
