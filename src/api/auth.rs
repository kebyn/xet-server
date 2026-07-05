//! Ed25519 JWT authentication for Xet Storage server
//!
//! Uses EdDSA signing for xet tokens with the format:
//! `xet_{base64url(header).base64url(payload).base64url(signature)}`

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Error types for authentication operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Token format is invalid (not a valid JWT structure)
    InvalidToken,
    /// Token has expired (exp claim check failed)
    Expired,
    /// Signature verification failed
    InvalidSignature,
    /// Key ID (kid) not recognized
    UnknownKid,
    /// Key parsing/loading failed
    InvalidKey,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidToken => write!(f, "Invalid token format"),
            AuthError::Expired => write!(f, "Token has expired"),
            AuthError::InvalidSignature => write!(f, "Invalid signature"),
            AuthError::UnknownKid => write!(f, "Unknown key ID"),
            AuthError::InvalidKey => write!(f, "Invalid key"),
        }
    }
}

impl std::error::Error for AuthError {}

/// JWT header for xet tokens
#[derive(Debug, Serialize, Deserialize)]
struct JwtHeader {
    alg: String,
    typ: String,
    kid: String,
}

/// Claims embedded in a xet JWT token.
///
/// Defined once in the `xet-auth-types` crate and shared by both CAS and Hub so
/// the token wire format cannot drift between signer and verifier. Re-exported
/// here so existing `crate::api::auth::XetClaims` paths keep resolving.
pub use xet_auth_types::XetClaims;

pub const PROXY_TOKEN_TTL_SECONDS: u64 = 300;

#[derive(Debug, Clone)]
pub struct SignedProxyToken {
    pub token: String,
    pub exp: u64,
    pub expires_in: u64,
}

/// Ed25519 key pair for signing and verification
pub struct KeyPair {
    signing_key: SigningKey,
}

impl KeyPair {
    /// Generate a new random Ed25519 key pair
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        KeyPair { signing_key }
    }

    /// Get the verifying (public) key from this key pair
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Load a public key from PEM format (SPKI DER wrapped in PEM markers)
    pub fn public_key_from_pem(pem: &str) -> Result<VerifyingKey, AuthError> {
        use ed25519_dalek::pkcs8::DecodePublicKey;
        VerifyingKey::from_public_key_pem(pem).map_err(|_| AuthError::InvalidKey)
    }

    /// Load a private key from PEM format
    pub fn private_key_from_pem(pem: &str) -> Result<Self, AuthError> {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        let signing_key = SigningKey::from_pkcs8_pem(pem).map_err(|_| AuthError::InvalidKey)?;
        Ok(KeyPair { signing_key })
    }

    /// Export the public key to PEM format
    pub fn public_key_to_pem(verifying_key: &VerifyingKey) -> Result<String, AuthError> {
        use ed25519_dalek::pkcs8::EncodePublicKey;
        // LineEnding::LF is the standard for PEM files
        verifying_key
            .to_public_key_pem(pkcs8::LineEnding::LF)
            .map_err(|_| AuthError::InvalidKey)
    }

    /// Get a unique key ID for this key pair (first 8 bytes of public key as hex)
    pub fn kid(&self) -> String {
        let verifying_key = self.signing_key.verifying_key();
        let pk_bytes = verifying_key.as_bytes();
        hex::encode(&pk_bytes[..8])
    }
}

fn sign_claims_with_prefix(
    claims: &XetClaims,
    signing_key: &SigningKey,
    prefix: &str,
) -> Result<String, AuthError> {
    if prefix.is_empty() {
        return Err(AuthError::InvalidToken);
    }

    let header = JwtHeader {
        alg: "EdDSA".to_string(),
        typ: "JWT".to_string(),
        kid: claims.kid.clone(),
    };

    let header_json = serde_json::to_string(&header).map_err(|_| AuthError::InvalidToken)?;
    let payload_json = serde_json::to_string(claims).map_err(|_| AuthError::InvalidToken)?;

    let header_b64 = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());

    let message = format!("{}.{}", header_b64, payload_b64);
    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

    Ok(format!(
        "{}{}.{}.{}",
        prefix, header_b64, payload_b64, sig_b64
    ))
}

fn sign_claims_for_type(
    claims: &XetClaims,
    keypair: &KeyPair,
    expected_token_type: &str,
    prefix: &str,
) -> Result<String, AuthError> {
    if claims.token_type != expected_token_type {
        return Err(AuthError::InvalidToken);
    }

    sign_claims_with_prefix(claims, &keypair.signing_key, prefix)
}

/// Sign user claims with the key pair to create a xet token
///
/// The token format is: `xet_{base64url(header).base64url(payload).base64url(signature)}`
///
/// This signer is intentionally limited to user tokens. Internal and proxy
/// tokens have distinct prefixes and must not be emitted through this API.
pub fn sign_xet_token(claims: &XetClaims, keypair: &KeyPair) -> Result<String, AuthError> {
    sign_claims_for_type(claims, keypair, "user", "xet_")
}

/// Sign internal service claims with the key pair to create an internal token.
///
/// The token format is:
/// `internal_{base64url(header).base64url(payload).base64url(signature)}`.
pub fn sign_internal_token(claims: &XetClaims, keypair: &KeyPair) -> Result<String, AuthError> {
    sign_claims_for_type(claims, keypair, "internal", "internal_")
}

/// Sign proxy claims with the key pair to create a proxy token.
///
/// The token format is: `proxy_{base64url(header).base64url(payload).base64url(signature)}`.
pub fn sign_proxy_claims_token(claims: &XetClaims, keypair: &KeyPair) -> Result<String, AuthError> {
    sign_claims_for_type(claims, keypair, "proxy", "proxy_")
}

/// Verify a xet token with a specific public key and expected kid
///
/// Checks:
/// 1. Token format (xet_, internal_, or proxy_ prefix, three base64url parts)
/// 2. Signature validity
/// 3. Key ID matches expected kid
/// 4. Token has not expired
/// 5. Token prefix matches the token_type claim
pub fn verify_xet_token(
    token: &str,
    public_key: &VerifyingKey,
    expected_kid: &str,
) -> Result<XetClaims, AuthError> {
    let (token_body, expected_token_type) = if let Some(body) = token.strip_prefix("xet_") {
        (body, "user")
    } else if let Some(body) = token.strip_prefix("internal_") {
        (body, "internal")
    } else if let Some(body) = token.strip_prefix("proxy_") {
        (body, "proxy")
    } else {
        return Err(AuthError::InvalidToken);
    };

    // Split into three parts
    let parts: Vec<&str> = token_body.split('.').collect();
    if parts.len() != 3 {
        return Err(AuthError::InvalidToken);
    }

    let header_b64 = parts[0];
    let payload_b64 = parts[1];
    let sig_b64 = parts[2];

    // Decode header
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| AuthError::InvalidToken)?;
    let header: JwtHeader =
        serde_json::from_slice(&header_bytes).map_err(|_| AuthError::InvalidToken)?;

    // Verify kid matches expected
    if header.kid != expected_kid {
        return Err(AuthError::UnknownKid);
    }

    // Verify algorithm is EdDSA
    if header.alg != "EdDSA" {
        return Err(AuthError::InvalidToken);
    }

    // Decode signature
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| AuthError::InvalidToken)?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| AuthError::InvalidSignature)?;

    // Verify signature over "{header_b64}.{payload_b64}"
    let message = format!("{}.{}", header_b64, payload_b64);
    public_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| AuthError::InvalidSignature)?;

    // Decode payload
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| AuthError::InvalidToken)?;
    let claims: XetClaims =
        serde_json::from_slice(&payload_bytes).map_err(|_| AuthError::InvalidToken)?;

    if claims.kid != expected_kid {
        return Err(AuthError::UnknownKid);
    }

    if claims.token_type != expected_token_type {
        return Err(AuthError::InvalidToken);
    }

    // Check expiration
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuthError::InvalidToken)?
        .as_secs();
    if claims.exp < now {
        return Err(AuthError::Expired);
    }

    // I9 fix: Validate iat (issued-at) to limit max token lifetime.
    // If a signing key leaks, tokens with exp set far in the future are
    // still bounded by this max-age check.
    const MAX_TOKEN_LIFETIME_SECS: u64 = 7 * 24 * 3600; // 7 days
    if claims.iat > now {
        return Err(AuthError::InvalidToken); // iat in the future
    }
    if now - claims.iat > MAX_TOKEN_LIFETIME_SECS {
        return Err(AuthError::Expired); // token too old regardless of exp
    }

    // C2 fix (security): Enforce that proxy tokens can only have lfs-* scopes
    if claims.token_type == "proxy" {
        let valid_proxy_scope = claims
            .scope
            .split_whitespace()
            .all(|s| s.starts_with("lfs-"));
        if !valid_proxy_scope {
            return Err(AuthError::InvalidToken);
        }
    }

    Ok(claims)
}

/// Check whether user claims have a regular public endpoint scope.
///
/// Internal service authorization is intentionally excluded from this helper.
/// Callers that need Hub-to-CAS internal access must use [`is_internal_token`].
pub fn check_scope(claims: &XetClaims, required_scope: &str) -> bool {
    if required_scope == "internal" || claims.token_type != "user" {
        return false;
    }

    let has_internal_scope = claims.scope.split_whitespace().any(|s| s == "internal");
    if has_internal_scope {
        return false;
    }

    claims.scope.split_whitespace().any(|s| s == required_scope)
}

/// Check if claims represent an internal service token from Hub.
///
/// Internal tokens are issued by the Hub for Hub-to-CAS communication.
/// They have: sub="hub-service", scope="internal", token_type="internal".
///
/// This is a defense-in-depth check that verifies all three fields to prevent
/// a buggy/misconfigured TokenStore from accidentally creating a user token
/// with sub="hub-service" that could bypass scope checks.
pub fn is_internal_token(claims: &XetClaims) -> bool {
    let has_internal_scope = claims.scope.split_whitespace().any(|s| s == "internal");
    claims.sub == "hub-service" && has_internal_scope && claims.token_type == "internal"
}

/// Unified authorization helper for public endpoints.
///
/// Public endpoints require a user token with the requested scope.
/// Internal service tokens are intentionally handled only by `AuthNeed::Internal`
/// at explicitly internal endpoints.
pub fn authorize_endpoint(claims: &XetClaims, required_scope: &str) -> bool {
    check_scope(claims, required_scope)
}

/// Pre-loaded verification keys for authentication.
/// Created at server startup from AuthConfig to avoid per-request file I/O.
/// Holds the public key and trusted key IDs (kids) for token verification.
/// I5 fix: Optionally holds a signing key for generating proxy tokens in batch API responses.
#[derive(Clone)]
pub struct AuthVerifier {
    public_key: VerifyingKey,
    trusted_kids: Vec<String>,
    /// I5 fix: Optional signing key for generating proxy tokens.
    /// When present, batch API generates short-lived proxy tokens instead of
    /// passing through the user's long-lived token.
    signing_key: Option<SigningKey>,
    /// Kid to use when signing proxy tokens
    signing_kid: Option<String>,
}

/// 私钥文件若对 group/other 可读/可写/可执行(mode & 0o077 != 0)则视为权限过宽。
#[cfg(unix)]
fn key_permissions_too_open(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o077 != 0)
        .unwrap_or(false)
}

impl AuthVerifier {
    /// Load verification keys from AuthConfig at server startup.
    ///
    /// Reads the public key PEM file once and caches the VerifyingKey.
    /// Returns an error if the key file cannot be read or parsed.
    pub fn from_config(auth_config: &crate::config::AuthConfig) -> Result<Self, AuthError> {
        let pem_content = std::fs::read_to_string(&auth_config.public_key_path)
            .map_err(|_| AuthError::InvalidKey)?;
        let public_key = KeyPair::public_key_from_pem(&pem_content)?;

        // I5 fix: Optionally load private key for proxy token generation
        let signing_key = if let Some(ref pk_path) = auth_config.private_key_path {
            #[cfg(unix)]
            if key_permissions_too_open(std::path::Path::new(pk_path)) {
                tracing::warn!(
                    "CAS private key {} is group/other-accessible; recommend chmod 0600",
                    pk_path
                );
            }
            let pk_pem = std::fs::read_to_string(pk_path).map_err(|e| {
                tracing::error!("Failed to read CAS private key from {}: {}", pk_path, e);
                AuthError::InvalidKey
            })?;
            let keypair = KeyPair::private_key_from_pem(&pk_pem)?;
            tracing::info!(
                "CAS private key loaded from {} — proxy token generation enabled",
                pk_path
            );
            Some(keypair.signing_key)
        } else {
            tracing::warn!(
                "CAS_PRIVATE_KEY_PATH not set. Batch API will pass through user tokens. \
                Set CAS_PRIVATE_KEY_PATH for production deployments to prevent long-lived token leakage."
            );
            None
        };

        Ok(AuthVerifier {
            public_key,
            trusted_kids: auth_config.trusted_kids.clone(),
            signing_key,
            signing_kid: auth_config
                .signing_kid
                .clone()
                .or_else(|| auth_config.trusted_kids.first().cloned()),
        })
    }

    /// Verify a xet token against the cached public key and trusted kids.
    ///
    /// Tries each trusted kid until one succeeds, ensuring the token's kid
    /// matches an expected trusted kid.
    pub fn verify_token(&self, token: &str) -> Result<XetClaims, AuthError> {
        // Try each trusted kid
        for trusted_kid in &self.trusted_kids {
            if let Ok(claims) = verify_xet_token(token, &self.public_key, trusted_kid) {
                // Also verify the token's kid matches what we expect
                if claims.kid == *trusted_kid {
                    return Ok(claims);
                }
            }
        }

        Err(AuthError::UnknownKid)
    }

    /// I5 fix: Sign a short-lived proxy token for LFS operations.
    ///
    /// Returns None if signing key is not configured (CAS_PRIVATE_KEY_PATH not set).
    /// Proxy tokens are bound to a specific OID and operation, limiting blast radius.
    pub fn sign_proxy_token(
        &self,
        sub: &str,
        oid: &str,
        operation: &str,
    ) -> Option<SignedProxyToken> {
        let signing_key = self.signing_key.as_ref()?;
        let kid = self.signing_kid.as_deref().unwrap_or("cas-key");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let exp = now + PROXY_TOKEN_TTL_SECONDS;

        let claims = XetClaims {
            sub: sub.to_string(),
            scope: format!("lfs-{}", operation),
            repo_id: String::new(),
            repo_type: String::new(),
            revision: String::new(),
            exp,
            iat: now,
            kid: kid.to_string(),
            token_type: "proxy".to_string(),
            oid: Some(oid.to_string()),
            operation: Some(operation.to_string()),
        };

        Some(SignedProxyToken {
            token: sign_claims_with_prefix(&claims, signing_key, "proxy_").ok()?,
            exp,
            expires_in: PROXY_TOKEN_TTL_SECONDS,
        })
    }

    /// Check if proxy token generation is enabled
    pub fn can_sign_proxy_tokens(&self) -> bool {
        self.signing_key.is_some()
    }
}

/// Extract a bearer token from an Authorization header value.
/// Returns `Some(token)` if the header is `Bearer <token>`, `None` otherwise.
pub fn extract_bearer_token(auth_header: &str) -> Option<String> {
    auth_header.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Extract JWT token from HTTP request.
/// Supports both Bearer token and Basic auth (where password is JWT token).
/// Delegates Bearer extraction to `extract_bearer_token`.
pub fn extract_token_from_request(req: &actix_web::HttpRequest) -> Option<String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

    let auth_header = req.headers().get("Authorization")?;
    let auth_str = auth_header.to_str().ok()?;

    // Try Bearer token first (reuses extract_bearer_token)
    if let Some(token) = extract_bearer_token(auth_str) {
        return Some(token);
    }

    // Try Basic auth (username:password where password is JWT token)
    if let Some(encoded) = auth_str.strip_prefix("Basic ")
        && let Ok(decoded) = BASE64.decode(encoded)
        && let Ok(credentials) = String::from_utf8(decoded)
    {
        // Format: username:password (split only on first colon to preserve
        // passwords that may contain ':' characters)
        if let Some(password) = credentials.split_once(':').map(|x| x.1) {
            return Some(password.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_secs_for_test() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn sign_payload_for_test(
        payload: serde_json::Value,
        keypair: &KeyPair,
        kid: &str,
        prefix: &str,
    ) -> String {
        let header = JwtHeader {
            alg: "EdDSA".to_string(),
            typ: "JWT".to_string(),
            kid: kid.to_string(),
        };
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let message = format!("{}.{}", header_b64, payload_b64);
        let signature = keypair.signing_key.sign(message.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        format!("{}{}.{}.{}", prefix, header_b64, payload_b64, sig_b64)
    }

    fn claims_for_test(kid: &str, scope: &str, token_type: &str) -> XetClaims {
        let now = now_secs_for_test();
        XetClaims {
            sub: "test-user".to_string(),
            scope: scope.to_string(),
            repo_id: "test/repo".to_string(),
            repo_type: "model".to_string(),
            revision: "main".to_string(),
            exp: now + 3600,
            iat: now,
            kid: kid.to_string(),
            token_type: token_type.to_string(),
            oid: None,
            operation: None,
        }
    }

    #[test]
    fn test_verify_rejects_missing_token_type() {
        let keypair = KeyPair::generate();
        let kid = keypair.kid();
        let now = now_secs_for_test();
        let payload = serde_json::json!({
            "sub": "test-user",
            "scope": "read",
            "repo_id": "test/repo",
            "repo_type": "model",
            "revision": "main",
            "exp": now + 3600,
            "iat": now,
            "kid": kid,
        });
        let token = sign_payload_for_test(payload, &keypair, &kid, "xet_");

        let result = verify_xet_token(&token, &keypair.verifying_key(), &kid);
        assert_eq!(result, Err(AuthError::InvalidToken));
    }

    #[test]
    fn test_verify_rejects_internal_token_with_xet_prefix() {
        let keypair = KeyPair::generate();
        let kid = keypair.kid();
        let claims = claims_for_test(&kid, "internal", "internal");
        let token = sign_payload_for_test(
            serde_json::to_value(&claims).unwrap(),
            &keypair,
            &kid,
            "xet_",
        );

        let result = verify_xet_token(&token, &keypair.verifying_key(), &kid);
        assert_eq!(result, Err(AuthError::InvalidToken));
    }

    #[test]
    fn test_verify_rejects_proxy_token_with_xet_prefix() {
        let keypair = KeyPair::generate();
        let kid = keypair.kid();
        let mut claims = claims_for_test(&kid, "lfs-download", "proxy");
        claims.oid = Some("a".repeat(64));
        claims.operation = Some("download".to_string());
        let token = sign_payload_for_test(
            serde_json::to_value(&claims).unwrap(),
            &keypair,
            &kid,
            "xet_",
        );

        let result = verify_xet_token(&token, &keypair.verifying_key(), &kid);
        assert_eq!(result, Err(AuthError::InvalidToken));
    }

    #[test]
    fn test_verify_rejects_payload_kid_mismatch() {
        let keypair = KeyPair::generate();
        let kid = keypair.kid();
        let claims = claims_for_test("payload-kid", "read", "user");
        let token = sign_payload_for_test(
            serde_json::to_value(&claims).unwrap(),
            &keypair,
            &kid,
            "xet_",
        );

        let result = verify_xet_token(&token, &keypair.verifying_key(), &kid);
        assert_eq!(result, Err(AuthError::UnknownKid));
    }

    #[test]
    fn test_sign_xet_token_rejects_internal_claims() {
        let keypair = KeyPair::generate();
        let kid = keypair.kid();
        let claims = claims_for_test(&kid, "internal", "internal");

        let result = sign_xet_token(&claims, &keypair);

        assert_eq!(result, Err(AuthError::InvalidToken));
    }

    #[test]
    fn test_sign_xet_token_rejects_proxy_claims() {
        let keypair = KeyPair::generate();
        let kid = keypair.kid();
        let mut claims = claims_for_test(&kid, "lfs-download", "proxy");
        claims.oid = Some("a".repeat(64));
        claims.operation = Some("download".to_string());

        let result = sign_xet_token(&claims, &keypair);

        assert_eq!(result, Err(AuthError::InvalidToken));
    }

    #[cfg(unix)]
    #[test]
    fn test_key_permissions_detection() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("key.pem");
        std::fs::write(&p, b"x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!key_permissions_too_open(&p));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(key_permissions_too_open(&p));
    }

    #[test]
    fn test_sign_proxy_token_returns_token_and_expiry_data() {
        let keypair = KeyPair::generate();
        let public_key = keypair.verifying_key();
        let kid = "test-kid".to_string();
        let verifier = AuthVerifier {
            public_key,
            trusted_kids: vec![kid.clone()],
            signing_key: Some(keypair.signing_key),
            signing_kid: Some(kid.clone()),
        };
        let oid = "a".repeat(64);

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let signed = verifier
            .sign_proxy_token("alice", &oid, "download")
            .expect("proxy token should be signed");
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert!(signed.token.starts_with("proxy_"));
        assert_eq!(signed.expires_in, PROXY_TOKEN_TTL_SECONDS);
        assert!(signed.exp >= before + PROXY_TOKEN_TTL_SECONDS);
        assert!(signed.exp <= after + PROXY_TOKEN_TTL_SECONDS);

        let claims = verifier.verify_token(&signed.token).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.scope, "lfs-download");
        assert_eq!(claims.kid, kid);
        assert_eq!(claims.token_type, "proxy");
        assert_eq!(claims.oid.as_deref(), Some(oid.as_str()));
        assert_eq!(claims.operation.as_deref(), Some("download"));
        assert_eq!(claims.exp, signed.exp);
    }

    #[test]
    fn test_sign_proxy_token_without_signing_key_returns_none() {
        let keypair = KeyPair::generate();
        let verifier = AuthVerifier {
            public_key: keypair.verifying_key(),
            trusted_kids: vec!["test-kid".to_string()],
            signing_key: None,
            signing_kid: Some("test-kid".to_string()),
        };

        assert!(
            verifier
                .sign_proxy_token("alice", &"a".repeat(64), "download")
                .is_none()
        );
    }
}
