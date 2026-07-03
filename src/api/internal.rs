//! Internal API endpoints for Hub-to-CAS communication.
//!
//! These endpoints are used by HuggingFace Hub to query blob storage state
//! and check blob accessibility. They require the "internal" scope.

use actix_web::{HttpResponse, web};
use serde::Serialize;
use tracing::{info, warn};

use crate::api::auth::AuthVerifier;
use crate::api::guard::{AuthNeed, require_auth};
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::StorageBackend;

/// Error response for internal endpoints
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Get storage state for a blob by OID.
///
/// Stateless logic:
/// - Check MetadataIndex for xet data → return xet_only
/// - Check raw blob in storage → return raw_only
/// - Not found → 404
///
/// Requires "internal" scope.
pub async fn get_blob_state(
    path: web::Path<String>,
    auth: web::Data<AuthVerifier>,
    storage: web::Data<Box<dyn StorageBackend>>,
    index: web::Data<MetadataIndex>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let oid = path.into_inner();

    // Validate oid format (should be a hex hash)
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: "Invalid oid format, expected 64-character hex string".to_string(),
        });
    }

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(
        &req,
        &auth,
        AuthNeed::Internal("Internal endpoint requires internal token type and scope"),
    ) {
        return rej.respond(start);
    }

    // Check MetadataIndex first
    if index.get_file_refs(&oid).is_some() {
        info!("Internal state query for {}: xet_only", oid);
        GLOBAL_METRICS.record_request(200);
        GLOBAL_METRICS.record_latency(start);
        // Get actual blob size from storage (M4 fix: log errors instead of silently returning 0)
        let size = match storage.get_size(&format!("lfs/objects/{}", oid)).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to get size for xet_only blob {}: {}", oid, e);
                0
            }
        };
        return HttpResponse::Ok().json(serde_json::json!({
            "state": "xet_only",
            "xet_file_id": oid,
            "size": size,
            "sha256": oid,
            "converted_at": null
        }));
    }

    // Check raw blob
    let object_key = format!("lfs/objects/{}", oid);
    match storage.exists(&object_key).await {
        Ok(true) => {
            info!("Internal state query for {}: raw_only", oid);
            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_latency(start);
            // Get actual blob size from storage (M4 fix: log errors instead of silently returning 0)
            let size = match storage.get_size(&object_key).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to get size for raw_only blob {}: {}", oid, e);
                    0
                }
            };
            HttpResponse::Ok().json(serde_json::json!({
                "state": "raw_only",
                "xet_file_id": null,
                "size": size,
                "sha256": oid,
                "converted_at": null
            }))
        }
        Ok(false) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::NotFound().json(ErrorResponse {
                error: format!("Blob not found: {}", oid),
            })
        }
        Err(e) => {
            // I3 fix: Log internal error details but don't leak them to the client.
            // The error message could contain file paths, S3 bucket names, or other
            // infrastructure details that shouldn't be exposed even on internal endpoints.
            warn!("Storage error checking blob {}: {}", oid, e);
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_error();
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::InternalServerError().json(ErrorResponse {
                error: "Internal storage error".to_string(),
            })
        }
    }
}

/// Check if blob is accessible via HEAD request.
///
/// Stateless logic:
/// - Check MetadataIndex for xet data → X-Storage-State: xet_only
/// - Check raw blob in storage → X-Storage-State: raw_only
/// - Not found → 404
///
/// Requires "internal" scope.
pub async fn head_blob(
    path: web::Path<String>,
    auth: web::Data<AuthVerifier>,
    storage: web::Data<Box<dyn StorageBackend>>,
    index: web::Data<MetadataIndex>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let oid = path.into_inner();

    // Validate oid format
    if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: "Invalid oid format, expected 64-character hex string".to_string(),
        });
    }

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(
        &req,
        &auth,
        AuthNeed::Internal("Internal endpoint requires internal token type and scope"),
    ) {
        return rej.respond(start);
    }

    // Check MetadataIndex first
    if index.get_file_refs(&oid).is_some() {
        GLOBAL_METRICS.record_request(200);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::Ok()
            .insert_header(("X-Storage-State", "xet_only"))
            .insert_header(("X-File-Id", oid.as_str()))
            .finish();
    }

    // Check raw blob
    let object_key = format!("lfs/objects/{}", oid);
    match storage.exists(&object_key).await {
        Ok(true) => {
            GLOBAL_METRICS.record_request(200);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::Ok()
                .insert_header(("X-Storage-State", "raw_only"))
                .finish()
        }
        Ok(false) => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::NotFound().finish()
        }
        Err(_) => {
            GLOBAL_METRICS.record_request(500);
            GLOBAL_METRICS.record_latency(start);
            HttpResponse::InternalServerError().finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{AuthVerifier, KeyPair, XetClaims, sign_xet_token};
    use crate::config::AuthConfig;
    use crate::storage::local::LocalStorage;
    use actix_web::{App, test, web};
    use bytes::Bytes;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;

    fn create_test_config() -> (KeyPair, AuthVerifier) {
        let kp = KeyPair::generate();
        let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();

        let temp_dir = tempdir().unwrap();
        let temp_path = temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
        std::fs::write(&temp_path, &public_key_pem).unwrap();

        let temp_path_str = temp_path.to_str().unwrap().to_string();
        std::mem::forget(temp_dir);

        let auth_config = AuthConfig {
            public_key_path: temp_path_str,
            trusted_kids: vec![kp.kid()],
            private_key_path: None,
            signing_kid: None,
        };

        let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();
        (kp, auth_verifier)
    }

    fn create_internal_token(kp: &KeyPair) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let claims = XetClaims {
            sub: "hub-service".to_string(),
            scope: "internal".to_string(),
            repo_id: "test/repo".to_string(),
            repo_type: "model".to_string(),
            revision: "main".to_string(),
            exp: now + 3600,
            iat: now,
            kid: kp.kid(),
            token_type: "internal".to_string(),
            oid: None,
            operation: None,
        };

        sign_xet_token(&claims, kp).unwrap()
    }

    #[actix_web::test]
    async fn test_internal_head_ignores_unverified_shard_poisoning() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        storage
            .put(
                "shards/invalid-poison-shard",
                Bytes::from_static(b"not a shard"),
            )
            .await
            .unwrap();

        let index = MetadataIndex::new();
        let (kp, auth) = create_test_config();
        let token = create_internal_token(&kp);
        let victim_oid = "a".repeat(64);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(index))
                .route("/internal/blob/{oid}", web::head().to(head_blob)),
        )
        .await;

        let req = test::TestRequest::default()
            .method(actix_web::http::Method::HEAD)
            .uri(&format!("/internal/blob/{}", victim_oid))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }
}
