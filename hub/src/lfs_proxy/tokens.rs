use crate::auth::xet_signer::XetSigner;
use actix_web::HttpRequest;

/// Extract token from Authorization header (Bearer/Basic).
///
/// Query parameter tokens are intentionally excluded here to avoid leaking user
/// tokens through URLs. Query parameters are only accepted by `extract_proxy_token`
/// for short-lived, OID-bound LFS proxy tokens.
pub(crate) fn extract_token(req: &HttpRequest) -> Option<String> {
    if let Some(auth) = req.headers().get("Authorization") {
        let auth_str = auth.to_str().ok()?;

        if let Some(token) = auth_str.strip_prefix("Bearer ") {
            return Some(token.to_string());
        }

        if let Some(encoded) = auth_str.strip_prefix("Basic ") {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            let credentials = STANDARD
                .decode(encoded)
                .ok()
                .and_then(|decoded| String::from_utf8(decoded).ok());
            if let Some(pass) = credentials
                .as_deref()
                .and_then(|creds| creds.split_once(':').map(|(_user, pass)| pass))
            {
                return Some(pass.to_string());
            }
        }
    }

    None
}

/// Extract proxy token for LFS download/upload operations.
///
/// Query parameter proxy tokens are supported for redirects from `/resolve/*`.
/// They are short-lived and OID-bound, which limits log-leakage blast radius.
pub(crate) fn extract_proxy_token(req: &HttpRequest) -> Option<String> {
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if key != "token" {
                continue;
            }
            let Ok(decoded) = percent_encoding::percent_decode_str(value).decode_utf8() else {
                continue;
            };
            if decoded.starts_with("proxy_") {
                tracing::info!(
                    path = %req.uri().path(),
                    "Proxy token received via query parameter (short-lived, OID-bound)"
                );
                return Some(decoded.into_owned());
            }
        }
    }

    extract_token(req)
}

/// Validate a short-lived LFS proxy token against operation-specific bindings.
///
/// Cryptographic verification, prefix format and token type are handled by
/// `signer.verify_proxy_token`; this helper checks the business-level binding.
pub(crate) fn validate_proxy_token(
    token: &str,
    expected_oid: &str,
    expected_operation: &str,
    signer: &XetSigner,
) -> bool {
    let claims = match signer.verify_proxy_token(token) {
        Some(claims) => claims,
        None => {
            tracing::warn!(
                reason = "verification_failed",
                operation = expected_operation,
                "Proxy token rejected"
            );
            return false;
        }
    };

    if claims.token_type != "proxy" {
        tracing::warn!(
            reason = "token_type_mismatch",
            operation = expected_operation,
            "Proxy token rejected"
        );
        return false;
    }

    let expected_scope = format!("lfs-{}", expected_operation);
    if !claims
        .scope
        .split_whitespace()
        .any(|scope| scope == expected_scope.as_str())
    {
        tracing::warn!(
            reason = "scope_mismatch",
            operation = expected_operation,
            "Proxy token rejected"
        );
        return false;
    }

    if claims.oid.as_deref() != Some(expected_oid) {
        tracing::warn!(
            reason = "oid_mismatch",
            operation = expected_operation,
            "Proxy token rejected"
        );
        return false;
    }

    if claims.operation.as_deref() != Some(expected_operation) {
        tracing::warn!(
            reason = "operation_mismatch",
            operation = expected_operation,
            "Proxy token rejected"
        );
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    use crate::auth::xet_signer::XetSigner;

    use super::validate_proxy_token;

    fn signer() -> XetSigner {
        let signing_key = SigningKey::generate(&mut OsRng);
        XetSigner::new(signing_key, "test-key", 3600, 300)
    }

    fn sign_raw_proxy_token(token_type: &str, scope: &str, operation: &str) -> (String, XetSigner) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let signer = XetSigner::new(signing_key.clone(), "test-key", 3600, 300);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let header = serde_json::json!({
            "alg": "EdDSA",
            "typ": "JWT",
            "kid": "test-key",
        });
        let claims = serde_json::json!({
            "sub": "testuser",
            "scope": scope,
            "repo_id": "",
            "repo_type": "",
            "revision": "",
            "exp": now + 300,
            "iat": now,
            "kid": "test-key",
            "token_type": token_type,
            "oid": "abc123def456",
            "operation": operation,
        });

        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let signing_input = format!("{}.{}", header_b64, claims_b64);
        let signature = signing_key.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        (format!("proxy_{}.{}", signing_input, sig_b64), signer)
    }

    #[test]
    fn valid_proxy_token_is_accepted() {
        let signer = signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);

        assert!(result, "Valid proxy token should be accepted");
    }

    #[test]
    fn non_expired_proxy_token_is_accepted() {
        let signer = signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);

        assert!(result, "Non-expired token should be accepted");
    }

    #[test]
    fn proxy_token_with_wrong_oid_is_rejected() {
        let signer = signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        let result = validate_proxy_token(&token, "wrongoid", "upload", &signer);

        assert!(!result, "Token with wrong OID should be rejected");
    }

    #[test]
    fn proxy_token_with_wrong_operation_is_rejected() {
        let signer = signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "download", &signer);

        assert!(!result, "Token with wrong operation should be rejected");
    }

    #[test]
    fn proxy_token_with_wrong_scope_is_rejected() {
        let signer = signer();
        assert!(
            signer
                .sign_proxy_claims_for_test(
                    "testuser",
                    "lfs-upload",
                    "abc123def456",
                    "download",
                    "",
                    "",
                )
                .is_err(),
            "production signer must reject a scope/operation mismatch"
        );
        let (token, signer) = sign_raw_proxy_token("proxy", "lfs-upload", "download");

        let result = validate_proxy_token(&token, "abc123def456", "download", &signer);

        assert!(!result, "Token with wrong scope should be rejected");
    }

    #[test]
    fn proxy_token_with_invalid_signature_is_rejected() {
        let signer = signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();
        let tampered_token = format!("{}x", &token[..token.len() - 1]);

        let result = validate_proxy_token(&tampered_token, "abc123def456", "upload", &signer);

        assert!(!result, "Token with invalid signature should be rejected");
    }

    #[test]
    fn user_token_is_rejected_as_proxy_token() {
        let signer = signer();
        let (user_token, _) = signer
            .sign("testuser", "read", "repo", "model", "main")
            .unwrap();

        let result = validate_proxy_token(&user_token, "abc123def456", "upload", &signer);

        assert!(!result, "User token should be rejected as proxy token");
    }

    #[test]
    fn malformed_proxy_tokens_are_rejected() {
        let signer = signer();

        assert!(
            !validate_proxy_token("", "abc123", "upload", &signer),
            "Empty token should be rejected"
        );
        assert!(
            !validate_proxy_token("proxy_", "abc123", "upload", &signer),
            "Empty body should be rejected"
        );
        assert!(
            !validate_proxy_token("proxy_abc", "abc123", "upload", &signer),
            "Single part should be rejected"
        );
        assert!(
            !validate_proxy_token("proxy_abc.def", "abc123", "upload", &signer),
            "Two parts should be rejected"
        );
        assert!(
            !validate_proxy_token("proxy_abc.def.ghi.jkl", "abc123", "upload", &signer),
            "Four parts should be rejected"
        );
    }

    #[test]
    fn proxy_token_with_wrong_token_type_is_rejected() {
        let (tampered_token, signer) = sign_raw_proxy_token("user", "lfs-upload", "upload");

        let result = validate_proxy_token(&tampered_token, "abc123def456", "upload", &signer);

        assert!(!result, "Token with wrong token_type should be rejected");
    }

    #[test]
    fn proxy_token_with_wrong_kid_is_rejected() {
        let signer1 = XetSigner::new(SigningKey::generate(&mut OsRng), "key-id-1", 3600, 300);
        let signer2 = XetSigner::new(SigningKey::generate(&mut OsRng), "key-id-2", 3600, 300);
        let (token, _) = signer1
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer2);

        assert!(!result, "Token with wrong kid should be rejected");
    }
}
