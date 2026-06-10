use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;

/// Extract Bearer token from Authorization header
fn extract_bearer(req: &HttpRequest) -> Option<String> {
    let auth = req.headers().get("Authorization")?;
    auth.to_str().ok()?.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Rewrite URLs in batch response from CAS URLs to Hub URLs
fn rewrite_batch_urls(response: &mut serde_json::Value, hub_base: &str, cas_base: &str) {
    if let Some(objects) = response.get_mut("objects") {
        if let Some(arr) = objects.as_array_mut() {
            for obj in arr {
                if let Some(actions) = obj.get_mut("actions") {
                    for key in ["upload", "download"] {
                        if let Some(action) = actions.get_mut(key) {
                            if let Some(href) = action.get("href").and_then(|h| h.as_str()) {
                                let new_href = href.replace(cas_base, hub_base);
                                if let Some(action_obj) = action.as_object_mut() {
                                    action_obj.insert("href".to_string(), serde_json::Value::String(new_href));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
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
    let token = match extract_bearer(&req) {
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

    // Rewrite URLs
    let hub_base = format!("http://{}:{}", config.server.host, config.server.port);
    let cas_base = config.cas.base_url.clone();
    rewrite_batch_urls(&mut response, &hub_base, &cas_base);

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
    // Extract and validate Bearer token
    let token = match extract_bearer(&req) {
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

    let oid = path.into_inner();

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
    // Extract and validate Bearer token
    let token = match extract_bearer(&req) {
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

    let oid = path.into_inner();

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

        rewrite_batch_urls(&mut response, "http://hub:8080", "http://cas:9090");

        let objects = response.get("objects").unwrap().as_array().unwrap();
        let actions = objects[0].get("actions").unwrap();
        let upload_href = actions.get("upload").unwrap().get("href").unwrap().as_str().unwrap();
        let download_href = actions.get("download").unwrap().get("href").unwrap().as_str().unwrap();

        assert_eq!(upload_href, "http://hub:8080/lfs/objects/abc123");
        assert_eq!(download_href, "http://hub:8080/lfs/objects/abc123");
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

        rewrite_batch_urls(&mut response, "http://hub:8080", "http://cas:9090");

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

        rewrite_batch_urls(&mut response, "http://hub:8080", "http://cas:9090");

        let objects = response.get("objects").unwrap().as_array().unwrap();

        // First object has upload action
        let upload_href = objects[0].get("actions").unwrap().get("upload").unwrap().get("href").unwrap().as_str().unwrap();
        assert_eq!(upload_href, "http://hub:8080/lfs/objects/abc123");

        // Second object has download action
        let download_href = objects[1].get("actions").unwrap().get("download").unwrap().get("href").unwrap().as_str().unwrap();
        assert_eq!(download_href, "http://hub:8080/lfs/objects/def456");
    }
}