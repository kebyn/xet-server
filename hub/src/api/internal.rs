//! Internal API endpoints for CAS-to-Hub communication
//!
//! These endpoints are used by CAS for GC and other internal operations.
//! They require internal scope authentication.

use actix_web::{web, HttpRequest, HttpResponse};
use crate::auth::xet_signer::XetSigner;
use crate::metadata::MetadataStore;

/// GET /internal/referenced-hashes
///
/// Returns all CAS hashes referenced by file_tree entries (for GC).
/// Requires internal scope authentication.
///
/// Response format:
/// ```json
/// {
///   "hashes": ["abc123...", "def456...", ...],
///   "count": 1234
/// }
/// ```
pub async fn get_referenced_hashes(
    req: HttpRequest,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
) -> HttpResponse {
    // Extract and verify internal token
    let token = match extract_internal_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing or invalid authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    // Verify token - use verify_internal_token for internal tokens (I2 fix)
    let claims = match xet_signer.verify_internal_token(&token) {
        Some(c) => c,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Invalid token",
                "error_type": "AuthenticationError"
            }));
        }
    };

    // Verify it's an internal token - check both scope and token_type for defense-in-depth
    // C1 fix: Prevent proxy tokens from accessing internal endpoints
    if claims.scope != "internal" || claims.token_type != "internal" {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "Internal endpoint requires internal token type and scope",
            "error_type": "AuthorizationError"
        }));
    }

    // Query referenced hashes
    match metadata.get_all_referenced_hashes().await {
        Ok(hashes) => {
            let hash_vec: Vec<String> = hashes.into_iter().collect();
            HttpResponse::Ok().json(serde_json::json!({
                "hashes": hash_vec,
                "count": hash_vec.len(),
            }))
        }
        Err(e) => {
            tracing::error!("Failed to query referenced hashes: {}", e);
            // C1 fix: Don't leak database error details to client
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal server error",
                "error_type": "InternalError"
            }))
        }
    }
}

/// Extract Bearer token from Authorization header
fn extract_internal_token(req: &HttpRequest) -> Option<String> {
    let auth_header = req.headers().get("Authorization")?;
    let auth_str = auth_header.to_str().ok()?;

    auth_str.strip_prefix("Bearer ").map(|token| token.to_string())
}
