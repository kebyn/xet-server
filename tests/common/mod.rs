//! Common test utilities for xet-server tests
//!
//! Provides helpers for creating test tokens and configurations
//! using Ed25519 authentication.

use xet_server::api::auth::{sign_xet_token, KeyPair, XetClaims, AuthVerifier};
use xet_server::config::{AuthConfig, ConversionConfig, ServerConfig, ServerSettings, StorageConfig};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

/// Test context that holds temp resources to keep them alive during tests.
///
/// The `temp_dir` field keeps the temporary directory alive, preventing
/// the temp files (including the public key PEM file) from being deleted
/// until the test completes.
#[allow(dead_code)] // Fields kept for RAII - they hold resources alive during tests
pub struct TestContext {
    pub config: ServerConfig,
    pub keypair: KeyPair,
    pub auth_verifier: AuthVerifier,
    /// Keep temp dir alive so key files don't get deleted during test
    pub temp_dir: TempDir,
}

/// Create a test token with the given scope
///
/// Returns the key pair and the signed token
#[allow(dead_code)]
pub fn test_token(scope: &str) -> (KeyPair, String) {
    let kp = KeyPair::generate();
    let kid = kp.kid();
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
        exp: now + 3600, // Valid for 1 hour
        iat: now,
        kid: kid.clone(),
        token_type: "user".to_string(),
    };

    let token = sign_xet_token(&claims, &kp).unwrap();
    (kp, token)
}

/// Create a test token with custom claims
///
/// Returns the key pair and the signed token
#[allow(dead_code)]
pub fn test_token_with_claims(claims: XetClaims) -> (KeyPair, String) {
    let kp = KeyPair::generate();
    let token = sign_xet_token(&claims, &kp).unwrap();
    (kp, token)
}

/// Create a test configuration with the given key pair
///
/// Returns a TestContext that holds the config, keypair, and temp dir.
/// The temp dir keeps the public key file alive for the duration of the test.
#[allow(dead_code)]
pub fn test_config_with_key(kp: &KeyPair) -> TestContext {
    // Create a temp directory and write public key inside it
    let temp_dir = tempfile::tempdir().unwrap();
    let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();
    let temp_path = temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
    std::fs::write(&temp_path, &public_key_pem).unwrap();

    let auth_config = AuthConfig {
        public_key_path: temp_path.to_str().unwrap().to_string(),
        trusted_kids: vec![kp.kid()],
    };

    let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();

    let config = ServerConfig {
        server: ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 8080,
            public_base_url: None,
            max_body_size_mb: 2048,
            rate_limit_rpm: 60,
        },
        storage: StorageConfig {
            backend: "local".to_string(),
            s3_bucket: None,
            s3_region: None,
            s3_endpoint: None,
            local_path: Some("./data".to_string()),
            upload_temp_dir: None,
            verify_download_integrity: false,
        },
        auth: auth_config,
        conversion: ConversionConfig::default(),
        gc: xet_server::config::GcConfig::default(),
    };

    TestContext {
        config,
        keypair: KeyPair::generate(), // New keypair for caller's use
        auth_verifier,
        temp_dir,
    }
}

/// Create a test configuration with a new generated key pair
///
/// Returns a TestContext that holds the config, keypair, and temp dir.
#[allow(dead_code)]
pub fn test_config_with_new_key() -> TestContext {
    let kp = KeyPair::generate();
    // Create a temp directory and write public key inside it
    let temp_dir = tempfile::tempdir().unwrap();
    let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();
    let temp_path = temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
    std::fs::write(&temp_path, &public_key_pem).unwrap();

    let auth_config = AuthConfig {
        public_key_path: temp_path.to_str().unwrap().to_string(),
        trusted_kids: vec![kp.kid()],
    };

    let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();

    let config = ServerConfig {
        server: ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 8080,
            public_base_url: None,
            max_body_size_mb: 2048,
            rate_limit_rpm: 60,
        },
        storage: StorageConfig {
            backend: "local".to_string(),
            s3_bucket: None,
            s3_region: None,
            s3_endpoint: None,
            local_path: Some("./data".to_string()),
            upload_temp_dir: None,
            verify_download_integrity: false,
        },
        auth: auth_config,
        conversion: ConversionConfig::default(),
        gc: xet_server::config::GcConfig::default(),
    };

    TestContext {
        config,
        keypair: kp,
        auth_verifier,
        temp_dir,
    }
}

/// Create a test token for a specific key pair (useful for testing with known keys)
#[allow(dead_code)]
pub fn test_token_for_keypair(kp: &KeyPair, scope: &str) -> String {
    let kid = kp.kid();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // I2 fix: When scope is "internal", create a proper internal token
    // that matches is_internal_token() validation (sub="hub-service", token_type="internal")
    let (sub, token_type) = if scope == "internal" {
        ("hub-service".to_string(), "internal".to_string())
    } else {
        ("test-user".to_string(), "user".to_string())
    };

    let claims = XetClaims {
        sub,
        scope: scope.to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: now + 3600,
        iat: now,
        kid,
        token_type,
    };

    sign_xet_token(&claims, kp).unwrap()
}
