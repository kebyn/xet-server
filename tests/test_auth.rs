//! Tests for Ed25519 JWT authentication

use std::time::{SystemTime, UNIX_EPOCH};
use xet_server::api::auth::{
    AuthError, KeyPair, XetClaims, authorize_endpoint, check_scope, extract_bearer_token,
    sign_internal_token, sign_xet_token, verify_xet_token,
};

fn create_test_claims(kid: &str, scope: &str) -> XetClaims {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    XetClaims {
        sub: "test-user".to_string(),
        scope: scope.to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: now + 3600, // Valid for 1 hour
        iat: now,
        kid: kid.to_string(),
        token_type: "user".to_string(),
        oid: None,
        operation: None,
    }
}

#[test]
fn test_keypair_generation() {
    let kp = KeyPair::generate();
    let kid = kp.kid();

    // kid should be 16 hex chars (8 bytes)
    assert_eq!(kid.len(), 16);
    assert!(kid.chars().all(|c| c.is_ascii_hexdigit()));

    // Each generated key should have unique kid
    let kp2 = KeyPair::generate();
    assert_ne!(kp.kid(), kp2.kid());
}

#[test]
fn test_public_key_pem_export_import() {
    let kp = KeyPair::generate();
    let pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();

    // PEM should have correct markers
    assert!(pem.contains("-----BEGIN PUBLIC KEY-----"));
    assert!(pem.contains("-----END PUBLIC KEY-----"));

    // Should be able to re-import
    let imported = KeyPair::public_key_from_pem(&pem).unwrap();
    assert_eq!(kp.verifying_key(), imported);
}

#[test]
fn test_sign_and_verify_xet_token() {
    let kp = KeyPair::generate();
    let kid = kp.kid();
    let claims = create_test_claims(&kid, "read write");

    let token = sign_xet_token(&claims, &kp).unwrap();

    // Token should have xet_ prefix
    assert!(token.starts_with("xet_"));

    // Should verify successfully
    let verified = verify_xet_token(&token, &kp.verifying_key(), &kid).unwrap();
    assert_eq!(verified.sub, "test-user");
    assert_eq!(verified.scope, "read write");
    assert_eq!(verified.kid, kid);
}

#[test]
fn test_verify_expired_token() {
    let kp = KeyPair::generate();
    let kid = kp.kid();

    let claims = XetClaims {
        sub: "test-user".to_string(),
        scope: "read".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 1, // Expired
        iat: 1,
        kid: kid.to_string(),
        token_type: "user".to_string(),
        oid: None,
        operation: None,
    };

    let token = sign_xet_token(&claims, &kp).unwrap();

    let result = verify_xet_token(&token, &kp.verifying_key(), &kid);
    assert_eq!(result, Err(AuthError::Expired));
}

#[test]
fn test_verify_invalid_signature() {
    let kp = KeyPair::generate();
    let kp2 = KeyPair::generate(); // Different key
    let kid = kp.kid();
    let claims = create_test_claims(&kid, "read");

    // Sign with kp but verify with kp2's public key
    let token = sign_xet_token(&claims, &kp).unwrap();

    let result = verify_xet_token(&token, &kp2.verifying_key(), &kid);
    assert_eq!(result, Err(AuthError::InvalidSignature));
}

#[test]
fn test_verify_unknown_kid() {
    let kp = KeyPair::generate();
    let kid = kp.kid();
    let claims = create_test_claims(&kid, "read");

    let token = sign_xet_token(&claims, &kp).unwrap();

    // Try to verify with wrong expected kid
    let result = verify_xet_token(&token, &kp.verifying_key(), "wrong-kid");
    assert_eq!(result, Err(AuthError::UnknownKid));
}

#[test]
fn test_verify_invalid_token_format() {
    let kp = KeyPair::generate();
    let kid = kp.kid();

    // No xet_ prefix
    let result = verify_xet_token("not.xet.token", &kp.verifying_key(), &kid);
    assert_eq!(result, Err(AuthError::InvalidToken));

    // Wrong number of parts
    let result = verify_xet_token("xet_only_one_part", &kp.verifying_key(), &kid);
    assert_eq!(result, Err(AuthError::InvalidToken));

    // Invalid base64
    let result = verify_xet_token("xet_invalid!@#.parts.here", &kp.verifying_key(), &kid);
    assert_eq!(result, Err(AuthError::InvalidToken));
}

#[test]
fn test_extract_bearer_token() {
    let header = "Bearer xet_eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...";
    let token = extract_bearer_token(header);
    assert_eq!(
        token,
        Some("xet_eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...".to_string())
    );

    let invalid = "Basic dXNlcjpwYXNz";
    assert_eq!(extract_bearer_token(invalid), None);

    let no_prefix = "just_a_token";
    assert_eq!(extract_bearer_token(no_prefix), None);
}

#[test]
fn test_check_scope() {
    let claims = XetClaims {
        sub: "user123".to_string(),
        scope: "read write".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 9999999999 - 3600,
        kid: "test-kid".to_string(),
        token_type: "user".to_string(),
        oid: None,
        operation: None,
    };

    assert!(check_scope(&claims, "read"));
    assert!(check_scope(&claims, "write"));
    assert!(!check_scope(&claims, "admin"));
}

#[test]
fn test_check_scope_internal_restricted() {
    let claims = XetClaims {
        sub: "internal-user".to_string(),
        scope: "internal".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 9999999999 - 3600,
        kid: "test-kid".to_string(),
        token_type: "internal".to_string(),
        oid: None,
        operation: None,
    };

    // "internal" is not a regular endpoint scope.
    assert!(!check_scope(&claims, "internal"));
    assert!(!authorize_endpoint(&claims, "internal"));
    // Internal tokens are rejected for non-internal endpoints (least privilege)
    assert!(!check_scope(&claims, "read"));
    assert!(!check_scope(&claims, "write"));
    assert!(!check_scope(&claims, "admin"));
}

#[test]
fn test_authorize_endpoint_rejects_malformed_internal_token_for_regular_scope() {
    let claims = XetClaims {
        sub: "test-user".to_string(),
        scope: "read".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 9999999999 - 3600,
        kid: "test-kid".to_string(),
        token_type: "internal".to_string(),
        oid: None,
        operation: None,
    };

    assert!(!authorize_endpoint(&claims, "read"));
}

#[test]
fn test_authorize_endpoint_rejects_proxy_token_for_regular_scope() {
    let claims = XetClaims {
        sub: "test-user".to_string(),
        scope: "read".to_string(),
        repo_id: "test/repo".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: 9999999999,
        iat: 9999999999 - 3600,
        kid: "test-kid".to_string(),
        token_type: "proxy".to_string(),
        oid: Some("a".repeat(64)),
        operation: Some("download".to_string()),
    };

    assert!(!check_scope(&claims, "read"));
    assert!(!authorize_endpoint(&claims, "read"));
}

#[test]
fn test_authorize_endpoint_rejects_real_internal_token_for_regular_scope() {
    let claims = XetClaims {
        sub: "hub-service".to_string(),
        scope: "internal".to_string(),
        repo_id: "*".to_string(),
        repo_type: "*".to_string(),
        revision: "*".to_string(),
        exp: 9999999999,
        iat: 9999999999 - 3600,
        kid: "test-kid".to_string(),
        token_type: "internal".to_string(),
        oid: None,
        operation: None,
    };

    assert!(!check_scope(&claims, "internal"));
    assert!(!authorize_endpoint(&claims, "internal"));
    assert!(!authorize_endpoint(&claims, "read"));
    assert!(!authorize_endpoint(&claims, "write"));
}

#[test]
fn test_sign_and_verify_internal_token() {
    let kp = KeyPair::generate();
    let kid = kp.kid();
    let claims = XetClaims {
        sub: "hub-service".to_string(),
        scope: "internal".to_string(),
        repo_id: "*".to_string(),
        repo_type: "*".to_string(),
        revision: "*".to_string(),
        exp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600,
        iat: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        kid: kid.clone(),
        token_type: "internal".to_string(),
        oid: None,
        operation: None,
    };

    let token = sign_internal_token(&claims, &kp).unwrap();
    assert!(token.starts_with("internal_"));

    let verified = verify_xet_token(&token, &kp.verifying_key(), &kid).unwrap();
    assert_eq!(verified.token_type, "internal");
    assert_eq!(verified.scope, "internal");
}

#[test]
fn test_token_with_multiple_scopes() {
    let kp = KeyPair::generate();
    let kid = kp.kid();
    let claims = create_test_claims(&kid, "read write admin");

    let token = sign_xet_token(&claims, &kp).unwrap();
    let verified = verify_xet_token(&token, &kp.verifying_key(), &kid).unwrap();

    assert!(check_scope(&verified, "read"));
    assert!(check_scope(&verified, "write"));
    assert!(check_scope(&verified, "admin"));
    assert!(!check_scope(&verified, "delete"));
}
