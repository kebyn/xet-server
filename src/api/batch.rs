//! Git LFS Batch API
//!
//! POST /objects/batch - Git LFS batch operations
//!
//! This implements the Git LFS batch API to enable standard Git LFS clients
//! to work with the Xet server.

use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::api::auth::{extract_token_from_request, validate_jwt};
use crate::config::ServerConfig;
use crate::metrics::GLOBAL_METRICS;

#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub operation: String,
    pub transfers: Option<Vec<String>>,
    pub objects: Vec<BatchObject>,
}

#[derive(Debug, Deserialize)]
pub struct BatchObject {
    pub oid: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub transfer: String,
    pub objects: Vec<BatchResponseObject>,
}

#[derive(Debug, Serialize)]
pub struct BatchResponseObject {
    pub oid: String,
    pub size: u64,
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<BatchActions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchError>,
}

#[derive(Debug, Serialize)]
pub struct BatchActions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<BatchAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download: Option<BatchAction>,
}

#[derive(Debug, Serialize)]
pub struct BatchAction {
    pub href: String,
    pub header: std::collections::HashMap<String, String>,
    pub expires_in: u64,
}

#[derive(Debug, Serialize)]
pub struct BatchError {
    pub code: u32,
    pub message: String,
}

/// Maximum number of objects allowed in a single batch request.
/// Prevents a small request body from generating a disproportionately large
/// response (each entry includes headers, action URLs, etc.).
const MAX_BATCH_SIZE: usize = 1000;

/// Handle Git LFS batch API requests
pub async fn batch_operation(
    body: web::Json<BatchRequest>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();

    info!("Batch operation request: {} ({} objects)", body.operation, body.objects.len());

    // Bound logical cardinality — PayloadConfig bounds body bytes but not object count.
    if body.objects.len() > MAX_BATCH_SIZE {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "message": format!("Too many objects: {} exceeds limit of {}", body.objects.len(), MAX_BATCH_SIZE)
        }));
    }

    // Validate transfer protocol. This server only supports "basic" transfer.
    // Per Git LFS spec, the client sends preferred transfers in order; the server
    // picks the first supported one. If none are supported, reject the request.
    if let Some(ref transfers) = body.transfers {
        if !transfers.is_empty() && !transfers.iter().any(|t| t == "basic") {
            GLOBAL_METRICS.record_request(400);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "message": format!(
                    "Unsupported transfer protocol: {:?}. This server only supports 'basic'.",
                    transfers
                )
            }));
        }
    }

    // Extract and validate auth token
    let token = match extract_token_from_request(&req) {
        Some(t) => t,
        None => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "message": "Missing or invalid authorization"
            }));
        }
    };

    let claims = match validate_jwt(&token, &config.auth.jwt_secret) {
        Ok(c) => c,
        Err(_) => {
            GLOBAL_METRICS.record_request(401);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "message": "Invalid token"
            }));
        }
    };

    // Check scope based on operation
    let required_scope = if body.operation == "upload" { "write" } else { "read" };
    if !crate::api::auth::check_scope(&claims, required_scope) {
        GLOBAL_METRICS.record_request(403);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Forbidden().json(serde_json::json!({
            "message": "Insufficient scope"
        }));
    }

    // Calculate action URL expiry from JWT exp claim.
    // Actions become invalid when the JWT expires, so we surface that to the client.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expires_in = claims.exp.saturating_sub(now_secs as usize).max(1) as u64;

    // Process each object
    let mut response_objects = Vec::new();

    for obj in &body.objects {
        let response_obj = match body.operation.as_str() {
            "upload" => {
                // For upload, provide upload action
                let upload_url = format!(
                    "{}/lfs/objects/{}",
                    config.server.base_url(),
                    obj.oid
                );

                let mut headers = std::collections::HashMap::new();
                headers.insert("Authorization".to_string(), format!("Bearer {}", token));
                headers.insert("Content-Type".to_string(), "application/octet-stream".to_string());

                BatchResponseObject {
                    oid: obj.oid.clone(),
                    size: obj.size,
                    authenticated: true,
                    actions: Some(BatchActions {
                        upload: Some(BatchAction {
                            href: upload_url,
                            header: headers,
                            expires_in,
                        }),
                        download: None,
                    }),
                    error: None,
                }
            }
            "download" => {
                // For download, provide download action
                let download_url = format!(
                    "{}/lfs/objects/{}",
                    config.server.base_url(),
                    obj.oid
                );

                let mut headers = std::collections::HashMap::new();
                headers.insert("Authorization".to_string(), format!("Bearer {}", token));

                BatchResponseObject {
                    oid: obj.oid.clone(),
                    size: obj.size,
                    authenticated: true,
                    actions: Some(BatchActions {
                        upload: None,
                        download: Some(BatchAction {
                            href: download_url,
                            header: headers,
                            expires_in,
                        }),
                    }),
                    error: None,
                }
            }
            _ => {
                BatchResponseObject {
                    oid: obj.oid.clone(),
                    size: obj.size,
                    authenticated: false,
                    actions: None,
                    error: Some(BatchError {
                        code: 400,
                        message: format!("Unknown operation: {}", body.operation),
                    }),
                }
            }
        };

        response_objects.push(response_obj);
    }

    let response = BatchResponse {
        transfer: "basic".to_string(),
        objects: response_objects,
    };

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(response)
}
