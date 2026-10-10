//! Assembly regression tests for the real Hub application.
//!
//! These tests build the app through `hub_api::server::build_app` — the exact
//! factory the production binary uses — with real components (real
//! `CasClient` against a spawned mock CAS, real `TokenStore` /
//! `SqliteMetadataStore` / `XetSigner`). A hand-rolled `App::new()` once
//! masked a missing `Data<Arc<dyn CasClientTrait>>` registration: every unit
//! and integration test stayed green while every production commit request
//! failed with HTTP 500 "Requested application data is not configured
//! correctly". Going through `build_app` here makes that class of assembly
//! drift fail in `cargo test` instead of in production.

use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::{App, HttpRequest, HttpResponse, HttpServer, test, web};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::SigningKey;
use hub_api::auth::token_store::TokenStore;
use hub_api::auth::xet_signer::XetSigner;
use hub_api::cas_client::CasClient;
use hub_api::config::HubConfig;
use hub_api::metadata::sqlite::SqliteMetadataStore;
use hub_api::metadata::{MetadataStore, RepoType};
use hub_api::server::{HubAppDeps, build_app, governor_config};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePoolOptions;

/// Peer address for requests routed through the Governor-wrapped scope.
/// `PeerIpKeyExtractor` rejects requests without a peer address, and under
/// `test::call_service` that surfaces as a panic (service error), not a 429.
fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 61616))
}

async fn wait_for_listener(addr: SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);

    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("mock CAS did not start listening on {addr}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Mock CAS: records every inline upload as `(oid, Authorization header)`.
async fn start_mock_cas() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    start_mock_cas_inner(None).await
}

/// Like [`start_mock_cas`] but the inline-upload PUT stalls — used by the
/// upload-timeout test.
async fn start_mock_cas_with_slow_put() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    start_mock_cas_inner(Some(Duration::from_secs(2))).await
}

async fn start_mock_cas_inner(
    put_delay: Option<Duration>,
) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let uploads: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let uploads_for_handler = uploads.clone();
    let server = HttpServer::new(move || {
        let uploads = uploads_for_handler.clone();
        App::new()
            .route(
                "/lfs/objects/{oid}",
                web::put().to(move |path: web::Path<String>, req: HttpRequest| {
                    let uploads = uploads.clone();
                    async move {
                        if let Some(delay) = put_delay {
                            tokio::time::sleep(delay).await;
                        }
                        let auth = req
                            .headers()
                            .get("Authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        uploads.lock().unwrap().push((path.into_inner(), auth));
                        HttpResponse::Ok().finish()
                    }
                }),
            )
            .route(
                "/ready",
                web::get().to(|| async {
                    HttpResponse::Ok().json(serde_json::json!({"status": "ready"}))
                }),
            )
            // HEAD verification endpoint that answers slowly — used by the
            // commit-timeout test (the commit flow's internal HEAD check).
            .route(
                "/internal/blob/{oid}",
                web::head().to(|| async {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    HttpResponse::Ok()
                        .insert_header(("X-Blob-Size", "3"))
                        .finish()
                }),
            )
    })
    .listen(listener)
    .unwrap()
    .run();

    tokio::spawn(server);
    wait_for_listener(addr).await;

    (format!("http://{addr}"), uploads)
}

/// Real `HubAppDeps` with a real `CasClient` pointed at the mock CAS.
async fn real_deps(cas_base_url: &str) -> (HubAppDeps, Arc<TokenStore>, Arc<dyn MetadataStore>) {
    real_deps_with_timeout(cas_base_url, 5).await
}

/// Like [`real_deps`] but with a configurable CAS request timeout (seconds).
async fn real_deps_with_timeout(
    cas_base_url: &str,
    internal_timeout_seconds: u64,
) -> (HubAppDeps, Arc<TokenStore>, Arc<dyn MetadataStore>) {
    let token_store = Arc::new(TokenStore::in_memory().await.unwrap());
    let metadata: Arc<dyn MetadataStore> =
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
    let signer = Arc::new(XetSigner::new(
        SigningKey::generate(&mut OsRng),
        "test-key",
        3600,
        300,
    ));
    let cas_client = Arc::new(
        CasClient::new(&hub_api::config::CasSettings {
            base_url: cas_base_url.to_string(),
            internal_timeout_seconds,
            max_download_size: 1024,
            health_check_timeout_seconds: 5,
        })
        .unwrap(),
    );
    let ready_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    // High RPM so the Governor never rejects test requests with 429.
    let mut config = HubConfig::default();
    config.server.rate_limit_rpm = 60_000;
    config.cas.base_url = cas_base_url.to_string();

    let deps = HubAppDeps {
        config,
        token_store: token_store.clone(),
        metadata: metadata.clone(),
        signer,
        cas_client,
        ready_pool,
        governor_conf: governor_config(Duration::from_millis(1), 60_000).unwrap(),
    };

    (deps, token_store, metadata)
}

#[actix_web::test]
async fn commit_routes_only_write_main_without_side_effects_on_rejection() {
    let (cas_url, uploads) = start_mock_cas().await;
    let (mut deps, token_store, _) = real_deps(&cas_url).await;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let metadata = Arc::new(SqliteMetadataStore::with_pool(pool.clone()).await.unwrap());
    deps.metadata = metadata.clone();
    let token = token_store
        .create_token("owner", "test", "write")
        .await
        .unwrap();
    let app = test::init_service(build_app(deps)).await;

    for (route, repo_type) in [
        ("models", RepoType::Model),
        ("datasets", RepoType::Dataset),
        ("spaces", RepoType::Space),
    ] {
        let repo = metadata
            .create_repo("owner", "repo", repo_type, false)
            .await
            .unwrap();
        let initial = test::TestRequest::post()
            .uri(&format!("/api/{route}/owner/repo/commit/main"))
            .peer_addr(peer())
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_payload("{\"key\":\"header\",\"value\":{\"summary\":\"initial\"}}\n{\"key\":\"file\",\"value\":{\"path\":\"a\",\"content\":\"YQ==\"}}")
            .to_request();
        let response = test::call_service(&app, initial).await;
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = test::read_body_json(response).await;
        let head = body["commitOid"].as_str().unwrap();
        let count_before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM revisions WHERE repo_id = ?")
                .bind(repo.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let calls_before = uploads.lock().unwrap().len();

        for revision in ["feature", head, "Main"] {
            let body = format!(
                "{{\"key\":\"header\",\"value\":{{\"summary\":\"rejected\",\"parentRevision\":\"{head}\"}}}}\n{{\"key\":\"file\",\"value\":{{\"path\":\"b\",\"content\":\"Yg==\"}}}}"
            );
            let request = test::TestRequest::post()
                .uri(&format!("/api/{route}/owner/repo/commit/{revision}"))
                .peer_addr(peer())
                .insert_header(("Authorization", format!("Bearer {token}")))
                .set_payload(body)
                .to_request();
            let response = test::call_service(&app, request).await;
            assert_eq!(response.status(), 400);
            let error: serde_json::Value = test::read_body_json(response).await;
            assert_eq!(error["error_type"], "ValidationError");
            assert!(error["error"].as_str().unwrap().contains("'main'"));
            assert_eq!(
                metadata.get_head(repo.id).await.unwrap().as_deref(),
                Some(head)
            );
            let count_after: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM revisions WHERE repo_id = ?")
                    .bind(repo.id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(count_after, count_before);
            assert_eq!(uploads.lock().unwrap().len(), calls_before);
        }
    }
}

#[actix_web::test]
async fn commit_with_inline_file_returns_200_on_real_app_assembly() {
    let (cas_url, uploads) = start_mock_cas().await;
    let (deps, token_store, metadata) = real_deps(&cas_url).await;
    let token = token_store
        .create_token("testuser", "assembly-token", "write")
        .await
        .unwrap();
    metadata
        .create_repo("testuser", "assembly-model", RepoType::Model, false)
        .await
        .unwrap();

    let app = test::init_service(build_app(deps)).await;

    let content = STANDARD.encode("{\"test\": true}");
    let body = format!(
        "{{\"key\":\"header\",\"value\":{{\"summary\":\"Add config\",\"parentRevision\":null}}}}\n\
         {{\"key\":\"file\",\"value\":{{\"path\":\"config.json\",\"content\":\"{}\"}}}}",
        content
    );

    let req = test::TestRequest::post()
        .uri("/api/models/testuser/assembly-model/commit/main")
        .peer_addr(peer())
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", "application/x-ndjson"))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    let body: serde_json::Value = test::read_body_json(resp).await;
    // The production symptom of an app_data mismatch is a 500 whose body
    // renders the extractor error — assert the success shape instead.
    assert!(!body.to_string().contains("Requested application data"));
    let commit_oid = body["commitOid"].as_str().unwrap_or_default().to_string();
    assert!(!commit_oid.is_empty());
    assert_eq!(
        body["commitUrl"].as_str().unwrap_or_default(),
        format!("/testuser/assembly-model/commit/{commit_oid}")
    );

    // The inline file must have been uploaded to (mock) CAS exactly once,
    // content-addressed, authorized by an xet_ write token.
    let uploads = uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1, "expected exactly one inline upload");
    assert_eq!(
        uploads[0].0,
        hex::encode(Sha256::digest(b"{\"test\": true}"))
    );
    assert!(
        uploads[0].1.starts_with("Bearer xet_"),
        "upload must be authorized by an xet write token, got: {}",
        uploads[0].1
    );
}

#[actix_web::test]
async fn ready_endpoint_reports_database_and_cas_ready_on_real_app_assembly() {
    let (cas_url, _uploads) = start_mock_cas().await;
    let (deps, _token_store, _metadata) = real_deps(&cas_url).await;

    let app = test::init_service(build_app(deps)).await;

    // /ready is registered at App level and bypasses the Governor scope,
    // so no peer address is needed here.
    let req = test::TestRequest::get().uri("/ready").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["checks"]["database"], "ok");
    assert_eq!(body["checks"]["cas"], "ok");
}

#[actix_web::test]
async fn preupload_is_not_a_data_configuration_error_on_real_app_assembly() {
    let (cas_url, _uploads) = start_mock_cas().await;
    let (deps, token_store, metadata) = real_deps(&cas_url).await;
    let token = token_store
        .create_token("testuser", "assembly-token", "write")
        .await
        .unwrap();
    metadata
        .create_repo("testuser", "assembly-model", RepoType::Model, false)
        .await
        .unwrap();

    let app = test::init_service(build_app(deps)).await;

    let req = test::TestRequest::post()
        .uri("/api/models/testuser/assembly-model/preupload/main")
        .peer_addr(peer())
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(serde_json::json!({
            "files": [{"path": "config.json", "size": 1024}]
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    // Any app_data type mismatch would surface as exactly HTTP 500 here.
    assert_ne!(
        resp.status(),
        actix_web::http::StatusCode::INTERNAL_SERVER_ERROR
    );

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(!body.to_string().contains("Requested application data"));
    assert_eq!(body["files"][0]["uploadMode"], "regular");
}

#[actix_web::test]
async fn commit_cas_verification_timeout_returns_504() {
    let (cas_url, _uploads) = start_mock_cas().await;
    let (deps, token_store, metadata) = real_deps_with_timeout(&cas_url, 1).await;
    let token = token_store
        .create_token("testuser", "assembly-token", "write")
        .await
        .unwrap();
    metadata
        .create_repo("testuser", "assembly-model", RepoType::Model, false)
        .await
        .unwrap();

    let app = test::init_service(build_app(deps)).await;

    // An lfsFile op triggers the internal CAS HEAD verification, which the
    // mock answers after 2s — past the 1s client timeout.
    let body = format!(
        "{{\"key\":\"header\",\"value\":{{\"summary\":\"slow cas\",\"parentRevision\":null}}}}\n\
         {{\"key\":\"lfsFile\",\"value\":{{\"path\":\"model.bin\",\"oid\":\"{}\",\"size\":3}}}}",
        "a".repeat(64)
    );
    let req = test::TestRequest::post()
        .uri("/api/models/testuser/assembly-model/commit/main")
        .peer_addr(peer())
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", "application/x-ndjson"))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::GATEWAY_TIMEOUT);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["error"], "Upstream CAS request failed");
    assert_eq!(body["error_type"], "GatewayTimeout");
}

#[actix_web::test]
async fn commit_inline_upload_timeout_returns_504() {
    let (cas_url, _uploads) = start_mock_cas_with_slow_put().await;
    let (deps, token_store, metadata) = real_deps_with_timeout(&cas_url, 1).await;
    let token = token_store
        .create_token("testuser", "assembly-token", "write")
        .await
        .unwrap();
    metadata
        .create_repo("testuser", "assembly-model", RepoType::Model, false)
        .await
        .unwrap();

    let app = test::init_service(build_app(deps)).await;

    // The inline-file commit reaches PUT /lfs/objects/{oid}, which the mock
    // answers after 2s — past the 1s client timeout.
    let content = STANDARD.encode("timeout");
    let body = format!(
        "{{\"key\":\"header\",\"value\":{{\"summary\":\"slow cas upload\",\"parentRevision\":null}}}}\n\
         {{\"key\":\"file\",\"value\":{{\"path\":\"config.json\",\"content\":\"{}\"}}}}",
        content
    );
    let req = test::TestRequest::post()
        .uri("/api/models/testuser/assembly-model/commit/main")
        .peer_addr(peer())
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", "application/x-ndjson"))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::GATEWAY_TIMEOUT);

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["error"], "Upstream CAS request failed");
    assert_eq!(body["error_type"], "GatewayTimeout");
}
