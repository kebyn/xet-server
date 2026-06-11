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
            if let Ok(decoded) = STANDARD.decode(encoded) {
                if let Ok(creds) = String::from_utf8(decoded) {
                    if let Some((_user, pass)) = creds.split_once(':') {
                        return Some(pass.to_string());
                    }
                }
            }
        }
    }

    // Fall back to query parameter token (?token=...)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                if key == "token" {
                    // URL-decode the token value to handle special characters
                    if let Ok(decoded) = percent_encoding::percent_decode_str(value).decode_utf8() {
                        return Some(decoded.into_owned());
                    }
                }
            }
        }
    }

    None
}

/// Rewrite URLs in batch response from CAS URLs to Hub URLs,
/// and replace internal CAS auth tokens with the user's original token.
fn rewrite_batch_urls(response: &mut serde_json::Value, hub_base: &str, user_token: &str) {
    use url::Url;

    // Parse base URLs once for efficient rewriting
    let hub_url = match Url::parse(hub_base) {
        Ok(u) => u,
        Err(_) => return, // Invalid hub URL, skip rewriting
    };

    if let Some(objects) = response.get_mut("objects") {
        if let Some(arr) = objects.as_array_mut() {
            for obj in arr {
                if let Some(actions) = obj.get_mut("actions") {
                    for key in ["upload", "download"] {
                        if let Some(action) = actions.get_mut(key) {
                            // Rewrite URL from CAS to Hub using proper URL parsing
                            let new_href = action.get("href")
                                .and_then(|h| h.as_str())
                                .and_then(|h| Url::parse(h).ok())
                                .map(|mut url| {
                                    // Replace scheme and host with hub's scheme and host
                                    url.set_scheme(hub_url.scheme()).ok();
                                    url.set_host(hub_url.host_str()).ok();
                                    if let Some(port) = hub_url.port() {
                                        url.set_port(Some(port)).ok();
                                    } else {
                                        url.set_port(None).ok();
                                    }

                                    // SECURITY: Some LFS clients (e.g. huggingface_hub's python-httpx)
                                    // do not forward Authorization headers from batch responses.
                                    // We append token as query param as a workaround.
                                    // TODO(security): Replace with short-lived scoped proxy tokens
                                    // (5-min TTL, bound to specific oid+operation) to prevent token
                                    // leakage in server/proxy logs. See code review C1.
                                    url.query_pairs_mut().append_pair("token", user_token);
                                    url.to_string()
                                });
                            if let Some(href) = new_href {
                                if let Some(action_obj) = action.as_object_mut() {
                                    action_obj.insert("href".to_string(), serde_json::Value::String(href));
                                }
                            }
                            // Also replace internal CAS token with user's token in Authorization header
                            let needs_auth_replace = action.get("header")
                                .and_then(|h| h.get("Authorization"))
                                .and_then(|a| a.as_str())
                                .map(|s| s.starts_with("Bearer xet_"))
                                .unwrap_or(false);
                            if needs_auth_replace {
                                if let Some(header_obj) = action.get_mut("header").and_then(|h| h.as_object_mut()) {
                                    header_obj.insert(
                                        "Authorization".to_string(),
                                        serde_json::Value::String(format!("Bearer {}", user_token)),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Validate OID format (64 hex characters)
fn validate_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.chars().all(|c| c.is_ascii_hexdigit())
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

    match token_store.validate_token(&token) {
        Ok(Some(_)) => {},
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

    // Rewrite URLs and auth headers
    let hub_base = config.server.base_url();
    rewrite_batch_urls(&mut response, &hub_base, &token);

    HttpResponse::Ok().json(response)
}

/// Handle LFS object upload
pub async fn lfs_upload(
    req: HttpRequest,
    path: web::Path<String>,
    body: web::Bytes,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    cas_client: web::Data<std::sync::Arc<CasClient>>,
) -> HttpResponse {
    // Extract token (Bearer from batch action or Basic from git-lfs)
    let token = match extract_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    // Validate token through TokenStore
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

    // C5: Check write scope for upload operations
    if token_info.scope != "write" {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Write scope required",
            "error_type": "AuthorizationError"
        }));
    }

    let oid = path.into_inner();

    // I7: Validate OID format
    if !validate_oid(&oid) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid OID format",
            "error_type": "ValidationError"
        }));
    }

    // Generate internal token for CAS
    let (internal_token, _) = xet_signer.sign_internal();

    // Forward to CAS
    match cas_client.proxy_lfs_upload(&oid, body, &internal_token).await {
        Ok(_) => HttpResponse::Ok().finish(),
        Err(e) => {
            HttpResponse::BadGateway().json(serde_json::json!({
                "error": e.to_string(),
                "error_type": "BadGateway"
            }))
        }
    }
}

/// Handle LFS object download
pub async fn lfs_download(
    req: HttpRequest,
    path: web::Path<String>,
    token_store: web::Data<std::sync::Arc<TokenStore>>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    cas_client: web::Data<std::sync::Arc<CasClient>>,
) -> HttpResponse {
    // Extract token (Bearer from batch action or Basic from git-lfs)
    let token = match extract_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    // Validate token through TokenStore
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

    // I1: Check read scope for download operations
    if token_info.scope != "read" && token_info.scope != "write" {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Read scope required",
            "error_type": "AuthorizationError"
        }));
    }

    let oid = path.into_inner();

    // I7: Validate OID format
    if !validate_oid(&oid) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid OID format",
            "error_type": "ValidationError"
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
        let mut response = json!({
            "objects": [
                {
                    "oid": "abc123",
                    "size": 1024,
                    "actions": {
                        "upload": {
                            "href": "http://cas:9090/lfs/objects/abc123"
                        },
                        "download": {
                            "href": "http://cas:9090/lfs/objects/abc123"
                        }
                    }
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", "hf_test123");

        let objects = response.get("objects").unwrap().as_array().unwrap();
        let actions = objects[0].get("actions").unwrap();
        let upload_href = actions.get("upload").unwrap().get("href").unwrap().as_str().unwrap();
        let download_href = actions.get("download").unwrap().get("href").unwrap().as_str().unwrap();

        assert_eq!(upload_href, "http://hub:8080/lfs/objects/abc123?token=hf_test123");
        assert_eq!(download_href, "http://hub:8080/lfs/objects/abc123?token=hf_test123");
    }

    #[test]
    fn test_rewrite_batch_urls_no_actions() {
        let mut response = json!({
            "objects": [
                {
                    "oid": "abc123",
                    "size": 1024
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", "hf_test123");

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
        let mut response = json!({
            "objects": [
                {
                    "oid": "abc123",
                    "size": 1024,
                    "actions": {
                        "upload": {
                            "href": "http://cas:9090/lfs/objects/abc123"
                        }
                    }
                },
                {
                    "oid": "def456",
                    "size": 2048,
                    "actions": {
                        "download": {
                            "href": "http://cas:9090/lfs/objects/def456"
                        }
                    }
                }
            ]
        });

        rewrite_batch_urls(&mut response, "http://hub:8080", "hf_test123");

        let objects = response.get("objects").unwrap().as_array().unwrap();

        // First object has upload action
        let upload_href = objects[0].get("actions").unwrap().get("upload").unwrap().get("href").unwrap().as_str().unwrap();
        assert_eq!(upload_href, "http://hub:8080/lfs/objects/abc123?token=hf_test123");

        // Second object has download action
        let download_href = objects[1].get("actions").unwrap().get("download").unwrap().get("href").unwrap().as_str().unwrap();
        assert_eq!(download_href, "http://hub:8080/lfs/objects/def456?token=hf_test123");
    }
}
