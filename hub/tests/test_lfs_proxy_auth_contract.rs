use actix_web::{App, HttpRequest, HttpResponse, test, web};
use ed25519_dalek::SigningKey;
use hub_api::auth::token_store::TokenStore;
use hub_api::auth::xet_signer::XetSigner;
use hub_api::cas_client::CasClient;
use hub_api::config::{CasSettings, HubConfig};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use std::sync::Arc;

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

async fn start_batch_cas_requiring_xet_scope(
    signer: Arc<XetSigner>,
    expected_scope: &'static str,
    oid: String,
) -> String {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());
    let base_url = url.clone();

    let server = actix_web::HttpServer::new(move || {
        let signer = signer.clone();
        let oid = oid.clone();
        let base_url = base_url.clone();

        App::new().route(
            "/objects/batch",
            web::post().to(move |req: HttpRequest| {
                let signer = signer.clone();
                let oid = oid.clone();
                let base_url = base_url.clone();

                async move {
                    let auth = req
                        .headers()
                        .get("Authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    let token = auth.strip_prefix("Bearer ").unwrap_or("");
                    let Some(claims) = signer.verify_xet_token(token) else {
                        return HttpResponse::Unauthorized().json(serde_json::json!({
                            "error": "expected xet user token"
                        }));
                    };
                    if !claims.scope.split_whitespace().any(|s| s == expected_scope) {
                        return HttpResponse::Forbidden().json(serde_json::json!({
                            "error": format!("expected scope {expected_scope}")
                        }));
                    }

                    HttpResponse::Ok().json(serde_json::json!({
                        "transfer": "basic",
                        "objects": [{
                            "oid": oid,
                            "size": 3,
                            "authenticated": true,
                            "actions": {
                                "download": {
                                    "href": format!("{base_url}/lfs/objects/{oid}"),
                                    "header": {
                                        "Authorization": "Bearer internal_should_not_leak"
                                    },
                                    "expires_in": 300
                                }
                            }
                        }]
                    }))
                }
            }),
        )
    })
    .listen(std_listener)
    .unwrap()
    .run();

    tokio::spawn(server);
    wait_for_listener(addr).await;

    url
}

async fn start_download_cas_requiring_proxy_token(
    signer: Arc<XetSigner>,
    oid: String,
    content: Vec<u8>,
) -> String {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    let server = actix_web::HttpServer::new(move || {
        let signer = signer.clone();
        let oid = oid.clone();
        let content = content.clone();

        App::new().route(
            "/lfs/objects/{oid}",
            web::get().to(move |req: HttpRequest| {
                let signer = signer.clone();
                let expected_oid = oid.clone();
                let content = content.clone();

                async move {
                    let auth = req
                        .headers()
                        .get("Authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    let token = auth.strip_prefix("Bearer ").unwrap_or("");
                    let Some(claims) = signer.verify_proxy_token(token) else {
                        return HttpResponse::Unauthorized().finish();
                    };
                    if claims.oid.as_deref() != Some(expected_oid.as_str())
                        || claims.operation.as_deref() != Some("download")
                    {
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

    url
}

#[actix_web::test]
async fn lfs_batch_forwards_xet_user_token_to_cas_batch() {
    let signer = test_signer();
    let oid = "a".repeat(64);
    let cas_url = start_batch_cas_requiring_xet_scope(signer.clone(), "read", oid.clone()).await;
    let token_store = Arc::new(TokenStore::in_memory().await.unwrap());
    let hf_token = token_store
        .create_token("testuser", "read-token", "read")
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
            .app_data(web::Data::new(signer))
            .app_data(web::Data::new(cas_client))
            .app_data(web::Data::new(HubConfig::default()))
            .route(
                "/objects/batch",
                web::post().to(hub_api::api::lfs_proxy::lfs_batch),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/objects/batch")
        .insert_header(("Authorization", format!("Bearer {hf_token}")))
        .insert_header(("Content-Type", "application/vnd.git-lfs+json"))
        .set_json(serde_json::json!({
            "operation": "download",
            "objects": [{"oid": oid, "size": 3}]
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "unexpected status: {}",
        resp.status()
    );

    let body: serde_json::Value = test::read_body_json(resp).await;
    let action = &body["objects"][0]["actions"]["download"];
    assert!(
        action["href"].as_str().unwrap().contains("token=proxy_"),
        "Hub must rewrite CAS action href to a Hub proxy token URL: {action}"
    );
    assert!(
        action["header"]["Authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer proxy_"),
        "Hub must replace CAS action auth with proxy token: {action}"
    );
}

#[actix_web::test]
async fn lfs_download_forwards_client_proxy_token_to_cas_object_endpoint() {
    let signer = test_signer();
    let content = b"abc".to_vec();
    let oid = hex::encode(Sha256::digest(&content));
    let cas_url =
        start_download_cas_requiring_proxy_token(signer.clone(), oid.clone(), content.clone())
            .await;
    let cas_client = Arc::new(
        CasClient::new(&CasSettings {
            base_url: cas_url,
            ..CasSettings::default()
        })
        .expect("CAS client should be created"),
    );
    let (proxy_token, _) = signer
        .sign_proxy("testuser", &oid, "download", "", "")
        .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(signer))
            .app_data(web::Data::new(cas_client))
            .app_data(web::Data::new(HubConfig::default()))
            .route(
                "/lfs/objects/{oid}",
                web::get().to(hub_api::api::lfs_proxy::lfs_download),
            ),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/lfs/objects/{oid}"))
        .insert_header(("Authorization", format!("Bearer {proxy_token}")))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "unexpected status: {}",
        resp.status()
    );
    let body = test::read_body(resp).await;
    assert_eq!(body.as_ref(), content.as_slice());
}
