use actix_web::http::{Method, header::HeaderMap};
use actix_web::{App, HttpRequest, HttpResponse, test, web};
use ed25519_dalek::SigningKey;
use hub_api::auth::token_store::TokenStore;
use hub_api::auth::xet_signer::XetSigner;
use hub_api::cas_client::CasClient;
use hub_api::config::{CasSettings, HubConfig};
use hub_api::metadata::{FileEntry, MetadataStore, RepoType, Revision, SqliteMetadataStore};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn test_signer() -> Arc<XetSigner> {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    Arc::new(XetSigner::new(signing_key, "test-key", 3600, 300))
}

async fn wait_for_listener(addr: std::net::SocketAddr) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("mock CAS did not start listening on {addr}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn start_download_cas_requiring_xet_scope(
    signer: Arc<XetSigner>,
    expected_scope: &'static str,
    content: Vec<u8>,
) -> (String, Arc<AtomicUsize>) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());
    let request_count = Arc::new(AtomicUsize::new(0));
    let server_request_count = request_count.clone();

    let server = actix_web::HttpServer::new(move || {
        let signer = signer.clone();
        let content = content.clone();
        let request_count = server_request_count.clone();

        App::new().route(
            "/lfs/objects/{oid}",
            web::get().to(move |req: HttpRequest| {
                let signer = signer.clone();
                let content = content.clone();
                let request_count = request_count.clone();

                async move {
                    request_count.fetch_add(1, Ordering::SeqCst);
                    let auth = req
                        .headers()
                        .get("Authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    let token = auth.strip_prefix("Bearer ").unwrap_or("");
                    let Some(claims) = signer.verify_xet_token(token) else {
                        return HttpResponse::Unauthorized().finish();
                    };
                    if !claims.scope.split_whitespace().any(|s| s == expected_scope) {
                        return HttpResponse::Forbidden().finish();
                    }
                    if claims.repo_id.is_empty() {
                        return HttpResponse::Forbidden().finish();
                    }

                    HttpResponse::Ok()
                        .content_type("application/octet-stream")
                        .body(content)
                }
            }),
        )
    })
    .listen(std_listener)
    .unwrap()
    .run();

    tokio::spawn(server);
    wait_for_listener(addr).await;

    (url, request_count)
}

async fn resolve_inline_response(
    method: Method,
    expected_content: &[u8],
    expected_size: u64,
    cas_content: Vec<u8>,
) -> (actix_web::http::StatusCode, HeaderMap, web::Bytes, usize) {
    let signer = test_signer();
    let oid = hex::encode(Sha256::digest(expected_content));
    let (cas_url, cas_request_count) =
        start_download_cas_requiring_xet_scope(signer.clone(), "read", cas_content).await;

    let token_store = Arc::new(TokenStore::in_memory().await.unwrap());
    let token = token_store
        .create_token("testuser", "read-token", "read")
        .await
        .unwrap();
    let metadata: Arc<dyn MetadataStore> =
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
    let repo = metadata
        .create_repo("testuser", "my-model", RepoType::Model, false)
        .await
        .unwrap();
    let commit_id = "commit123";
    metadata
        .add_revision(Revision {
            commit_id: commit_id.to_string(),
            repo_id: repo.id,
            parent: None,
            message: "initial".to_string(),
            author: "testuser".to_string(),
            created_at: 1000,
        })
        .await
        .unwrap();
    metadata.set_head(repo.id, commit_id).await.unwrap();
    metadata
        .add_file_entries(vec![FileEntry {
            path: "config.json".to_string(),
            repo_id: repo.id,
            commit_id: commit_id.to_string(),
            size: expected_size,
            cas_hash: oid,
            is_lfs: false,
        }])
        .await
        .unwrap();

    let cas_client = Arc::new(
        CasClient::new(&CasSettings {
            base_url: cas_url,
            ..CasSettings::default()
        })
        .expect("CAS client should be created"),
    );

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store))
            .app_data(web::Data::new(metadata))
            .app_data(web::Data::new(HubConfig::default()))
            .app_data(web::Data::new(signer))
            .app_data(web::Data::new(cas_client))
            .route(
                "/{ns}/{repo}/resolve/{revision}/{path:.*}",
                web::get().to(hub_api::api::resolve::resolve_model),
            )
            .route(
                "/{ns}/{repo}/resolve/{revision}/{path:.*}",
                web::head().to(hub_api::api::resolve::resolve_model),
            ),
    )
    .await;

    let req = test::TestRequest::default()
        .method(method)
        .uri("/testuser/my-model/resolve/main/config.json")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = test::read_body(resp).await;
    let requests = cas_request_count.load(Ordering::SeqCst);
    (status, headers, body, requests)
}

fn assert_sanitized_bad_gateway(status: actix_web::http::StatusCode, body: &[u8]) {
    assert_eq!(status, actix_web::http::StatusCode::BAD_GATEWAY);
    let error: serde_json::Value =
        serde_json::from_slice(body).expect("bad gateway response should be JSON");
    assert_eq!(error["error"], "Upstream CAS request failed");
    assert_eq!(error["error_type"], "BadGateway");
}

#[actix_web::test]
async fn resolve_inline_fetch_uses_xet_user_token_for_cas_download() {
    let content = b"inline";
    let (status, _, body, requests) =
        resolve_inline_response(Method::GET, content, content.len() as u64, content.to_vec()).await;
    assert!(status.is_success(), "unexpected status: {}", status);
    assert_eq!(body.as_ref(), content);
    assert_eq!(requests, 1);
}

#[actix_web::test]
async fn resolve_inline_rejects_cas_content_with_wrong_size() {
    let expected = b"inline";
    let (status, _, body, _) = resolve_inline_response(
        Method::GET,
        expected,
        expected.len() as u64,
        b"short".to_vec(),
    )
    .await;

    assert_sanitized_bad_gateway(status, &body);
}

#[actix_web::test]
async fn resolve_inline_rejects_cas_content_with_wrong_sha256() {
    let expected = b"inline";
    let (status, _, body, _) = resolve_inline_response(
        Method::GET,
        expected,
        expected.len() as u64,
        b"damage".to_vec(),
    )
    .await;

    assert_sanitized_bad_gateway(status, &body);
}

#[actix_web::test]
async fn resolve_inline_head_uses_snapshot_metadata_without_fetching_cas_body() {
    let content = b"inline";
    let oid = hex::encode(Sha256::digest(content));
    let (status, headers, body, requests) = resolve_inline_response(
        Method::HEAD,
        content,
        content.len() as u64,
        content.to_vec(),
    )
    .await;

    assert_eq!(status, actix_web::http::StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(requests, 0, "HEAD must not download the CAS object body");
    assert_eq!(headers.get("Content-Length").unwrap(), "6");
    assert_eq!(headers.get("X-Repo-Commit").unwrap(), "commit123");
    assert_eq!(
        headers.get("ETag").unwrap().to_str().unwrap(),
        format!("\"{oid}\"")
    );
    assert_eq!(headers.get("X-Linked-Size").unwrap(), "6");
    assert_eq!(headers.get("X-Linked-Etag").unwrap().to_str().unwrap(), oid);
}

#[actix_web::test]
async fn resolve_inline_cas_timeout_returns_sanitized_504() {
    // A CAS that accepts connections but answers after 2 seconds, combined
    // with a 1-second client timeout, exercises the timeout classification
    // end to end through the resolve inline fetch.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(|| {
        App::new().route(
            "/lfs/objects/{oid}",
            web::get().to(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                HttpResponse::Ok().body("late")
            }),
        )
    })
    .listen(listener)
    .unwrap()
    .run();
    tokio::spawn(server);
    wait_for_listener(addr).await;
    let cas_url = format!("http://127.0.0.1:{}", addr.port());

    let signer = test_signer();
    let token_store = Arc::new(TokenStore::in_memory().await.unwrap());
    let token = token_store
        .create_token("testuser", "read-token", "read")
        .await
        .unwrap();
    let metadata: Arc<dyn MetadataStore> =
        Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
    let repo = metadata
        .create_repo("testuser", "my-model", RepoType::Model, false)
        .await
        .unwrap();
    let commit_id = "commit123";
    metadata
        .add_revision(Revision {
            commit_id: commit_id.to_string(),
            repo_id: repo.id,
            parent: None,
            message: "initial".to_string(),
            author: "testuser".to_string(),
            created_at: 1000,
        })
        .await
        .unwrap();
    metadata.set_head(repo.id, commit_id).await.unwrap();
    metadata
        .add_file_entries(vec![FileEntry {
            path: "config.json".to_string(),
            repo_id: repo.id,
            commit_id: commit_id.to_string(),
            size: 3,
            cas_hash: "a".repeat(64),
            is_lfs: false,
        }])
        .await
        .unwrap();

    let cas_client = Arc::new(
        CasClient::new(&CasSettings {
            base_url: cas_url,
            internal_timeout_seconds: 1,
            ..CasSettings::default()
        })
        .expect("CAS client should be created"),
    );

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store))
            .app_data(web::Data::new(metadata))
            .app_data(web::Data::new(HubConfig::default()))
            .app_data(web::Data::new(signer))
            .app_data(web::Data::new(cas_client))
            .route(
                "/{ns}/{repo}/resolve/{revision}/{path:.*}",
                web::get().to(hub_api::api::resolve::resolve_model),
            ),
    )
    .await;

    let req = test::TestRequest::get()
        .uri("/testuser/my-model/resolve/main/config.json")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::GATEWAY_TIMEOUT);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["error"], "Upstream CAS request failed");
    assert_eq!(body["error_type"], "GatewayTimeout");
}
