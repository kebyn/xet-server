#[path = "support/process.rs"]
mod process;

use process::ServerProcess;
use xet_server::api::auth::KeyPair;

fn cas_env(dir: &std::path::Path, strict: bool) -> Vec<(&'static str, String)> {
    let key = KeyPair::generate();
    let public_path = dir.join("public.pem");
    std::fs::write(
        &public_path,
        KeyPair::public_key_to_pem(&key.verifying_key()).unwrap(),
    )
    .unwrap();
    vec![
        ("XET_HOST", "127.0.0.1".to_string()),
        ("XET_LOCAL_PATH", dir.join("storage").display().to_string()),
        ("CAS_PUBLIC_KEY_PATH", public_path.display().to_string()),
        ("XET_INDEX_REBUILD_STRICT", strict.to_string()),
    ]
}

#[test]
fn corrupt_shard_prevents_readiness_and_strict_startup() {
    for strict in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("storage/shards")).unwrap();
        std::fs::write(
            dir.path().join("storage/shards/broken-private-object"),
            b"corrupt shard",
        )
        .unwrap();
        let env = cas_env(dir.path(), strict);
        let mut server = ServerProcess::spawn(
            env!("CARGO_BIN_EXE_xet-server"),
            dir.path(),
            "XET_PORT",
            &env,
        );
        if strict {
            assert!(!server.wait_for_exit().success());
        } else {
            server.wait_for_health();
            let ready = server.request("/ready", &[]).unwrap();
            assert!(ready.starts_with("HTTP/1.1 503"));
            let body: serde_json::Value =
                serde_json::from_str(ready.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"status":"not_ready","checks":{"storage":"ok","index":"failed"}})
            );
        }
        let logs = std::fs::read_to_string(dir.path().join("server.log")).unwrap();
        assert!(logs.contains("broken-private-object"));
        assert!(logs.contains("1 shards failed"));
    }
}

#[test]
fn empty_storage_is_ready_on_real_server() {
    let dir = tempfile::tempdir().unwrap();
    let env = cas_env(dir.path(), true);
    let mut server = ServerProcess::spawn(
        env!("CARGO_BIN_EXE_xet-server"),
        dir.path(),
        "XET_PORT",
        &env,
    );
    server.wait_for_health();
    let ready = server.request("/ready", &[]).unwrap();
    assert!(ready.starts_with("HTTP/1.1 200"));
    let body: serde_json::Value =
        serde_json::from_str(ready.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"status":"ready","checks":{"storage":"ok","index":"ok"},"index_shard_count":0})
    );
}

#[test]
fn cas_access_log_omits_query_and_credential_headers() {
    let dir = tempfile::tempdir().unwrap();
    let env = cas_env(dir.path(), true);
    let mut server = ServerProcess::spawn(
        env!("CARGO_BIN_EXE_xet-server"),
        dir.path(),
        "XET_PORT",
        &env,
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
    let denied = server.request("/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?token=OTHER_QUERY_SECRET", &[("Authorization", "Bearer proxy_BADPROXY_MARKER")]).unwrap();
    assert!(denied.starts_with("HTTP/1.1 401"));
    let logs = process::wait_for_log(dir.path(), "status=401");
    for marker in [
        "QUERY_SECRET",
        "REFERER_SECRET",
        "AUTH_SECRET",
        "COOKIE_SECRET",
        "OTHER_QUERY_SECRET",
        "BADPROXY_MARKER",
    ] {
        assert!(!logs.contains(marker), "leaked {marker}");
    }
    assert!(logs.contains("peer=127.0.0.1"));
    assert!(logs.contains("method=GET path=/health status=200 bytes="));
    assert!(logs.contains("duration="));
}
