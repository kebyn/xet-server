//! Shared authentication claim types for the Xet workspace.
//!
//! `XetClaims` is the wire format for Ed25519 JWT tokens exchanged between the
//! Hub (`hub-api`, which signs tokens) and the CAS server (`xet-server`, which
//! verifies them). Both crates depend on this crate so the struct is defined
//! exactly once — keeping the serialized token format authoritative and
//! preventing silent wire-compatibility drift.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Claims embedded in a xet JWT token.
///
/// The serialized form of this struct is the on-the-wire token payload shared
/// between Hub and CAS. Field names and serde annotations are part of the wire
/// contract — do not change them without versioning the token format.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct XetClaims {
    /// Subject (user identity)
    pub sub: String,
    /// Scope(s) granted (space-separated, e.g., "read write")
    pub scope: String,
    /// Repository ID (HuggingFace-style repo identifier)
    pub repo_id: String,
    /// Repository type (e.g., "model", "dataset", "space")
    pub repo_type: String,
    /// Git revision being accessed
    pub revision: String,
    /// Expiration timestamp (Unix seconds)
    pub exp: u64,
    /// Issued-at timestamp (Unix seconds)
    pub iat: u64,
    /// Key ID identifying the signing key
    pub kid: String,
    /// Token type: "user", "proxy", or "internal"
    /// Used for defense-in-depth to distinguish internal service tokens.
    pub token_type: String,
    /// LFS object ID (for proxy tokens, binds token to specific object)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// LFS operation: "upload" or "download" (for proxy tokens)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    User,
    Proxy,
    Internal,
}

impl TokenKind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::User => "xet_",
            Self::Proxy => "proxy_",
            Self::Internal => "internal_",
        }
    }

    pub fn token_type(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Proxy => "proxy",
            Self::Internal => "internal",
        }
    }

    fn from_prefix(token: &str) -> Option<(Self, &str)> {
        for kind in [Self::User, Self::Proxy, Self::Internal] {
            if let Some(body) = token.strip_prefix(kind.prefix()) {
                return Some((kind, body));
            }
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenWireError {
    InvalidToken,
    Expired,
    InvalidSignature,
    UnknownKid,
}

#[derive(Debug, Serialize, Deserialize)]
struct JwtHeader {
    alg: String,
    typ: String,
    kid: String,
}

pub fn sign_claims(
    claims: &XetClaims,
    signing_key: &SigningKey,
    kind: TokenKind,
) -> Result<String, TokenWireError> {
    if claims.token_type != kind.token_type() {
        return Err(TokenWireError::InvalidToken);
    }

    let header = JwtHeader {
        alg: "EdDSA".to_string(),
        typ: "JWT".to_string(),
        kid: claims.kid.clone(),
    };

    let header_b64 = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&header).map_err(|_| TokenWireError::InvalidToken)?);
    let claims_b64 = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).map_err(|_| TokenWireError::InvalidToken)?);
    let signing_input = format!("{}.{}", header_b64, claims_b64);
    let signature = signing_key.sign(signing_input.as_bytes());

    Ok(format!(
        "{}{}.{}",
        kind.prefix(),
        signing_input,
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

pub fn verify_token(
    token: &str,
    public_key: &VerifyingKey,
    expected_kid: &str,
    expected_kind: TokenKind,
) -> Result<XetClaims, TokenWireError> {
    let (kind, claims) = verify_token_any_kind(token, public_key, expected_kid)?;
    if kind != expected_kind {
        return Err(TokenWireError::InvalidToken);
    }
    Ok(claims)
}

pub fn verify_token_any_kind(
    token: &str,
    public_key: &VerifyingKey,
    expected_kid: &str,
) -> Result<(TokenKind, XetClaims), TokenWireError> {
    let (kind, token_body) = TokenKind::from_prefix(token).ok_or(TokenWireError::InvalidToken)?;
    let parts: Vec<&str> = token_body.split('.').collect();
    if parts.len() != 3 {
        return Err(TokenWireError::InvalidToken);
    }

    let header_bytes = URL_SAFE_NO_PAD
        .decode(parts[0])
        .map_err(|_| TokenWireError::InvalidToken)?;
    let header: JwtHeader =
        serde_json::from_slice(&header_bytes).map_err(|_| TokenWireError::InvalidToken)?;
    if header.alg != "EdDSA" || header.typ != "JWT" {
        return Err(TokenWireError::InvalidToken);
    }
    if header.kid != expected_kid {
        return Err(TokenWireError::UnknownKid);
    }

    let signature_bytes = URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| TokenWireError::InvalidToken)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| TokenWireError::InvalidSignature)?;
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    public_key
        .verify(signing_input.as_bytes(), &signature)
        .map_err(|_| TokenWireError::InvalidSignature)?;

    let claims_bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| TokenWireError::InvalidToken)?;
    let claims: XetClaims =
        serde_json::from_slice(&claims_bytes).map_err(|_| TokenWireError::InvalidToken)?;
    if claims.kid != expected_kid {
        return Err(TokenWireError::UnknownKid);
    }
    if claims.token_type != kind.token_type() {
        return Err(TokenWireError::InvalidToken);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TokenWireError::InvalidToken)?
        .as_secs();
    if claims.exp < now {
        return Err(TokenWireError::Expired);
    }

    const MAX_TOKEN_LIFETIME_SECS: u64 = 7 * 24 * 3600;
    if claims.iat > now {
        return Err(TokenWireError::InvalidToken);
    }
    if now - claims.iat > MAX_TOKEN_LIFETIME_SECS {
        return Err(TokenWireError::Expired);
    }

    if kind == TokenKind::Proxy {
        let valid_proxy_scope = claims
            .scope
            .split_whitespace()
            .all(|scope| scope.starts_with("lfs-"));
        if !valid_proxy_scope {
            return Err(TokenWireError::InvalidToken);
        }
    }

    Ok((kind, claims))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn claims(kind: TokenKind, kid: &str) -> XetClaims {
        XetClaims {
            sub: "alice".to_string(),
            scope: match kind {
                TokenKind::User => "read".to_string(),
                TokenKind::Proxy => "lfs-download".to_string(),
                TokenKind::Internal => "internal".to_string(),
            },
            repo_id: "alice/model".to_string(),
            repo_type: "model".to_string(),
            revision: "main".to_string(),
            exp: now_secs() + 3600,
            iat: now_secs(),
            kid: kid.to_string(),
            token_type: kind.token_type().to_string(),
            oid: (kind == TokenKind::Proxy).then(|| "a".repeat(64)),
            operation: (kind == TokenKind::Proxy).then(|| "download".to_string()),
        }
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn sign_with_header_alg(signing_key: &SigningKey, claims: &XetClaims, alg: &str) -> String {
        let header = serde_json::json!({
            "alg": alg,
            "typ": "JWT",
            "kid": claims.kid,
        });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let input = format!("{}.{}", header_b64, claims_b64);
        let sig = signing_key.sign(input.as_bytes());
        format!(
            "{}{}.{}",
            TokenKind::User.prefix(),
            input,
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        )
    }

    #[test]
    fn signed_user_token_verifies_with_expected_kind() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let kid = "kid-1";
        let claims = claims(TokenKind::User, kid);

        let token = sign_claims(&claims, &signing_key, TokenKind::User).unwrap();
        let verified =
            verify_token(&token, &signing_key.verifying_key(), kid, TokenKind::User).unwrap();

        assert_eq!(verified, claims);
    }

    #[test]
    fn token_kind_mismatch_is_rejected() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let kid = "kid-1";
        let claims = claims(TokenKind::User, kid);

        let token = sign_claims(&claims, &signing_key, TokenKind::User).unwrap();
        let err = verify_token(&token, &signing_key.verifying_key(), kid, TokenKind::Proxy)
            .expect_err("user token must not verify as proxy");

        assert_eq!(err, TokenWireError::InvalidToken);
    }

    #[test]
    fn any_kind_verification_returns_detected_kind() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let kid = "kid-1";
        let claims = claims(TokenKind::Internal, kid);

        let token = sign_claims(&claims, &signing_key, TokenKind::Internal).unwrap();
        let (kind, verified) =
            verify_token_any_kind(&token, &signing_key.verifying_key(), kid).unwrap();

        assert_eq!(kind, TokenKind::Internal);
        assert_eq!(verified, claims);
    }

    #[test]
    fn header_algorithm_mismatch_is_rejected() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let kid = "kid-1";
        let claims = claims(TokenKind::User, kid);
        let token = sign_with_header_alg(&signing_key, &claims, "none");

        let err = verify_token(&token, &signing_key.verifying_key(), kid, TokenKind::User)
            .expect_err("non-EdDSA token must be rejected");

        assert_eq!(err, TokenWireError::InvalidToken);
    }
}
