//! Integration tests for metrics middleware

use actix_web::{App, HttpResponse, middleware::from_fn, test, web};
use serial_test::serial;
use std::sync::atomic::Ordering;
use xet_server::metrics::GLOBAL_METRICS;
use xet_server::middleware::metrics_middleware;
use xet_server::server::health_check;

#[actix_web::test]
#[serial]
async fn test_middleware_tracks_in_flight_requests() {
    // Record initial state
    let initial = GLOBAL_METRICS.active_requests.load(Ordering::Relaxed);

    let app = test::init_service(
        App::new()
            .wrap(from_fn(metrics_middleware))
            .route("/health", web::get().to(health_check)),
    )
    .await;

    // Make request
    let req = test::TestRequest::get().uri("/health").to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // After request completes, in-flight requests should return to initial
    let final_count = GLOBAL_METRICS.active_requests.load(Ordering::Relaxed);
    assert_eq!(
        final_count, initial,
        "in-flight requests should return to baseline after request"
    );
}

#[actix_web::test]
#[serial]
async fn test_health_and_ready_are_not_counted_as_requests() {
    let initial_requests = GLOBAL_METRICS.http_requests_total.load(Ordering::Relaxed);

    let app = test::init_service(
        App::new()
            .wrap(from_fn(metrics_middleware))
            .route("/health", web::get().to(health_check))
            .route("/ready", web::get().to(health_check)),
    )
    .await;

    for path in ["/health", "/ready"] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
        assert_eq!(resp.status(), 200);
    }

    let final_requests = GLOBAL_METRICS.http_requests_total.load(Ordering::Relaxed);
    assert_eq!(
        final_requests, initial_requests,
        "monitoring endpoints must not be counted as business requests"
    );
}

#[actix_web::test]
#[serial]
async fn test_middleware_counts_unmatched_routes() {
    let initial_requests = GLOBAL_METRICS.http_requests_total.load(Ordering::Relaxed);

    let app = test::init_service(
        App::new()
            .wrap(from_fn(metrics_middleware))
            .route("/known", web::get().to(HttpResponse::Ok)),
    )
    .await;

    // A request to an unknown path: no handler runs, but the middleware
    // (wrapping the whole app) still observes the 404 response.
    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/missing").to_request()).await;
    assert_eq!(resp.status(), 404);

    let final_requests = GLOBAL_METRICS.http_requests_total.load(Ordering::Relaxed);
    assert!(
        final_requests > initial_requests,
        "router 404s should be counted (they were invisible to handler-side recording)"
    );
}
