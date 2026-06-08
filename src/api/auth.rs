//! JWT authentication for Xet Storage server

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

pub fn extract_bearer_token(auth_header: &str) -> Option<String> {
    auth_header
        .strip_prefix("Bearer ")
        .map(|s| s.to_string())
}

pub fn check_scope(claims: &JwtClaims, required_scope: &str) -> bool {
    claims.scope.split_whitespace().any(|s| s == required_scope)
}
