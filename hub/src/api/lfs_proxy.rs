use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;
use crate::lfs_proxy::streaming::MaxBytesStream;
use crate::lfs_proxy::tokens::{extract_proxy_token, extract_token};
use crate::services::lfs_batch::{
    LfsBatchCasClient, LfsBatchRequest, LfsBatchService, LfsBatchServiceError,
};
use crate::services::lfs_object::{LfsObjectGuard, LfsObjectGuardError, LfsObjectOperation};
use crate::services::lfs_upload::{
    LfsUploadCasClient, LfsUploadService, LfsUploadServiceError, LfsUploadStoreError,
};
use actix_web::{HttpRequest, HttpResponse, web};
use futures_util::{StreamExt, TryStreamExt};
use std::sync::Arc;

/// Handle Git LFS batch request
///
/// # I2: Memory Usage Analysis
///
/// The entire CAS batch response is buffered in memory before URL rewriting. This is
/// required because URL rewriting mutates the JSON structure in-place (replacing CAS
/// URLs with Hub URLs and injecting short-lived proxy tokens).
///
/// ## Memory Bound
///
/// Memory usage is bounded by `MAX_BATCH_SIZE` (currently 1000 objects):
/// - Each object entry ≈ 200-500 bytes (OID, URLs, auth headers, actions)
/// - Worst case: 1000 × 500 bytes ≈ 500 KB per concurrent request
/// - With 100 concurrent batch requests: ≈ 50 MB peak
///
/// ## Why not streaming JSON?
///
/// Streaming JSON processing (e.g., `serde_json::from_reader` with iterator) would
/// require either:
/// - A custom streaming rewriter that preserves JSON structure (complex, error-prone)
/// - Two passes: first validate, then rewrite (defeats the purpose of streaming)
///
/// Given the hard cap at 1000 objects, the full-buffer approach is simpler and the
/// memory cost is well-bounded. If `MAX_BATCH_SIZE` is ever increased beyond 10000,
/// consider migrating to a streaming JSON library (e.g., `json-streams`).
///
/// ## Mitigations
///
/// 1. Hard cap at MAX_BATCH_SIZE (defense-in-depth, checked before CAS forward)
/// 2. Batch size logged for monitoring (see below)
/// 3. actix-web `PayloadConfig` limits request body size (50 MB default)
///
/// ## Known security limitation: no per-repo authorization on LFS object bytes
///
/// This handler authorizes only on token *scope* (read/write) and signs an
/// OID-bound proxy token for each requested object. It does NOT verify that the
/// requested OIDs actually belong to the repo in the request URL:
/// - The batch body's OIDs are decoupled from the URL's `{ns}/{repo}`, so a URL
///   ownership check would be trivially bypassable (attacker uses their own public
///   repo as the URL and puts a victim's private OID in the body).
/// - Several routes (`/objects/batch`, `/lfs/objects/{oid}`) carry no repo context
///   at all (standard git-lfs clients).
///
/// This is the content-addressed capability model inherent to LFS/Xet: knowing a
/// 64-hex content hash grants access to the bytes. The practical exposure is bounded
/// because the only paths that map a private repo to its content hashes (tree /
/// resolve / repo metadata endpoints) are repo-ownership gated — without an OID,
/// this path is not reachable.
///
/// A complete fix is an architectural change (out of scope here): add a
/// `MetadataStore` reverse lookup backed by a `file_entries(repo_id, cas_hash)`
/// index, verify every download OID against the repo, AND remove/repo-scope the
/// context-free `/objects/batch` and `/lfs/objects/{oid}` routes (otherwise the
/// reverse-lookup is bypassed via the bare routes). Dedup semantics (one OID may
/// legitimately belong to both a public and a private repo) must be handled too.
pub async fn lfs_batch(
    req: HttpRequest,
    body: web::Json<serde_json::Value>,
    token_store: web::Data<Arc<TokenStore>>,
    xet_signer: web::Data<Arc<XetSigner>>,
    cas_client: web::Data<Arc<CasClient>>,
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
    let body = body.into_inner();
    let cas_batch_client: Arc<dyn LfsBatchCasClient> = cas_client.get_ref().clone();
    let service = LfsBatchService::new(
        token_store.get_ref().clone(),
        xet_signer.get_ref().clone(),
        cas_batch_client,
    );

    let hub_base_url = config.server.base_url();
    match service
        .batch(LfsBatchRequest {
            user_token: &token,
            body: &body,
            hub_base_url: &hub_base_url,
        })
        .await
    {
        Ok(response) => HttpResponse::Ok().json(response),
        Err(err) => lfs_batch_error_response(err),
    }
}

fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}

fn lfs_batch_error_response(err: LfsBatchServiceError) -> HttpResponse {
    match err {
        LfsBatchServiceError::InvalidToken => HttpResponse::Unauthorized().json(error_json(
            "Invalid token".to_string(),
            "AuthenticationError",
        )),
        LfsBatchServiceError::Validation(message) => {
            HttpResponse::BadRequest().json(error_json(message, "ValidationError"))
        }
        LfsBatchServiceError::Authorization(message) => {
            HttpResponse::Forbidden().json(error_json(message, "AuthorizationError"))
        }
        LfsBatchServiceError::BadGateway(message) => {
            HttpResponse::BadGateway().json(error_json(message, "BadGateway"))
        }
        LfsBatchServiceError::Internal(message) => {
            HttpResponse::InternalServerError().json(error_json(message, "InternalError"))
        }
    }
}

fn lfs_object_guard_error_response(err: LfsObjectGuardError) -> HttpResponse {
    match err {
        LfsObjectGuardError::InvalidOid => HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid OID format",
            "error_type": "ValidationError"
        })),
        LfsObjectGuardError::InvalidToken => HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid or expired proxy token",
            "error_type": "AuthenticationError"
        })),
    }
}

fn lfs_upload_store_error_response(err: LfsUploadStoreError, temp_dir: &str) -> HttpResponse {
    match err {
        LfsUploadStoreError::CreateTempDir(message) => {
            tracing::error!("Failed to create temp dir {}: {}", temp_dir, message);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to initialize upload",
                "error_type": "InternalError"
            }))
        }
        LfsUploadStoreError::CreateTempFile(message) => {
            tracing::error!("Failed to create temp file: {}", message);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create temporary file",
                "error_type": "InternalError"
            }))
        }
        LfsUploadStoreError::PrepareTempFile(message) => {
            tracing::error!("Failed to detach temp file ownership: {}", message);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to prepare upload storage",
                "error_type": "InternalError"
            }))
        }
        LfsUploadStoreError::OpenTempFile(message)
        | LfsUploadStoreError::WriteTempFile(message) => {
            tracing::error!("Failed to write temp upload file: {}", message);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to write upload data",
                "error_type": "InternalError"
            }))
        }
        LfsUploadStoreError::ReadPayload(message) => {
            tracing::error!("Error reading payload: {}", message);
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Error reading upload data",
                "error_type": "ClientError"
            }))
        }
        LfsUploadStoreError::PayloadTooLarge { actual, max } => {
            tracing::warn!("Upload too large: {} bytes (max {})", actual, max);
            HttpResponse::PayloadTooLarge().json(serde_json::json!({
                "error": format!("Upload too large ({} bytes), max allowed: {} bytes", actual, max),
                "error_type": "PayloadTooLarge"
            }))
        }
        LfsUploadStoreError::FlushTempFile(message) => {
            tracing::error!("Failed to flush temp file: {}", message);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to finalize upload data",
                "error_type": "InternalError"
            }))
        }
    }
}

fn lfs_upload_service_error_response(
    err: LfsUploadServiceError,
    oid: &str,
    temp_dir: &str,
) -> HttpResponse {
    match err {
        LfsUploadServiceError::Store(err) => lfs_upload_store_error_response(err, temp_dir),
        LfsUploadServiceError::HashMismatch { computed, size } => {
            tracing::warn!(
                "Hash mismatch for OID {}: computed {} ({} bytes)",
                oid,
                computed,
                size
            );
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Hash mismatch: uploaded content does not match OID",
                "error_type": "ValidationError"
            }))
        }
        LfsUploadServiceError::Cas { status, message } => {
            let status_code = actix_web::http::StatusCode::from_u16(status)
                .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
            HttpResponse::build(status_code).json(serde_json::json!({
                "error": message,
                "error_type": "CasError"
            }))
        }
    }
}

/// Handle LFS object upload
/// Proxy LFS upload from client to CAS — streaming version
///
/// Memory usage: O(chunk_size) instead of O(file_size)
///
/// Flow:
/// 1. Receive web::Payload stream
/// 2. Write to temp file while computing SHA256 incrementally
/// 3. Verify hash == OID
/// 4. Stream temp file to CAS
/// 5. Clean up temp file
pub async fn lfs_upload(
    req: HttpRequest,
    path: web::Path<String>,
    payload: web::Payload,
    config: web::Data<crate::config::HubConfig>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    cas_client: web::Data<std::sync::Arc<CasClient>>,
) -> HttpResponse {
    // Extract token
    let token = match extract_proxy_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let oid = path.into_inner();
    let guard = LfsObjectGuard::new(xet_signer.get_ref().clone());
    if let Err(err) = guard.authorize(&token, &oid, LfsObjectOperation::Upload) {
        return lfs_object_guard_error_response(err);
    }

    let temp_dir = std::path::Path::new(&config.storage.upload_temp_dir);
    let upload_cas_client: Arc<dyn LfsUploadCasClient> = cas_client.get_ref().clone();
    let service = LfsUploadService::new(upload_cas_client);
    // CAS accepts the same OID/operation-bound proxy token that Hub just validated.
    match service
        .upload(
            &oid,
            &token,
            payload,
            temp_dir,
            config.storage.max_upload_size,
        )
        .await
    {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(err) => lfs_upload_service_error_response(err, &oid, &config.storage.upload_temp_dir),
    }
}

/// Handle LFS object download
pub async fn lfs_download(
    req: HttpRequest,
    path: web::Path<String>,
    xet_signer: web::Data<std::sync::Arc<XetSigner>>,
    cas_client: web::Data<std::sync::Arc<CasClient>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    // Extract token
    let token = match extract_proxy_token(&req) {
        Some(t) => t,
        None => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "Missing authorization",
                "error_type": "AuthenticationError"
            }));
        }
    };

    let oid = path.into_inner();
    let guard = LfsObjectGuard::new(xet_signer.get_ref().clone());
    if let Err(err) = guard.authorize(&token, &oid, LfsObjectOperation::Download) {
        return lfs_object_guard_error_response(err);
    }

    // C3 fix: Use streaming download with runtime size enforcement.
    // CAS /lfs/objects/{oid} accepts the OID/operation-bound proxy token that
    // Hub just validated; do not use an internal token for this public endpoint.
    match cas_client.proxy_lfs_download_streaming(&oid, &token).await {
        Ok((content_length, resp)) => {
            // Convert reqwest response body to actix-web streaming body
            let stream = resp.bytes_stream();

            // C3 fix: Wrap stream with MaxBytesStream for runtime size enforcement
            // This protects against CAS bugs that could cause unbounded data transfer
            let max_size = config.cas.max_download_size;
            let limited_stream = MaxBytesStream::new(
                stream.map(|result| result.map_err(std::io::Error::other)),
                max_size,
            );

            let mut builder = HttpResponse::Ok();
            builder.content_type("application/octet-stream");

            if content_length > 0 {
                builder.insert_header(("Content-Length", content_length.to_string()));
            }

            builder.streaming(limited_stream.map_err(actix_web::Error::from))
        }
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
            })),
        },
    }
}
