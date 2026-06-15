use actix_web::{web, HttpRequest, HttpResponse};
use futures_util::{Stream, StreamExt, TryStreamExt};
use pin_project::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};
use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;

/// Maximum number of objects allowed in a single batch request.
/// Mirrors CAS-side limit for defense-in-depth — reject oversized batches early
/// at the Hub rather than forwarding to CAS and returning 502.
const MAX_BATCH_SIZE: usize = 1000;

/// C3 fix: Stream wrapper that enforces a maximum byte limit during streaming.
/// This prevents unbounded data transfer even if Content-Length header is missing or incorrect.
#[pin_project]
struct MaxBytesStream<S> {
    #[pin]
    stream: S,
    max_bytes: u64,
    bytes_read: u64,
}

impl<S, B> MaxBytesStream<S>
where
    S: Stream<Item = Result<B, std::io::Error>>,
    B: AsRef<[u8]>,
{
    fn new(stream: S, max_bytes: u64) -> Self {
        Self {
            stream,
            max_bytes,
            bytes_read: 0,
        }
    }
}

impl<S, B> Stream for MaxBytesStream<S>
where
    S: Stream<Item = Result<B, std::io::Error>>,
    B: AsRef<[u8]>,
{
    type Item = Result<B, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match this.stream.poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let chunk_len = chunk.as_ref().len() as u64;
                *this.bytes_read += chunk_len;

                if *this.bytes_read > *this.max_bytes {
                    Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Stream exceeded maximum size of {} bytes", this.max_bytes),
                    ))))
                } else {
                    Poll::Ready(Some(Ok(chunk)))
                }
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Extract token from Authorization header (Bearer/Basic).
///
/// I5 fix: Query parameter tokens have been removed from this function to prevent
/// token leakage in server logs, proxy logs, browser history, and referrer headers.
/// Query parameter tokens are only supported in `extract_proxy_token()` for LFS
/// redirect scenarios where proxy tokens (short-lived, OID-bound) are used.
fn extract_token(req: &HttpRequest) -> Option<String> {
    // Try Authorization header
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

    None
}

/// Extract proxy token for LFS download/upload operations.
/// Prefers query parameter token (?token=proxy_xxx) over Authorization header,
/// because clients following 302 redirects from /resolve/ send their user token
/// in the Authorization header, but the proxy token is in the URL query param.
///
/// Security: Query parameter tokens leak in logs. Proxy tokens are acceptable here
/// because they are short-lived (5 min TTL) and OID-bound, limiting blast radius.
fn extract_proxy_token(req: &HttpRequest) -> Option<String> {
    // First try query parameter (this is where proxy tokens are placed in redirects)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=')
                && key == "token"
                && let Ok(decoded) = percent_encoding::percent_decode_str(value).decode_utf8()
                && decoded.starts_with("proxy_")
            {
                // I5 fix: Structured audit log for query parameter token usage
                tracing::info!(
                    path = %req.uri().path(),
                    "Proxy token received via query parameter (short-lived, OID-bound)"
                );
                return Some(decoded.into_owned());
            }
        }
    }

    // Fall back to Authorization header
    extract_token(req)
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
                    // I2 fix: Handle signing errors gracefully - skip objects that fail to sign
                    if let Some(upload_action) = actions.get_mut("upload") {
                        match signer.sign_proxy(username, &oid, "upload", "", "") {
                            Ok((proxy_token, _)) => {
                                if !rewrite_action_url(upload_action, &hub_url, &proxy_token)
                                    && let Some(actions_obj) = actions.as_object_mut() {
                                        actions_obj.remove("upload");
                                    }
                            }
                            Err(e) => {
                                tracing::error!("Failed to sign proxy token for upload {}: {}", oid, e);
                                // Remove the action if we can't sign a token for it
                                if let Some(actions_obj) = actions.as_object_mut() {
                                    actions_obj.remove("upload");
                                }
                            }
                        }
                    }
                    if let Some(download_action) = actions.get_mut("download") {
                        match signer.sign_proxy(username, &oid, "download", "", "") {
                            Ok((proxy_token, _)) => {
                                if !rewrite_action_url(download_action, &hub_url, &proxy_token)
                                    && let Some(actions_obj) = actions.as_object_mut() {
                                        actions_obj.remove("download");
                                    }
                            }
                            Err(e) => {
                                tracing::error!("Failed to sign proxy token for download {}: {}", oid, e);
                                if let Some(actions_obj) = actions.as_object_mut() {
                                    actions_obj.remove("download");
                                }
                            }
                        }
                    }
                }
            }
        }
}

/// Rewrite a single action's URL and auth header with proxy token.
/// 返回 true 表示成功重写;false 表示 href 无法解析,调用方应丢弃该 action
/// 以免把内部 CAS URL 泄露给客户端。
fn rewrite_action_url(action: &mut serde_json::Value, hub_url: &url::Url, proxy_token: &str) -> bool {
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

    let Some(href) = new_href else {
        // 无法解析 href:不透传原始(内部 CAS)URL,交由调用方丢弃 action。
        return false;
    };
    if let Some(action_obj) = action.as_object_mut() {
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
    true
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
        None => {
            // M1: Use safe slicing to avoid panic on non-ASCII boundaries
            let token_preview = token.get(..30).unwrap_or(token);
            tracing::error!("validate_proxy_token: verify_proxy_token failed for token starting with: {}...", token_preview);
            return false;
        }
    };

    // Check token type (not checked by verify_proxy_token)
    if claims.token_type != "proxy" {
        tracing::error!("validate_proxy_token: token_type mismatch: {} != proxy", claims.token_type);
        return false;
    }

    // I3 FIX: Expiration is already checked by verify_proxy_token -> verify_token_inner.
    // Removed duplicate expiration check.

    // Check OID matches
    if claims.oid.as_deref() != Some(expected_oid) {
        tracing::error!("validate_proxy_token: oid mismatch: {:?} != {}", claims.oid, expected_oid);
        return false;
    }

    // Check operation matches
    if claims.operation.as_deref() != Some(expected_operation) {
        tracing::error!("validate_proxy_token: operation mismatch: {:?} != {}", claims.operation, expected_operation);
        return false;
    }

    true
}

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

    let token_info = match token_store.validate_token(&token).await {
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

    // I3 fix: Validate token scope based on operation type
    // LFS batch operation requires appropriate scope (upload -> write, download -> read)
    let operation = body.get("operation")
        .and_then(|o| o.as_str())
        .unwrap_or("download");  // Default to download if not specified
    let required_scope = match operation {
        "upload" => "write",
        "download" => "read",
        _ => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid operation: {}", operation),
                "error_type": "ValidationError"
            }));
        }
    };

    // C5 fix: Check scope using exact match or split-based matching instead of contains()
    // "write" implies "read" — a user with write access can always download
    let has_scope = token_info.scope == required_scope
        || token_info.scope == "read write"
        || token_info.scope.split_whitespace().any(|s| s == required_scope)
        || (required_scope == "read" && token_info.scope.split_whitespace().any(|s| s == "write"));
    if !has_scope {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": format!("Token scope '{}' insufficient for {} operation (requires '{}')",
                token_info.scope, operation, required_scope),
            "error_type": "AuthorizationError"
        }));
    }

    // Validate batch size before forwarding to CAS (defense-in-depth)
    let object_count = body.get("objects")
        .and_then(|o| o.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if object_count > MAX_BATCH_SIZE {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Too many objects: {} exceeds limit of {}", object_count, MAX_BATCH_SIZE),
            "error_type": "ValidationError"
        }));
    }
    // Log batch size for monitoring (I2: helps operators track memory usage patterns)
    tracing::debug!(
        object_count,
        user = %token_info.username,
        "Processing LFS batch request"
    );

    // Generate internal token for CAS
    // I2 fix: Handle signing errors - return HTTP 500 if we can't create internal token
    let (internal_token, _) = match xet_signer.sign_internal() {
        Ok(result) => result,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to sign internal token: {}", e),
                "error_type": "InternalError"
            }));
        }
    };

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
    mut payload: web::Payload,
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

    // Validate OID format
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

    // Create temp directory if it doesn't exist
    let temp_dir = std::path::Path::new(&config.storage.upload_temp_dir);
    if let Err(e) = tokio::fs::create_dir_all(temp_dir).await {
        tracing::error!("Failed to create temp dir {}: {}", config.storage.upload_temp_dir, e);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Failed to initialize upload",
            "error_type": "InternalError"
        }));
    }

    // Create temporary file
    let temp_file = match tempfile::Builder::new()
        .prefix("upload-")
        .tempfile_in(temp_dir)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("Failed to create temp file: {}", e);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to create temporary file",
                "error_type": "InternalError"
            }));
        }
    };

    let temp_path = temp_file.path().to_path_buf();

    // I4 fix: Detach NamedTempFile ownership — we manage cleanup explicitly on all exit paths.
    // This prevents Drop from racing with our tokio::fs::remove_file calls.
    // M6 fix: Check return value instead of ignoring potential errors.
    if let Err(e) = temp_file.keep() {
        tracing::error!("Failed to detach temp file ownership: {}", e);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Failed to prepare upload storage",
            "error_type": "InternalError"
        }));
    }

    // Stream payload to temp file while computing hash
    use sha2::{Sha256, Digest};
    use tokio::io::AsyncWriteExt;
    use futures_util::StreamExt;

    let mut hasher = Sha256::new();
    let mut file_writer = match tokio::fs::File::create(&temp_path).await {
        Ok(f) => tokio::io::BufWriter::new(f),
        Err(e) => {
            tracing::error!("Failed to open temp file for writing: {}", e);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to write upload data",
                "error_type": "InternalError"
            }));
        }
    };

    let mut total_bytes: u64 = 0;
    // M2: Use configurable max upload size from config
    let max_upload_size = config.storage.max_upload_size;

    while let Some(chunk_result) = payload.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Error reading payload: {}", e);
                // I4 fix: Explicit temp file cleanup on error path
                let _ = tokio::fs::remove_file(&temp_path).await;
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Error reading upload data",
                    "error_type": "ClientError"
                }));
            }
        };

        total_bytes += chunk.len() as u64;
        if total_bytes > max_upload_size {
            tracing::warn!("Upload too large: {} bytes (max {})", total_bytes, max_upload_size);
            // I4 fix: Explicit temp file cleanup on error path
            let _ = tokio::fs::remove_file(&temp_path).await;
            return HttpResponse::PayloadTooLarge().json(serde_json::json!({
                "error": format!("Upload too large ({} bytes), max allowed: {} bytes", total_bytes, max_upload_size),
                "error_type": "PayloadTooLarge"
            }));
        }

        hasher.update(&chunk);
        if let Err(e) = file_writer.write_all(&chunk).await {
            tracing::error!("Failed to write to temp file: {}", e);
            // I4 fix: Explicit temp file cleanup on error path
            let _ = tokio::fs::remove_file(&temp_path).await;
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to write upload data",
                "error_type": "InternalError"
            }));
        }
    }

    // Flush and close file
    if let Err(e) = file_writer.flush().await {
        tracing::error!("Failed to flush temp file: {}", e);
        // I4 fix: Explicit temp file cleanup on error path
        let _ = tokio::fs::remove_file(&temp_path).await;
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Failed to finalize upload data",
            "error_type": "InternalError"
        }));
    }
    drop(file_writer);

    // Verify hash
    let computed_hash = hex::encode(hasher.finalize());
    if computed_hash != oid {
        tracing::warn!(
            "Hash mismatch for OID {}: computed {} ({} bytes)",
            oid, computed_hash, total_bytes
        );
        // Clean up temp file
        let _ = tokio::fs::remove_file(&temp_path).await;
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Hash mismatch: uploaded content does not match OID",
            "error_type": "ValidationError"
        }));
    }

    // Stream temp file to CAS
    let (internal_token, _) = match xet_signer.sign_internal() {
        Ok(result) => result,
        Err(e) => {
            // Clean up temp file
            let _ = tokio::fs::remove_file(&temp_path).await;
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to sign internal token: {}", e),
                "error_type": "InternalError"
            }));
        }
    };
    let file_size = total_bytes;

    let result = cas_client.proxy_lfs_upload_from_path(
        &oid,
        &temp_path,
        file_size,
        &internal_token,
    ).await;

    // Clean up temp file
    let _ = tokio::fs::remove_file(&temp_path).await;

    match result {
        Ok(_) => HttpResponse::Ok().finish(),
        Err(e) => {
            let status_code = actix_web::http::StatusCode::from_u16(e.status).unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
            HttpResponse::build(status_code).json(serde_json::json!({
                "error": e.message,
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
    // I2 fix: Handle signing errors - return HTTP 500 if we can't create internal token
    let (internal_token, _) = match xet_signer.sign_internal() {
        Ok(result) => result,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to sign internal token: {}", e),
                "error_type": "InternalError"
            }));
        }
    };

    // C3 fix: Use streaming download with runtime size enforcement
    match cas_client.proxy_lfs_download_streaming(&oid, &internal_token).await {
        Ok((content_length, resp)) => {
            // Convert reqwest response body to actix-web streaming body
            let stream = resp.bytes_stream();

            // C3 fix: Wrap stream with MaxBytesStream for runtime size enforcement
            // This protects against CAS bugs that could cause unbounded data transfer
            let max_size = config.cas.max_download_size;
            let limited_stream = MaxBytesStream::new(
                stream.map(|result| result.map_err(std::io::Error::other)),
                max_size
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
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_action_url_drops_on_parse_failure() {
        let hub = url::Url::parse("https://hub.example.com").unwrap();
        let mut action = serde_json::json!({"href": "not a valid url at all"});
        let ok = rewrite_action_url(&mut action, &hub, "proxy_tok");
        assert!(!ok, "无法解析的 href 应返回 false 以便调用方丢弃 action");
    }

    #[test]
    fn test_rewrite_action_url_rewrites_valid() {
        let hub = url::Url::parse("https://hub.example.com:9000").unwrap();
        let mut action = serde_json::json!({"href": "http://cas-internal:5000/lfs/objects/abc"});
        let ok = rewrite_action_url(&mut action, &hub, "proxy_tok");
        assert!(ok);
        let href = action.get("href").unwrap().as_str().unwrap();
        assert!(href.contains("hub.example.com"));
        assert!(href.contains("token=proxy_tok"));
        assert!(!href.contains("cas-internal"));
    }
    use serde_json::json;

    #[test]
    fn test_rewrite_batch_urls() {
        use crate::auth::xet_signer::XetSigner;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let signer = XetSigner::new(signing_key, "test-key", 3600, 300);

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
        let signer = XetSigner::new(signing_key, "test-key", 3600, 300);

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
        let signer = XetSigner::new(signing_key, "test-key", 3600, 300);

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
        XetSigner::new(signing_key, "test-key", 3600, 300)
    }

    #[test]
    fn test_validate_proxy_token_valid() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "").unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);
        assert!(result, "Valid proxy token should be accepted");
    }

    #[test]
    fn test_validate_proxy_token_expired() {
        let signer = create_test_signer();
        // Create a token that expires immediately (we can't easily test this without mocking time)
        // For now, we'll just verify the validation logic works
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "").unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);
        assert!(result, "Non-expired token should be accepted");
    }

    #[test]
    fn test_validate_proxy_token_wrong_oid() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "").unwrap();

        // Try to validate with wrong OID
        let result = validate_proxy_token(&token, "wrongoid", "upload", &signer);
        assert!(!result, "Token with wrong OID should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_wrong_operation() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "").unwrap();

        // Try to validate with wrong operation
        let result = validate_proxy_token(&token, "abc123def456", "download", &signer);
        assert!(!result, "Token with wrong operation should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_invalid_signature() {
        let signer = create_test_signer();
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "").unwrap();

        // Tamper with the token
        let tampered_token = format!("{}x", &token[..token.len()-1]);

        let result = validate_proxy_token(&tampered_token, "abc123def456", "upload", &signer);
        assert!(!result, "Token with invalid signature should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_non_proxy_token() {
        let signer = create_test_signer();
        // Create a regular user token instead of proxy token
        let (user_token, _) = signer.sign("testuser", "read", "repo", "model", "main").unwrap();

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
        let (token, _) = signer.sign_proxy("testuser", "abc123def456", "upload", "", "").unwrap();

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
        let signer1 = XetSigner::new(signing_key1, "key-id-1", 3600, 300);

        let mut csprng2 = OsRng;
        let signing_key2 = SigningKey::generate(&mut csprng2);
        let signer2 = XetSigner::new(signing_key2, "key-id-2", 3600, 300);

        // Sign token with signer1
        let (token, _) = signer1.sign_proxy("testuser", "abc123def456", "upload", "", "").unwrap();

        // Try to validate with signer2 (different kid)
        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer2);
        assert!(!result, "Token with wrong kid should be rejected");
    }
}
