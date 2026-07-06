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
) -> String {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    let server = actix_web::HttpServer::new(move || {
        let signer = signer.clone();
        let content = content.clone();

        App::new().route(
            "/lfs/objects/{oid}",
            web::get().to(move |req: HttpRequest| {
                let signer = signer.clone();
                let content = content.clone();

                async move {
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

    url
}

#[actix_web::test]
async fn resolve_inline_fetch_uses_xet_user_token_for_cas_download() {
    let signer = test_signer();
    let content = b"inline".to_vec();
    let oid = hex::encode(Sha256::digest(&content));
    let cas_url =
        start_download_cas_requiring_xet_scope(signer.clone(), "read", content.clone()).await;

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
            size: content.len() as u64,
            cas_hash: oid,
            is_lfs: false,
        }])
        .await
        .unwrap();

    let cas_client = Arc::new(CasClient::new(&CasSettings {
        base_url: cas_url,
        ..CasSettings::default()
    }));

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
        .insert_header(("Authorization", format!("Bearer {token}")))
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
