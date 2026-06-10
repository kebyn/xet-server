//! Common test utilities for xet-server tests
//!
//! Provides helpers for creating test tokens and configurations
//! using Ed25519 authentication.

use xet_server::api::auth::{sign_xet_token, KeyPair, XetClaims};
use xet_server::config::{AuthConfig, ServerConfig, ServerSettings, StateConfig, StorageConfig};
use std::time::{SystemTime, UNIX_EPOCH};

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
        .as_secs() as usize;

    let claims = XetClaims {
        sub: "test-user".to_string(),
        scope: scope.to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: now + 3600, // Valid for 1 hour
        iat: now,
        kid: kid.clone(),
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
/// Writes the public key to a persistent temp file and sets up the auth config
#[allow(dead_code)]
pub fn test_config_with_key(kp: &KeyPair) -> ServerConfig {
    // Write public key to a persistent temp file (using /tmp with unique name)
    let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();
    let temp_path = format!("/tmp/xet-test-pubkey-{}.pem", kp.kid());
    std::fs::write(&temp_path, &public_key_pem).unwrap();

    ServerConfig {
        server: ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 8080,
            public_base_url: None,
            max_body_size_mb: 2048,
        },
        storage: StorageConfig {
            backend: "local".to_string(),
            s3_bucket: None,
            s3_region: None,
            s3_endpoint: None,
            local_path: Some("./data".to_string()),
            upload_temp_dir: None,
        },
        auth: AuthConfig {
            public_key_path: temp_path,
            trusted_kids: vec![kp.kid()],
            token_prefix: "xet_".to_string(),
        },
        state: StateConfig {
            sqlite_path: "/tmp/xet-test-state.db".to_string(),
        },
    }
}

/// Create a test configuration with a new generated key pair
///
/// Returns both the key pair and the config
#[allow(dead_code)]
pub fn test_config_with_new_key() -> (KeyPair, ServerConfig) {
    let kp = KeyPair::generate();
    let config = test_config_with_key(&kp);
    (kp, config)
}

/// Create a test token for a specific key pair (useful for testing with known keys)
#[allow(dead_code)]
pub fn test_token_for_keypair(kp: &KeyPair, scope: &str) -> String {
    let kid = kp.kid();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize;

    let claims = XetClaims {
        sub: "test-user".to_string(),
        scope: scope.to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: now + 3600,
        iat: now,
        kid,
    };

    sign_xet_token(&claims, kp).unwrap()
}