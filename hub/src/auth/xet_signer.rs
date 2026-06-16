use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// JWT header for Xet tokens
#[derive(Debug, Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
    kid: String,
}

/// Claims for Xet access tokens.
///
/// Defined once in the `xet-auth-types` crate and shared by both Hub and CAS so
/// the token wire format cannot drift between signer and verifier. Re-exported
/// here so existing `xet_signer::XetClaims` paths keep resolving.
pub use xet_auth_types::XetClaims;

/// Xet token signer for creating access tokens for CAS
pub struct XetSigner {
    signing_key: SigningKey,
    kid: String,
    ttl_seconds: u64,
    proxy_ttl_seconds: u64,
    /// C1 fix: Configurable TTL for internal tokens (Hub-to-CAS communication).
    /// Default: 86400 seconds (24 hours). GC runs hourly, so 60s TTL was too short.
    internal_ttl_seconds: u64,
}

impl XetSigner {
    /// Create a new XetSigner from a PEM-encoded private key
    pub fn from_pem(pem_bytes: &[u8], kid: &str, ttl_seconds: u64, proxy_ttl_seconds: u64) -> Result<Self, String> {
        Self::from_pem_with_internal_ttl(pem_bytes, kid, ttl_seconds, proxy_ttl_seconds, 86400)
    }

    /// Create a new XetSigner from a PEM-encoded private key with configurable internal token TTL.
    /// C1 fix: Allows GC internal tokens to have longer TTL (default 24 hours).
    pub fn from_pem_with_internal_ttl(pem_bytes: &[u8], kid: &str, ttl_seconds: u64, proxy_ttl_seconds: u64, internal_ttl_seconds: u64) -> Result<Self, String> {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        let pem_str = std::str::from_utf8(pem_bytes).map_err(|e| e.to_string())?;
        let signing_key = SigningKey::from_pkcs8_pem(pem_str)
            .map_err(|e| format!("Failed to load private key: {}", e))?;
        Ok(Self {
            signing_key,
            kid: kid.to_string(),
            ttl_seconds,
            proxy_ttl_seconds,
            internal_ttl_seconds,
        })
    }

    /// Create a new XetSigner from a raw signing key (for testing)
    pub fn new(signing_key: SigningKey, kid: &str, ttl_seconds: u64, proxy_ttl_seconds: u64) -> Self {
        Self {
            signing_key,
            kid: kid.to_string(),
            ttl_seconds,
            proxy_ttl_seconds,
            internal_ttl_seconds: 86400, // Default: 24 hours
        }
    }

    /// Create a new XetSigner from a raw signing key with configurable internal token TTL.
    /// C1 fix: Allows GC internal tokens to have longer TTL.
    pub fn new_with_internal_ttl(signing_key: SigningKey, kid: &str, ttl_seconds: u64, proxy_ttl_seconds: u64, internal_ttl_seconds: u64) -> Self {
        Self {
            signing_key,
            kid: kid.to_string(),
            ttl_seconds,
            proxy_ttl_seconds,
            internal_ttl_seconds,
        }
    }

    /// Internal helper to sign claims and produce a token
    /// Returns (token, expiration_timestamp)
    /// I1 fix: Return Result to propagate serialization errors instead of panicking
    fn sign_claims(&self, claims: XetClaims, prefix: &str) -> Result<(String, u64), String> {
        let exp = claims.exp;

        let header = JwtHeader {
            alg: "EdDSA",
            typ: "JWT",
            kid: self.kid.clone(),
        };

        // I1 fix: Use map_err instead of unwrap() to avoid panic on serialization failure
        let header_b64 = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&header).map_err(|e| format!("Failed to serialize header: {}", e))?
        );
        let claims_b64 = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&claims).map_err(|e| format!("Failed to serialize claims: {}", e))?
        );

        let signing_input = format!("{}.{}", header_b64, claims_b64);
        let signature = self.signing_key.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        Ok((format!("{}{}.{}", prefix, signing_input, sig_b64), exp))
    }

    /// Sign and create a Xet access token
    /// Returns (token, expiration_timestamp)
    /// I1 fix: Use unwrap_or_default() for system time to avoid panic on clock issues
    /// I2 fix: Return Result to propagate serialization errors instead of returning empty string
    pub fn sign(&self, sub: &str, scope: &str, repo_id: &str, repo_type: &str, revision: &str) -> Result<(String, u64), String> {
        // M4 fix: Reject signing if system clock is broken instead of using 0.
        // With iat=0, the verifier's max-age check would reject the token anyway,
        // but failing fast here gives a clearer error.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| "System clock is before UNIX_EPOCH, cannot sign tokens".to_string())?;
        let exp = now + self.ttl_seconds;

        let claims = XetClaims {
            sub: sub.to_string(),
            scope: scope.to_string(),
            repo_id: repo_id.to_string(),
            repo_type: repo_type.to_string(),
            revision: revision.to_string(),
            exp,
            iat: now,
            kid: self.kid.clone(),
            token_type: "user".to_string(),
            oid: None,
            operation: None,
        };

        self.sign_claims(claims, "xet_")
    }

    /// Sign and create a short-lived proxy token for LFS operations
    /// Proxy tokens are bound to a specific OID, operation (upload/download), and repository
    /// Returns (token, expiration_timestamp)
    /// I1 fix: Use unwrap_or_default() for system time to avoid panic on clock issues
    /// I2 fix: Return Result to propagate serialization errors instead of returning empty string
    pub fn sign_proxy(&self, sub: &str, oid: &str, operation: &str, repo_id: &str, repo_type: &str) -> Result<(String, u64), String> {
        // M4 fix: Reject signing if system clock is broken
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| "System clock is before UNIX_EPOCH, cannot sign tokens".to_string())?;
        // Proxy tokens use configurable TTL (default 300s / 5 minutes)
        let exp = now + self.proxy_ttl_seconds;

        let claims = XetClaims {
            sub: sub.to_string(),
            scope: format!("lfs-{}", operation),
            repo_id: repo_id.to_string(),
            repo_type: repo_type.to_string(),
            revision: String::new(),
            exp,
            iat: now,
            kid: self.kid.clone(),
            token_type: "proxy".to_string(),
            oid: Some(oid.to_string()),
            operation: Some(operation.to_string()),
        };

        self.sign_claims(claims, "proxy_")
    }

    /// Sign and create an internal token for Hub-to-CAS communication
    /// Internal tokens are short-lived (60 seconds) and can only access /internal/* endpoints
    /// Returns (token, expiration_timestamp)
    /// I1 fix: Use unwrap_or_default() for system time to avoid panic on clock issues
    /// I2 fix: Return Result to propagate serialization errors instead of returning empty string
    pub fn sign_internal(&self) -> Result<(String, u64), String> {
        // M4 fix: Reject signing if system clock is broken
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| "System clock is before UNIX_EPOCH, cannot sign tokens".to_string())?;
        let exp = now + self.internal_ttl_seconds; // C1 fix: Configurable TTL (was hardcoded 60s)

        let claims = XetClaims {
            sub: "hub-service".to_string(),
            scope: "internal".to_string(),
            repo_id: "".to_string(),
            repo_type: "".to_string(),
            revision: "".to_string(),
            exp,
            iat: now,
            kid: self.kid.clone(),
            token_type: "internal".to_string(),
            oid: None,
            operation: None,
        };

        // C2 fix: Use internal_ prefix to distinguish from user tokens
        self.sign_claims(claims, "internal_")
    }

    /// Internal helper to verify a token's signature and decode its claims
    /// Shared logic between verify_proxy_token and verify_xet_token
    fn verify_token_inner(&self, token: &str, expected_prefix: &str) -> Option<XetClaims> {
        use ed25519_dalek::{Signature, Verifier};

        // Check if it has the expected prefix
        if !token.starts_with(expected_prefix) {
            return None;
        }

        // Parse JWT
        let token_body = token.strip_prefix(expected_prefix)?;

        let parts: Vec<&str> = token_body.split('.').collect();
        if parts.len() != 3 {
            return None;
        }

        // Verify signature using ed25519-dalek
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let signature_bytes = match URL_SAFE_NO_PAD.decode(parts[2]) {
            Ok(bytes) => bytes,
            Err(_) => return None,
        };

        let signature = match Signature::from_slice(&signature_bytes) {
            Ok(sig) => sig,
            Err(_) => return None,
        };

        // Get verifying key from signing key
        let verifying_key = self.signing_key.verifying_key();

        // Verify signature
        if verifying_key.verify(signing_input.as_bytes(), &signature).is_err() {
            return None;
        }

        // Decode claims (signature is valid, so this should succeed)
        let claims_json = match URL_SAFE_NO_PAD.decode(parts[1]) {
            Ok(json) => json,
            Err(_) => return None,
        };

        let claims: XetClaims = serde_json::from_slice(&claims_json).ok()?;

        // I2 fix: Verify kid matches expected to prevent key confusion attacks
        if claims.kid != self.kid {
            return None;
        }

        // Check expiration
        // M4 fix: If system clock is broken, reject all tokens (safe default)
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX); // Treats all tokens as expired when clock is broken
        if claims.exp < now {
            return None; // Token expired
        }

        Some(claims)
    }

    /// Verify a proxy token's signature and decode its claims
    /// Returns Some(claims) if the signature is valid and claims can be decoded, None otherwise
    #[must_use = "the result of token verification should be checked"]
    pub fn verify_proxy_token(&self, token: &str) -> Option<XetClaims> {
        self.verify_token_inner(token, "proxy_")
    }

    /// Get the key ID
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Verify a xet token's signature and decode its claims
    /// Returns Some(claims) if the signature is valid and claims can be decoded, None otherwise
    #[must_use = "the result of token verification should be checked"]
    pub fn verify_xet_token(&self, token: &str) -> Option<XetClaims> {
        self.verify_token_inner(token, "xet_")
    }

    /// Verify an internal token's signature and decode its claims
    /// Internal tokens use "internal_" prefix and are for Hub-to-CAS communication
    /// Returns Some(claims) if the signature is valid and claims can be decoded, None otherwise
    #[must_use = "the result of token verification should be checked"]
    pub fn verify_internal_token(&self, token: &str) -> Option<XetClaims> {
        self.verify_token_inner(token, "internal_")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn generate_test_key() -> SigningKey {
        let mut csprng = OsRng;
        SigningKey::generate(&mut csprng)
    }

    #[test]
    fn test_sign_produces_valid_format() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-1", 3600, 300);

        let (token, exp) = signer.sign("user123", "read", "namespace/model", "model", "main").unwrap();

        assert!(token.starts_with("xet_"), "Token should start with xet_");

        // Check that the token has three parts (header.claims.signature) after xet_ prefix
        let token_body = token.strip_prefix("xet_").unwrap();
        let parts: Vec<&str> = token_body.split('.').collect();
        assert_eq!(parts.len(), 3, "Token should have 3 parts (header.claims.signature)");

        // Verify expiration is in the future
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(exp > now, "Expiration should be in the future");
        assert!(exp <= now + 3601, "Expiration should be at most ttl_seconds from now");
    }

    #[test]
    fn test_sign_includes_correct_claims() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-2", 3600, 300);

        let (token, _) = signer.sign("user123", "write", "namespace/model", "dataset", "v1.0").unwrap();

        // Decode and verify claims
        let token_body = token.strip_prefix("xet_").unwrap();
        let parts: Vec<&str> = token_body.split('.').collect();
        let claims_json = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let claims: XetClaims = serde_json::from_slice(&claims_json).unwrap();

        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.scope, "write");
        assert_eq!(claims.repo_id, "namespace/model");
        assert_eq!(claims.repo_type, "dataset");
        assert_eq!(claims.revision, "v1.0");
        assert_eq!(claims.kid, "test-key-2");
    }

    #[test]
    fn test_from_pem() {
        // Generate a key and export to PEM
        let signing_key = generate_test_key();

        // Create PEM using pkcs8 encoding
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let pem = signing_key.to_pkcs8_pem(pkcs8::LineEnding::LF).unwrap();
        let pem_bytes = pem.as_bytes();

        // Load it back
        let signer = XetSigner::from_pem(pem_bytes, "pem-key", 3600, 300).unwrap();

        // Verify by signing something
        let (token, _) = signer.sign("user", "read", "repo", "model", "main").unwrap();
        assert!(token.starts_with("xet_"));
    }

    #[test]
    fn test_different_keys_produce_different_signatures() {
        let key1 = generate_test_key();
        let key2 = generate_test_key();

        let signer1 = XetSigner::new(key1, "key1", 3600, 300);
        let signer2 = XetSigner::new(key2, "key2", 3600, 300);

        let (token1, _) = signer1.sign("user", "read", "repo", "model", "main").unwrap();
        let (token2, _) = signer2.sign("user", "read", "repo", "model", "main").unwrap();

        assert_ne!(token1, token2, "Different keys should produce different signatures");
    }

    #[test]
    fn test_sign_proxy_produces_valid_format() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-proxy", 3600, 300);

        let (token, exp) = signer.sign_proxy("user123", "abc123def456", "upload", "", "").unwrap();

        assert!(token.starts_with("proxy_"), "Proxy token should start with proxy_");

        // Check that the token has three parts (header.claims.signature) after proxy_ prefix
        let token_body = token.strip_prefix("proxy_").unwrap();
        let parts: Vec<&str> = token_body.split('.').collect();
        assert_eq!(parts.len(), 3, "Token should have 3 parts (header.claims.signature)");

        // Verify expiration is in the future and ~5 minutes
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(exp > now, "Expiration should be in the future");
        assert!(exp <= now + 301, "Expiration should be at most 5 minutes from now");
    }

    #[test]
    fn test_sign_proxy_includes_correct_claims() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-proxy-2", 3600, 300);

        let (token, _) = signer.sign_proxy("user456", "oid789xyz", "download", "", "").unwrap();

        // Decode and verify claims
        let token_body = token.strip_prefix("proxy_").unwrap();
        let parts: Vec<&str> = token_body.split('.').collect();
        let claims_json = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let claims: XetClaims = serde_json::from_slice(&claims_json).unwrap();

        assert_eq!(claims.sub, "user456");
        assert_eq!(claims.scope, "lfs-download");
        assert_eq!(claims.token_type, "proxy");
        assert_eq!(claims.oid, Some("oid789xyz".to_string()));
        assert_eq!(claims.operation, Some("download".to_string()));
        assert_eq!(claims.kid, "test-key-proxy-2");
    }

    #[test]
    fn test_verify_proxy_token_valid() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-verify", 3600, 300);

        let (token, _) = signer.sign_proxy("user", "oid123", "upload", "", "").unwrap();

        // Valid token should verify
        assert!(signer.verify_proxy_token(&token).is_some(), "Valid proxy token should verify");
    }

    #[test]
    fn test_verify_proxy_token_invalid_signature() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-verify-2", 3600, 300);

        let (token, _) = signer.sign_proxy("user", "oid123", "upload", "", "").unwrap();

        // Tamper with the token
        let tampered_token = format!("{}tampered", &token[..token.len()-8]);

        // Invalid signature should not verify
        assert!(signer.verify_proxy_token(&tampered_token).is_none(), "Tampered token should not verify");
    }

    #[test]
    fn test_verify_proxy_token_wrong_prefix() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-verify-3", 3600, 300);

        let (token, _) = signer.sign_proxy("user", "oid123", "upload", "", "").unwrap();

        // Change prefix from proxy_ to xet_
        let wrong_prefix_token = format!("xet_{}", &token[6..]);

        // Wrong prefix should not verify
        assert!(signer.verify_proxy_token(&wrong_prefix_token).is_none(), "Token with wrong prefix should not verify");
    }

    #[test]
    fn test_verify_proxy_token_malformed() {
        let signing_key = generate_test_key();
        let signer = XetSigner::new(signing_key, "test-key-verify-4", 3600, 300);

        // Malformed tokens should not verify
        assert!(signer.verify_proxy_token("proxy_").is_none(), "Empty token body should not verify");
        assert!(signer.verify_proxy_token("proxy_abc").is_none(), "Single part token should not verify");
        assert!(signer.verify_proxy_token("proxy_abc.def").is_none(), "Two part token should not verify");
        assert!(signer.verify_proxy_token("proxy_abc.def.ghi.jkl").is_none(), "Four part token should not verify");
    }

    #[test]
    fn test_verify_proxy_token_different_key() {
        let key1 = generate_test_key();
        let key2 = generate_test_key();

        let signer1 = XetSigner::new(key1, "key1", 3600, 300);
        let signer2 = XetSigner::new(key2, "key2", 3600, 300);

        let (token, _) = signer1.sign_proxy("user", "oid123", "upload", "", "").unwrap();

        // Token signed with key1 should not verify with key2
        assert!(signer2.verify_proxy_token(&token).is_none(), "Token should not verify with different key");
    }
}
