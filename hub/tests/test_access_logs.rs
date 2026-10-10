#[path = "../../tests/support/process.rs"]
mod process;

use actix_web::{App, HttpResponse, HttpServer, web};
use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};
use hub_api::auth::token_store::TokenStore;
use hub_api::metadata::{FileEntry, MetadataStore, RepoType, Revision, SqliteMetadataStore};
use process::ServerProcess;
use std::net::TcpListener;

#[actix_web::test]
async fn real_hub_logs_hide_credentials_and_proxy_redirect_still_downloads() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let cas_addr = listener.local_addr().unwrap();
    let cas = HttpServer::new(|| {
        App::new()
            .route(
                "/health",
                web::get().to(|| async { HttpResponse::Ok().finish() }),
            )
            .route(
                "/lfs/objects/{oid}",
                web::get().to(|| async { HttpResponse::Ok().body("download content") }),
            )
    })
    .workers(1)
    .listen(listener)
    .unwrap()
    .run();
    let cas_handle = cas.handle();
    tokio::spawn(cas);

    let dir = tempfile::tempdir().unwrap();
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let private_path = dir.path().join("private.pem");
    std::fs::write(
        &private_path,
        key.to_pkcs8_pem(pkcs8::LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    let db = dir.path().join("hub.db");
    let tokens = TokenStore::new(db.to_str().unwrap(), 1).await.unwrap();
    let token = tokens
        .create_token("owner", "log-test", "read")
        .await
        .unwrap();
    let metadata = SqliteMetadataStore::new(db.to_str().unwrap(), 1)
        .await
        .unwrap();
    let repo = metadata
        .create_repo("owner", "repo", RepoType::Model, false)
        .await
        .unwrap();
    let oid = "a".repeat(64);
    metadata
        .commit_atomic(
            &Revision {
                commit_id: "initial".to_string(),
                repo_id: repo.id,
                parent: None,
                message: "initial".to_string(),
                author: "owner".to_string(),
                created_at: 1,
            },
            &[FileEntry {
                path: "model.bin".to_string(),
                repo_id: repo.id,
                commit_id: "initial".to_string(),
                size: 16,
                cas_hash: oid.clone(),
                is_lfs: true,
            }],
            None,
        )
        .await
        .unwrap();

    let mut server = ServerProcess::spawn(
        env!("CARGO_BIN_EXE_hub-api"),
        dir.path(),
        "HUB_PORT",
        &[
            ("HUB_HOST", "127.0.0.1".to_string()),
            ("HUB_SQLITE_PATH", db.display().to_string()),
            ("HUB_PRIVATE_KEY_PATH", private_path.display().to_string()),
            ("HUB_CAS_BASE_URL", format!("http://{cas_addr}")),
            ("HUB_INLINE_THRESHOLD", "1".to_string()),
        ],
    );
    server.wait_for_health();
    let response = server
        .request(
            "/health?token=QUERY_SECRET",
            &[
                ("Referer", "https://example.test/?token=REFERER_SECRET"),
                ("Authorization", "Bearer AUTH_SECRET"),
                ("Cookie", "session=COOKIE_SECRET"),
            ],
        )
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    let denied = server
        .request(
            &format!("/lfs/objects/{oid}?token=proxy_BADPROXY_MARKER"),
            &[],
        )
        .unwrap();
    assert!(denied.starts_with("HTTP/1.1 401"));

    let redirect = server
        .request(
            "/owner/repo/resolve/main/model.bin",
            &[("Authorization", &format!("Bearer {token}"))],
        )
        .unwrap();
    assert!(redirect.starts_with("HTTP/1.1 302"), "{redirect}");
    let location = redirect
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("location").then(|| value.trim())
        })
        .unwrap();
    let location = url::Url::parse(location).unwrap();
    let query = location.query().unwrap();
    let proxy_token = location
        .query_pairs()
        .find(|(name, _)| name == "token")
        .unwrap()
        .1
        .into_owned();
    let downloaded = server
        .request(&format!("{}?{query}", location.path()), &[])
        .unwrap();
    assert!(downloaded.starts_with("HTTP/1.1 200"), "{downloaded}");
    assert!(downloaded.ends_with("download content"));
    let logs = process::wait_for_log(
        dir.path(),
        &format!("method=GET path=/lfs/objects/{oid} status=200"),
    );
    for marker in [
        "QUERY_SECRET",
        "REFERER_SECRET",
        "AUTH_SECRET",
        "COOKIE_SECRET",
        "BADPROXY_MARKER",
        &proxy_token,
        &token,
    ] {
        assert!(!logs.contains(marker), "leaked credential marker");
    }
    assert!(logs.contains("peer=127.0.0.1"));
    assert!(logs.contains("method=GET path=/health status=200 bytes="));
    assert!(logs.contains("method=GET path=/owner/repo/resolve/main/model.bin status=302"));
    assert!(logs.contains("duration="));
    assert!(logs.contains("verification_failed"));
    cas_handle.stop(true).await;
}
