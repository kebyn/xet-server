//! Integration tests for download byte tracking

use actix_web::{test, web, App};
use xet_server::api::reconstruction::get_reconstruction;
use xet_server::api::auth::{AuthVerifier, KeyPair, XetClaims, sign_xet_token};
use xet_server::config::{ServerConfig, AuthConfig};
use xet_server::storage::local::LocalStorage;
use xet_server::index::MetadataIndex;
use xet_server::metrics::GLOBAL_METRICS;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;
use serial_test::serial;

fn create_test_auth() -> (KeyPair, AuthVerifier) {
    let kp = KeyPair::generate();
    let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();

    let temp_dir = tempdir().unwrap();
    let temp_path = temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
    std::fs::write(&temp_path, &public_key_pem).unwrap();

    let temp_path_str = temp_path.to_str().unwrap().to_string();
    std::mem::forget(temp_dir);

    let auth_config = AuthConfig {
        public_key_path: temp_path_str,
        trusted_kids: vec![kp.kid()],
    };

    let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();
    (kp, auth_verifier)
}

fn create_test_token(kp: &KeyPair, scope: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = XetClaims {
        sub: "test-user".to_string(),
        scope: scope.to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: now + 3600,
        iat: now,
        kid: kp.kid(),
    };

    sign_xet_token(&claims, kp).unwrap()
}

#[actix_web::test]
#[serial]
async fn test_v2_reconstruction_tracks_download_bytes() {
    let dir = tempdir().unwrap();
    let storage: Box<dyn xet_server::storage::StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let config = ServerConfig::default();
    let (kp, auth) = create_test_auth();
    let token = create_test_token(&kp, "read");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(auth))
            .route("/v2/reconstructions/{file_id}", web::get().to(get_reconstruction))
    ).await;

    // Record initial download bytes
    let initial_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);

    // Request non-existent file (should not increment download bytes)
    let file_id = "a".repeat(64);
    let req = test::TestRequest::get()
        .uri(&format!("/v2/reconstructions/{}", file_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);

    // Download bytes should not have increased (error case)
    let final_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);
    assert_eq!(final_bytes, initial_bytes, "Download bytes should not increment on 404");
}

#[actix_web::test]
#[serial]
async fn test_v1_reconstruction_tracks_download_bytes() {
    use xet_server::api::reconstruction::get_reconstruction_v1;

    let dir = tempdir().unwrap();
    let storage: Box<dyn xet_server::storage::StorageBackend> = Box::new(
        LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
    );

    let index = MetadataIndex::new();
    let config = ServerConfig::default();
    let (kp, auth) = create_test_auth();
    let token = create_test_token(&kp, "read");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(index))
            .app_data(web::Data::new(storage))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(auth))
            .route("/v1/reconstructions/{file_id}", web::get().to(get_reconstruction_v1))
    ).await;

    // Record initial download bytes
    let initial_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);

    // Request non-existent file (should not increment download bytes)
    let file_id = "a".repeat(64);
    let req = test::TestRequest::get()
        .uri(&format!("/v1/reconstructions/{}", file_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);

    // Download bytes should not have increased (error case)
    let final_bytes = GLOBAL_METRICS.download_bytes.load(Ordering::Relaxed);
    assert_eq!(final_bytes, initial_bytes, "Download bytes should not increment on 404");
}
