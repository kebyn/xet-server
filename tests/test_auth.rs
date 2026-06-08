//! Tests for JWT authentication

use xet_server::api::auth::{create_jwt, validate_jwt, JwtClaims, extract_bearer_token, check_scope};

#[test]
fn test_jwt_create_validate() {
    let secret = "test-secret";
    let claims = JwtClaims {
        sub: "user123".to_string(),
        scope: "read write".to_string(),
        exp: 9999999999,
    };

    let token = create_jwt(&claims, secret).unwrap();
    let validated = validate_jwt(&token, secret).unwrap();

    assert_eq!(validated.sub, "user123");
    assert_eq!(validated.scope, "read write");
}

#[test]
fn test_jwt_expired() {
    let secret = "test-secret";
    let claims = JwtClaims {
        sub: "user123".to_string(),
        scope: "read".to_string(),
        exp: 1, // Long expired
    };

    let token = create_jwt(&claims, secret).unwrap();
    assert!(validate_jwt(&token, secret).is_err());
}

#[test]
fn test_jwt_invalid_signature() {
    let claims = JwtClaims {
        sub: "user123".to_string(),
        scope: "read".to_string(),
        exp: 9999999999,
    };

    let token = create_jwt(&claims, "secret1").unwrap();
    assert!(validate_jwt(&token, "secret2").is_err());
}

#[test]
fn test_extract_bearer_token() {
    let header = "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...";
    let token = extract_bearer_token(header);
    assert_eq!(token, Some("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...".to_string()));

    let invalid = "Basic dXNlcjpwYXNz";
    assert_eq!(extract_bearer_token(invalid), None);
}

#[test]
fn test_check_scope() {
    let claims = JwtClaims {
        sub: "user123".to_string(),
        scope: "read write".to_string(),
        exp: 9999999999,
    };

    assert!(check_scope(&claims, "read"));
    assert!(check_scope(&claims, "write"));
    assert!(!check_scope(&claims, "admin"));
}
