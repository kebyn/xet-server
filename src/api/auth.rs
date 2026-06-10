//! JWT authentication for Xet Storage server

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JwtClaims {
    pub sub: String,
    pub scope: String,
    pub exp: usize,
}

pub fn create_jwt(claims: &JwtClaims, secret: &str) -> Result<String, jsonwebtoken::errors::Error> {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

pub fn validate_jwt(token: &str, secret: &str) -> Result<JwtClaims, jsonwebtoken::errors::Error> {
    let token_data = decode::<JwtClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )?;

    Ok(token_data.claims)
}

/// Extract a bearer token from an Authorization header value.
/// Returns `Some(token)` if the header is `Bearer <token>`, `None` otherwise.
pub fn extract_bearer_token(auth_header: &str) -> Option<String> {
    auth_header
        .strip_prefix("Bearer ")
        .map(|s| s.to_string())
}

/// Extract JWT token from HTTP request.
/// Supports both Bearer token and Basic auth (where password is JWT token).
/// Delegates Bearer extraction to `extract_bearer_token`.
pub fn extract_token_from_request(req: &actix_web::HttpRequest) -> Option<String> {
    let auth_header = req.headers().get("Authorization")?;
    let auth_str = auth_header.to_str().ok()?;

    // Try Bearer token first (reuses extract_bearer_token)
    if let Some(token) = extract_bearer_token(auth_str) {
        return Some(token);
    }

    // Try Basic auth (username:password where password is JWT token)
    if let Some(encoded) = auth_str.strip_prefix("Basic ") {
        if let Ok(decoded) = BASE64.decode(encoded) {
            if let Ok(credentials) = String::from_utf8(decoded) {
                // Format: username:password (split only on first colon to preserve
                // passwords that may contain ':' characters)
                if let Some(password) = credentials.splitn(2, ':').nth(1) {
                    return Some(password.to_string());
                }
            }
        }
    }

    None
}

pub fn check_scope(claims: &JwtClaims, required_scope: &str) -> bool {
    claims.scope.split_whitespace().any(|s| s == required_scope)
}
