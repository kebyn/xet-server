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
            if let Ok(decoded) = STANDARD.decode(encoded)
                && let Ok(creds) = String::from_utf8(decoded)
                && let Some((_user, pass)) = creds.split_once(':')
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
            if let Some((key, value)) = pair.split_once('=')
                && key == "token"
                && let Ok(decoded) = percent_encoding::percent_decode_str(value).decode_utf8()
                && decoded.starts_with("proxy_")
            {
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
            let token_preview = token.get(..30).unwrap_or(token);
            tracing::error!(
                "validate_proxy_token: verify_proxy_token failed for token starting with: {}...",
                token_preview
            );
            return false;
        }
    };

    if claims.token_type != "proxy" {
        tracing::error!(
            "validate_proxy_token: token_type mismatch: {} != proxy",
            claims.token_type
        );
        return false;
    }

    let expected_scope = format!("lfs-{}", expected_operation);
    if !claims
        .scope
        .split_whitespace()
        .any(|scope| scope == expected_scope.as_str())
    {
        tracing::error!(
            "validate_proxy_token: scope mismatch: {} does not contain {}",
            claims.scope,
            expected_scope
        );
        return false;
    }

    if claims.oid.as_deref() != Some(expected_oid) {
        tracing::error!(
            "validate_proxy_token: oid mismatch: {:?} != {}",
            claims.oid,
            expected_oid
        );
        return false;
    }

    if claims.operation.as_deref() != Some(expected_operation) {
        tracing::error!(
            "validate_proxy_token: operation mismatch: {:?} != {}",
            claims.operation,
            expected_operation
        );
        return false;
    }

    true
}
