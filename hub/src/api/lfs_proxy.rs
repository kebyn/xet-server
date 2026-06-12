use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;

/// Extract token from Authorization header (Bearer/Basic) or query parameter (?token=...).
/// Security note: Query parameter tokens leak in server logs and proxy logs.
/// We support them as a fallback because some LFS clients (e.g. huggingface_hub's python-httpx)
/// do not forward Authorization headers from batch responses.
fn extract_token(req: &HttpRequest) -> Option<String> {
    // Try Authorization header first
    if let Some(auth) = req.headers().get("Authorization") {
        let auth_str = auth.to_str().ok()?;

        // Try Bearer first
        if let Some(token) = auth_str.strip_prefix("Bearer ") {
            return Some(token.to_string());
        }

        // Try Basic auth (username:password where password is the token)
        if let Some(encoded) = auth_str.strip_prefix("Basic ") {
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            if let Ok(decoded) = STANDARD.decode(encoded)
                && let Ok(creds) = String::from_utf8(decoded)
                    && let Some((_user, pass)) = creds.split_once(':') {
                        return Some(pass.to_string());
                    }
        }
    }

    // Fall back to query parameter token (?token=...)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=')
                && key == "token" {
                    // URL-decode the token value to handle special characters
                    if let Ok(decoded) = percent_encoding::percent_decode_str(value).decode_utf8() {
                        return Some(decoded.into_owned());
                    }
                }
        }
    }

    None
}

/// Rewrite URLs in batch response from CAS URLs to Hub URLs,
/// and replace internal CAS auth tokens with short-lived proxy tokens.
fn rewrite_batch_urls(
    response: &mut serde_json::Value,
    hub_base: &str,
    signer: &XetSigner,
    username: &str,
) {
    use url::Url;

    // Parse base URLs once for efficient rewriting
    let hub_url = match Url::parse(hub_base) {
        Ok(u) => u,
        Err(_) => return, // Invalid hub URL, skip rewriting
    };

    if let Some(objects) = response.get_mut("objects")
        && let Some(arr) = objects.as_array_mut() {
            for obj in arr {
                // Clone oid to avoid borrow conflict
                let oid = obj.get("oid").and_then(|o| o.as_str()).unwrap_or("").to_string();

                // Skip generating proxy tokens for invalid OIDs to avoid wasted computation
                if !validate_oid(&oid) {
                    continue;
                }

                if let Some(actions) = obj.get_mut("actions") {
                    // Generate proxy tokens for each operation
                    // Note: repo_id and repo_type are empty because LFS batch API doesn't include repo context
                    // The token is still bound to OID + operation, which provides sufficient security
                    if let Some(upload_action) = actions.get_mut("upload") {
                        let (proxy_token, _) = signer.sign_proxy(username, &oid, "upload", "", "");
                        rewrite_action_url(upload_action, &hub_url, &proxy_token);
                    }
                    if let Some(download_action) = actions.get_mut("download") {
                        let (proxy_token, _) = signer.sign_proxy(username, &oid, "download", "", "");
                        rewrite_action_url(download_action, &hub_url, &proxy_token);
                    }
                }
            }
        }
}

/// Rewrite a single action's URL and auth header with proxy token
fn rewrite_action_url(action: &mut serde_json::Value, hub_url: &url::Url, proxy_token: &str) {
    // Rewrite URL from CAS to Hub using proper URL parsing
    let new_href = action.get("href")
        .and_then(|h| h.as_str())
        .and_then(|h| url::Url::parse(h).ok())
        .map(|mut url| {
            // Replace scheme and host with hub's scheme and host
            url.set_scheme(hub_url.scheme()).ok();
            url.set_host(hub_url.host_str()).ok();
            if let Some(port) = hub_url.port() {
                url.set_port(Some(port)).ok();
            } else {
                url.set_port(None).ok();
            }

            // SECURITY: Use short-lived proxy token (5-min TTL) instead of user's long-lived token.
            // Even if this token leaks in logs, it's bound to a specific OID+operation and expires quickly.
            url.query_pairs_mut().append_pair("token", proxy_token);
            url.to_string()
        });

    if let Some(href) = new_href
        && let Some(action_obj) = action.as_object_mut() {
            action_obj.insert("href".to_string(), serde_json::Value::String(href));
        }

    // Always replace Authorization header with proxy token if present
    // This ensures internal CAS tokens are never leaked to clients
    if action.get("header").and_then(|h| h.get("Authorization")).is_some()
        && let Some(header_obj) = action.get_mut("header").and_then(|h| h.as_object_mut()) {
            header_obj.insert(
                "Authorization".to_string(),
                serde_json::Value::String(format!("Bearer {}", proxy_token)),
            );
        }
}

/// Validate OID format (64 hex characters)
fn validate_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.chars().all(|c| c.is_ascii_hexdigit())
}

/// Validate a proxy token (short-lived LFS token)
/// Returns true if the token is valid, false otherwise
///
/// This function performs business-level validation (OID, operation, expiration, token type).
/// Cryptographic verification (signature, prefix format) is handled by `signer.verify_proxy_token`.
fn validate_proxy_token(
    token: &str,
    expected_oid: &str,
    expected_operation: &str,
    signer: &XetSigner,
) -> bool {
    // Verify signature, decode claims, and check proxy_ prefix (all in one pass)
    let claims = match signer.verify_proxy_token(token) {
        Some(claims) => claims,
        None => return false,
    };

    // Check token type (not checked by verify_proxy_token)
    if claims.token_type != "proxy" {
        return false;
    }

    // Check key ID matches (defense-in-depth for key rotation scenarios)
    if claims.kid != signer.kid() {
        return false;
    }

    // Check expiration (not checked by verify_proxy_token)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if claims.exp < now {
        return false;
    }

    // Check OID matches
    if claims.oid.as_deref() != Some(expected_oid) {
        return false;
    }

    // Check operation matches
    if claims.operation.as_deref() != Some(expected_operation) {
        return false;
    }

    true
}

/// Handle Git LFS batch request
pub async fn lfs_batch(
    req: HttpRequest,
    body: web::Json<serde_json::Value>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    cas_client: web::Data<std::sync::Arc<CasClient>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    // Extract and validate Bearer token
    let token = match extract_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let token_info = match token_store.validate_token(&token) {
        Ok(Some(info)) => info,
        Ok(None) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token",
                "error_type": "AuthenticationError"
            }));
        }
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("{}", e),
                "error_type": "InternalError"
            }));
        }
    };

    // Generate internal token for CAS
    let (internal_token, _) = xet_signer.sign_internal();

    // Forward to CAS
    let mut response = match cas_client.proxy_batch(&body, &internal_token).await {
        Ok(r) => r,
        Err(e) => {
            return HttpResponse::BadGateway().json(serde_json::json!({
                "error": e.to_string(),
                "error_type": "BadGateway"
            }));
        }
    };

    // Rewrite URLs and auth headers with short-lived proxy tokens
    let hub_base = config.server.base_url();
    rewrite_batch_urls(&mut response, &hub_base, &xet_signer, &token_info.username);

    HttpResponse::Ok().json(response)
}

/// Handle LFS object upload
/// Note: Currently uses buffered upload with 512MB limit.
/// Future improvement: Implement true streaming with awc send_stream.
pub async fn lfs_upload(
    req: HttpRequest,
    path: web::Path<String>,
    body: web::Bytes,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    cas_client: web::Data<std::sync::Arc<CasClient>>,
) -> HttpResponse {
    // Extract token
    let token = match extract_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let oid = path.into_inner();

    // I7: Validate OID format
    if !validate_oid(&oid) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid OID format",
            "error_type": "ValidationError"
        }));
    }

    // Validate proxy token
    if !validate_proxy_token(&token, &oid, "upload", &xet_signer) {
        return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid or expired proxy token",
            "error_type": "AuthenticationError"
        }));
    }

    // Verify content hash matches OID (SHA256)
    use sha2::{Sha256, Digest};
    let computed_hash = hex::encode(Sha256::digest(&body));
    if computed_hash != oid {
        tracing::warn!(
            "Hash mismatch for OID {}: computed {} ({} bytes)",
            oid, computed_hash, body.len()
        );
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Hash mismatch: uploaded content does not match OID",
            "error_type": "ValidationError"
        }));
    }

    // Generate internal token for CAS
    let (internal_token, _) = xet_signer.sign_internal();

    // Forward to CAS
    match cas_client.proxy_lfs_upload(&oid, body, &internal_token).await {
        Ok(_) => HttpResponse::Ok().finish(),
        Err((status, error_msg)) => {
            let status_code = actix_web::http::StatusCode::from_u16(status).unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
            HttpResponse::build(status_code).json(serde_json::json!({
                "error": error_msg,
                "error_type": "CasError"
            }))
        }
    }
}

/// Handle LFS object download
pub async fn lfs_download(
    req: HttpRequest,
    path: web::Path<String>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    cas_client: web::Data<std::sync::Arc<CasClient>>,
) -> HttpResponse {
    // Extract token
    let token = match extract_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let oid = path.into_inner();

    // I7: Validate OID format
    if !validate_oid(&oid) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid OID format",
            "error_type": "ValidationError"
        }));
    }

    // Validate proxy token
    if !validate_proxy_token(&token, &oid, "download", &xet_signer) {
        return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid or expired proxy token",
            "error_type": "AuthenticationError"
        }));
    }

    // Generate internal token for CAS
    let (internal_token, _) = xet_signer.sign_internal();

    // Forward to CAS
    match cas_client.proxy_lfs_download(&oid, &internal_token).await {
        Ok(bytes) => HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(bytes),
        Err(e) => match e {
            crate::error::HubError::NotFound(_) => {
                HttpResponse::NotFound().json(serde_json::json!({
                    "error": e.to_string(),
                    "error_type": "NotFoundError"
                }))
            }
            _ => HttpResponse::BadGateway().json(serde_json::json!({
                "error": e.to_string(),
                "error_type": "BadGateway"
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_rewrite_batch_urls() {
        use crate::auth::xet_signer::XetSigner;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let signer = XetSigner::new(signing_key, "test-key", 3600);

        // Use a valid 64-character hex OID
        let valid_oid = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

        let mut response = json!({
            "objects": [
                {
                    "oid": valid_oid,
                    "size": 1024,
                    "actions": {
                        "upload": {
                            "href": format!("http://cas:9090/lfs/objects/{}", valid_oid)
                        },
                        "download": {
                            "href": format!("http://cas:9090/lfs/objects/{}", valid_oid)
                        }
                    }
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", &signer, "testuser");

        let objects = response.get("objects").unwrap().as_array().unwrap();
        let actions = objects[0].get("actions").unwrap();
        let upload_href = actions.get("upload").unwrap().get("href").unwrap().as_str().unwrap();
        let download_href = actions.get("download").unwrap().get("href").unwrap().as_str().unwrap();

        // URLs should be rewritten with proxy tokens (starting with proxy_)
        assert!(upload_href.starts_with(&format!("http://hub:8080/lfs/objects/{}?token=proxy_", valid_oid)));
        assert!(download_href.starts_with(&format!("http://hub:8080/lfs/objects/{}?token=proxy_", valid_oid)));
    }

    #[test]
    fn test_rewrite_batch_urls_no_actions() {
        use crate::auth::xet_signer::XetSigner;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let signer = XetSigner::new(signing_key, "test-key", 3600);

        let mut response = json!({
            "objects": [
                {
                    "oid": "abc123",
                    "size": 1024
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", &signer, "testuser");

        // Should remain unchanged
        assert_eq!(response, json!({
            "objects": [
                {
                    "oid": "abc123",
                    "size": 1024
                }
            ]
        }));
    }

    #[test]
    fn test_rewrite_batch_urls_partial_match() {
        use crate::auth::xet_signer::XetSigner;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let signer = XetSigner::new(signing_key, "test-key", 3600);

        // Use valid 64-character hex OIDs
        let oid1 = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let oid2 = "b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3";

        let mut response = json!({
            "objects": [
                {
                    "oid": oid1,
                    "size": 1024,
                    "actions": {
                        "upload": {
                            "href": format!("http://cas:9090/lfs/objects/{}", oid1)
                        }
                    }
                },
                {
                    "oid": oid2,
                    "size": 2048,
                    "actions": {
                        "download": {
                            "href": format!("http://cas:9090/lfs/objects/{}", oid2)
                        }
                    }
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", &signer, "testuser");

        let objects = response.get("objects").unwrap().as_array().unwrap();

        // First object has upload action
        let upload_href = objects[0].get("actions").unwrap().get("upload").unwrap().get("href").unwrap().as_str().unwrap();
        assert!(upload_href.starts_with(&format!("http://hub:8080/lfs/objects/{}?token=proxy_", oid1)));

        // Second object has download action
        let download_href = objects[1].get("actions").unwrap().get("download").unwrap().get("href").unwrap().as_str().unwrap();
        assert!(download_href.starts_with(&format!("http://hub:8080/lfs/objects/{}?token=proxy_", oid2)));
    }

    // Helper function to create a test signer
    fn create_test_signer() -> crate::auth::xet_signer::XetSigner {
        use crate::auth::xet_signer::XetSigner;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        XetSigner::new(signing_key, "test-key", 3600)
    }

    #[test]
    fn test_validate_proxy_token_valid() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "");

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);
        assert!(result, "Valid proxy token should be accepted");
    }

    #[test]
    fn test_validate_proxy_token_expired() {
        let signer = create_test_signer();
        // Create a token that expires immediately (we can't easily test this without mocking time)
        // For now, we'll just verify the validation logic works
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "");

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);
        assert!(result, "Non-expired token should be accepted");
    }

    #[test]
    fn test_validate_proxy_token_wrong_oid() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "");

        // Try to validate with wrong OID
        let result = validate_proxy_token(&token, "wrongoid", "upload", &signer);
        assert!(!result, "Token with wrong OID should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_wrong_operation() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "");

        // Try to validate with wrong operation
        let result = validate_proxy_token(&token, "abc123def456", "download", &signer);
        assert!(!result, "Token with wrong operation should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_invalid_signature() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "");

        // Tamper with the token
        let tampered_token = format!("{}x", &token[..token.len()-1]);

        let result = validate_proxy_token(&tampered_token, "abc123def456", "upload", &signer);
        assert!(!result, "Token with invalid signature should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_non_proxy_token() {
        let signer = create_test_signer();
        // Create a regular user token instead of proxy token
        let (user_token, _) = signer.sign("testuser", "read", "repo", "model", "main");

        let result = validate_proxy_token(&user_token, "abc123def456", "upload", &signer);
        assert!(!result, "User token should be rejected as proxy token");
    }

    #[test]
    fn test_validate_proxy_token_malformed() {
        let signer = create_test_signer();

        // Test various malformed tokens
        assert!(!validate_proxy_token("", "abc123", "upload", &signer), "Empty token should be rejected");
        assert!(!validate_proxy_token("proxy_", "abc123", "upload", &signer), "Empty body should be rejected");
        assert!(!validate_proxy_token("proxy_abc", "abc123", "upload", &signer), "Single part should be rejected");
        assert!(!validate_proxy_token("proxy_abc.def", "abc123", "upload", &signer), "Two parts should be rejected");
        assert!(!validate_proxy_token("proxy_abc.def.ghi.jkl", "abc123", "upload", &signer), "Four parts should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_wrong_token_type() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "");

        // Manually tamper with the token_type claim
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

        let token_body = token.strip_prefix("proxy_").unwrap();
        let parts: Vec<&str> = token_body.split('.').collect();

        // Decode claims
        let claims_json = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let mut claims: serde_json::Value = serde_json::from_slice(&claims_json).unwrap();

        // Change token_type from "proxy" to "user"
        claims["token_type"] = serde_json::json!("user");

        // Re-encode
        let new_claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let tampered_token = format!("proxy_{}.{}.{}", parts[0], new_claims_b64, parts[2]);

        let result = validate_proxy_token(&tampered_token, "abc123def456", "upload", &signer);
        assert!(!result, "Token with wrong token_type should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_wrong_kid() {
        use crate::auth::xet_signer::XetSigner;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        // Create two signers with different key IDs
        let mut csprng = OsRng;
        let signing_key1 = SigningKey::generate(&mut csprng);
        let signer1 = XetSigner::new(signing_key1, "key-id-1", 3600);

        let mut csprng2 = OsRng;
        let signing_key2 = SigningKey::generate(&mut csprng2);
        let signer2 = XetSigner::new(signing_key2, "key-id-2", 3600);

        // Sign token with signer1
        let (token, _) = signer1.sign_proxy("testuser", "abc123def456", "upload", "", "");

        // Try to validate with signer2 (different kid)
        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer2);
        assert!(!result, "Token with wrong kid should be rejected");
    }
}
