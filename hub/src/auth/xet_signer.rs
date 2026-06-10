use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Serialize, Deserialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// JWT header for Xet tokens
#[derive(Debug, Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
    kid: String,
}

/// Claims for Xet access tokens
#[derive(Debug, Serialize, Deserialize)]
pub struct XetClaims {
    pub sub: String,
    pub scope: String,
    pub repo_id: String,
    pub repo_type: String,
    pub revision: String,
    pub exp: u64,
    pub iat: u64,
    pub kid: String,
}

/// Xet token signer for creating access tokens for CAS
pub struct XetSigner {
    signing_key: SigningKey,
    kid: String,
    ttl_seconds: u64,
}

impl XetSigner {
    /// Create a new XetSigner from a PEM-encoded private key
    pub fn from_pem(pem_bytes: &[u8], kid: &str, ttl_seconds: u64) -> Result<Self, String> {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        let pem_str = std::str::from_utf8(pem_bytes).map_err(|e| e.to_string())?;
        let signing_key = SigningKey::from_pkcs8_pem(pem_str)
            .map_err(|e| format!("Failed to load private key: {}", e))?;
        Ok(Self {
            signing_key,
            kid: kid.to_string(),
            ttl_seconds,
        })
    }

    /// Create a new XetSigner from a raw signing key (for testing)
    pub fn new(signing_key: SigningKey, kid: &str, ttl_seconds: u64) -> Self {
        Self {
            signing_key,
            kid: kid.to_string(),
            ttl_seconds,
        }
    }

    /// Sign and create a Xet access token
    /// Returns (token, expiration_timestamp)
    pub fn sign(&self, sub: &str, scope: &str, repo_id: &str, repo_type: &str, revision: &str) -> (String, u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
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
        };

        let header = JwtHeader {
            alg: "EdDSA",
            typ: "JWT",
            kid: self.kid.clone(),
        };

        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());

        let signing_input = format!("{}.{}", header_b64, claims_b64);
        let signature = self.signing_key.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        (format!("xet_{}.{}", signing_input, sig_b64), exp)
    }

    /// Get the key ID
    pub fn kid(&self) -> &str {
        &self.kid
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
        let signer = XetSigner::new(signing_key, "test-key-1", 3600);

        let (token, exp) = signer.sign("user123", "read", "namespace/model", "model", "main");

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
        let signer = XetSigner::new(signing_key, "test-key-2", 3600);

        let (token, _) = signer.sign("user123", "write", "namespace/model", "dataset", "v1.0");

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
        let signer = XetSigner::from_pem(pem_bytes, "pem-key", 3600).unwrap();

        // Verify by signing something
        let (token, _) = signer.sign("user", "read", "repo", "model", "main");
        assert!(token.starts_with("xet_"));
    }

    #[test]
    fn test_different_keys_produce_different_signatures() {
        let key1 = generate_test_key();
        let key2 = generate_test_key();

        let signer1 = XetSigner::new(key1, "key1", 3600);
        let signer2 = XetSigner::new(key2, "key2", 3600);

        let (token1, _) = signer1.sign("user", "read", "repo", "model", "main");
        let (token2, _) = signer2.sign("user", "read", "repo", "model", "main");

        assert_ne!(token1, token2, "Different keys should produce different signatures");
    }
}