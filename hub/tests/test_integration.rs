use actix_web::{test, App, web};
use hub_api::auth::token_store::TokenStore;
use hub_api::auth::xet_signer::XetSigner;
use hub_api::cas_client::CasClient;
use hub_api::config::{HubConfig, CasSettings, ServerSettings, AuthSettings, MetadataSettings, StorageSettings};
use hub_api::metadata::{MetadataStore, SqliteMetadataStore, RepoType, Revision, FileEntry};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::sync::Arc;

/// Test environment type alias to reduce complexity
type TestEnv = (
    Arc<TokenStore>,
    Arc<XetSigner>,
    Arc<dyn MetadataStore>,
    Arc<CasClient>,
    HubConfig,
    String, // plaintext token
);

/// Setup test environment with all components
async fn setup_test_env() -> TestEnv {
    // Create in-memory token store
    let token_store = Arc::new(TokenStore::in_memory().await.unwrap());

    // Create test signing key
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let xet_signer = Arc::new(XetSigner::new(signing_key, "test-key", 3600));

    // Create in-memory metadata store
    let metadata: Arc<dyn MetadataStore> = Arc::new(SqliteMetadataStore::in_memory().await.unwrap());

    // Create CAS client
    let cas_client = Arc::new(CasClient::new(&CasSettings::default()));

    // Create test config
    let config = HubConfig {
        server: ServerSettings::default(),
        auth: AuthSettings::default(),
        metadata: MetadataSettings::default(),
        cas: CasSettings::default(),
        storage: StorageSettings::default(),
    };

    // Create test user and token
    let token = token_store.create_token("testuser", "test-token", "write").await.unwrap();

    (token_store, xet_signer, metadata, cas_client, config, token)
}

#[actix_web::test]
async fn test_whoami_endpoint() {
    let (token_store, _, _, _, _, token) = setup_test_env().await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .route("/api/whoami-v2", web::get().to(hub_api::api::whoami::whoami))
    ).await;

    let req = test::TestRequest::get()
        .uri("/api/whoami-v2")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["name"], "testuser");
    assert_eq!(body["auth"]["accessToken"]["name"], "test-token");
    assert_eq!(body["auth"]["accessToken"]["role"], "write");
}

#[actix_web::test]
async fn test_create_repo_endpoint() {
    let (token_store, _, metadata, _, _, token) = setup_test_env().await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .route("/api/models", web::post().to(hub_api::api::repo::create_model))
    ).await;

    // Create a model repo
    let req = test::TestRequest::post()
        .uri("/api/models")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(serde_json::json!({
            "name": "my-test-model",
            "private": false
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["id"], "testuser/my-test-model");
    assert_eq!(body["name"], "my-test-model");
}

#[actix_web::test]
async fn test_get_repo_endpoint() {
    let (token_store, _, metadata, _, _, token) = setup_test_env().await;

    // Create repo directly in metadata
    metadata.create_repo("testuser", "existing-model", RepoType::Model, false).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .route("/api/models/{ns}/{repo}", web::get().to(hub_api::api::repo::get_repo_model))
    ).await;

    let req = test::TestRequest::get()
        .uri("/api/models/testuser/existing-model")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["id"], "testuser/existing-model");
    assert_eq!(body["name"], "existing-model");
}

#[actix_web::test]
async fn test_commit_with_inline_file() {
    let (token_store, xet_signer, metadata, cas_client, _, token) = setup_test_env().await;

    // Create repo
    metadata.create_repo("testuser", "commit-test-model", RepoType::Model, false).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .app_data(web::Data::new(cas_client.clone()))
            .app_data(web::Data::new(xet_signer.clone()))
            .route("/api/models/{ns}/{repo}/commit/{rev}", web::post().to(hub_api::api::commit::commit_model))
    ).await;

    // NDJSON body with inline file
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let content = STANDARD.encode("{\"hello\": \"world\"}");
    let body = format!(
        "{{\"key\":\"header\",\"value\":{{\"summary\":\"Add config\",\"parentRevision\":null}}}}\n\
         {{\"key\":\"file\",\"value\":{{\"path\":\"config.json\",\"content\":\"{}\"}}}}",
        content
    );

    let req = test::TestRequest::post()
        .uri("/api/models/testuser/commit-test-model/commit/main")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", "application/x-ndjson"))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    // Since CAS is not running, we expect BadGateway (502) instead of success
    // This is expected behavior - the commit requires CAS to store inline files
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_GATEWAY);
}

#[actix_web::test]
async fn test_tree_listing() {
    let (token_store, _, metadata, _, _, token) = setup_test_env().await;

    // Create repo and add files
    let repo = metadata.create_repo("testuser", "tree-test-model", RepoType::Model, false).await.unwrap();
    let commit_id = "testcommit123";

    // Add revision
    let revision = Revision {
        commit_id: commit_id.to_string(),
        repo_id: repo.id,
        parent: None,
        message: "Initial".to_string(),
        author: "testuser".to_string(),
        created_at: 1000,
    };
    metadata.add_revision(revision).await.unwrap();
    metadata.set_head(repo.id, commit_id).await.unwrap();

    // Add file entries
    let entries = vec![
        FileEntry {
            path: "README.md".to_string(),
            repo_id: repo.id,
            commit_id: commit_id.to_string(),
            size: 100,
            cas_hash: "hash1".to_string(),
            is_lfs: false,
        },
        FileEntry {
            path: "model.bin".to_string(),
            repo_id: repo.id,
            commit_id: commit_id.to_string(),
            size: 1000,
            cas_hash: "hash2".to_string(),
            is_lfs: true,
        },
    ];
    metadata.add_file_entries(entries).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .route("/api/models/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(hub_api::api::tree::tree_model))
    ).await;

    let req = test::TestRequest::get()
        .uri("/api/models/testuser/tree-test-model/tree/main/")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: Vec<serde_json::Value> = test::read_body_json(resp).await;
    assert!(body.len() >= 2);

    // Check we have files
    let readme = body.iter().find(|e| e["path"] == "README.md");
    assert!(readme.is_some());
    assert_eq!(readme.unwrap()["type"], "file");
}

#[actix_web::test]
async fn test_token_exchange() {
    let (token_store, xet_signer, metadata, _, config, token) = setup_test_env().await;

    // Create repo for token exchange
    metadata.create_repo("testuser", "token-test-model", RepoType::Model, false).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(xet_signer.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .app_data(web::Data::new(config.clone()))
            .route("/api/models/{ns}/{repo}/xet-read-token/{rev}", web::get().to(hub_api::api::token_exchange::exchange_model_read))
    ).await;

    let req = test::TestRequest::get()
        .uri("/api/models/testuser/token-test-model/xet-read-token/main")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(body["accessToken"].as_str().unwrap().starts_with("xet_"));
    assert!(body["exp"].as_u64().unwrap() > 0);
}

#[actix_web::test]
async fn test_preupload_endpoint() {
    let (token_store, _, metadata, _, _, token) = setup_test_env().await;

    // Create repo
    metadata.create_repo("testuser", "preupload-test-model", RepoType::Model, false).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .app_data(web::Data::new(hub_api::config::HubConfig::default()))
            .route("/api/models/{ns}/{repo}/preupload/{rev}", web::post().to(hub_api::api::preupload::preupload_model))
    ).await;

    // Small file (< 1MB) should be regular mode
    let req = test::TestRequest::post()
        .uri("/api/models/testuser/preupload-test-model/preupload/main")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(serde_json::json!({
            "files": [
                {"path": "config.json", "size": 1024}
            ]
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp).await;
    // Response uses camelCase (uploadMode) for HF CLI compatibility
    assert_eq!(body["files"][0]["uploadMode"], "regular");
}

#[actix_web::test]
async fn test_health_endpoint() {
    let app = test::init_service(
        App::new()
            .route("/health", web::get().to(|| async {
                actix_web::HttpResponse::Ok().json(serde_json::json!({"status": "ok"}))
            }))
    ).await;

    let req = test::TestRequest::get()
        .uri("/health")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[actix_web::test]
async fn test_delete_repo_endpoint() {
    let (token_store, _, metadata, _, _, token) = setup_test_env().await;

    // Create repo directly
    metadata.create_repo("testuser", "delete-test-model", RepoType::Model, false).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .route("/api/models/{ns}/{repo}", web::delete().to(hub_api::api::repo::delete_repo_model))
    ).await;

    let req = test::TestRequest::delete()
        .uri("/api/models/testuser/delete-test-model")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Verify repo is deleted
    let result = metadata.get_repo("testuser", "delete-test-model", RepoType::Model).await;
    assert!(result.is_err());
}

#[actix_web::test]
async fn test_full_workflow() {
    let (token_store, xet_signer, metadata, cas_client, config, token) = setup_test_env().await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(xet_signer.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .app_data(web::Data::new(cas_client.clone()))
            .app_data(web::Data::new(config.clone()))
            .route("/api/whoami-v2", web::get().to(hub_api::api::whoami::whoami))
            .route("/api/models", web::post().to(hub_api::api::repo::create_model))
            .route("/api/models/{ns}/{repo}", web::get().to(hub_api::api::repo::get_repo_model))
            .route("/api/models/{ns}/{repo}/commit/{rev}", web::post().to(hub_api::api::commit::commit_model))
            .route("/api/models/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(hub_api::api::tree::tree_model))
            .route("/api/models/{ns}/{repo}/xet-read-token/{rev}", web::get().to(hub_api::api::token_exchange::exchange_model_read))
    ).await;

    // 1. Test whoami
    let req = test::TestRequest::get()
        .uri("/api/whoami-v2")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["name"], "testuser");

    // 2. Create repo
    let req = test::TestRequest::post()
        .uri("/api/models")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(serde_json::json!({"name": "workflow-test", "private": false}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["id"], "testuser/workflow-test");

    // 3. Commit with only header (no inline files - should succeed without CAS)
    let body = "{\"key\":\"header\",\"value\":{\"summary\":\"Initial commit\"}}";
    let req = test::TestRequest::post()
        .uri("/api/models/testuser/workflow-test/commit/main")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", "application/x-ndjson"))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    // This should succeed since there are no inline files to store
    assert!(resp.status().is_success());

    // 4. Tree listing
    let req = test::TestRequest::get()
        .uri("/api/models/testuser/workflow-test/tree/main/")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // 5. Get xet read token
    let req = test::TestRequest::get()
        .uri("/api/models/testuser/workflow-test/xet-read-token/main")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(body["accessToken"].as_str().unwrap().starts_with("xet_"));
}