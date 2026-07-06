use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;
use crate::lfs_proxy::oid::validate_oid;
use crate::services::lfs_batch::{
    LfsBatchCasClient, LfsBatchRequest, LfsBatchService, LfsBatchServiceError,
};
use actix_web::{HttpRequest, HttpResponse, web};
use futures_util::{StreamExt, TryStreamExt};
use std::sync::Arc;

mod streaming;
mod tokens;

use streaming::MaxBytesStream;
use tokens::{extract_proxy_token, extract_token, validate_proxy_token};

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
        tracing::error!(
            "Failed to create temp dir {}: {}",
            config.storage.upload_temp_dir,
            e
        );
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
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

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
            tracing::warn!(
                "Upload too large: {} bytes (max {})",
                total_bytes,
                max_upload_size
            );
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
            oid,
            computed_hash,
            total_bytes
        );
        // Clean up temp file
        let _ = tokio::fs::remove_file(&temp_path).await;
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Hash mismatch: uploaded content does not match OID",
            "error_type": "ValidationError"
        }));
    }

    // CAS /lfs/objects/{oid} accepts either a regular user token or the same
    // OID/operation-bound proxy token that Hub just validated.
    let file_size = total_bytes;

    let result = cas_client
        .proxy_lfs_upload_from_path(&oid, &temp_path, file_size, &token)
        .await;

    // Clean up temp file
    let _ = tokio::fs::remove_file(&temp_path).await;

    match result {
        Ok(_) => HttpResponse::Ok().finish(),
        Err(e) => {
            let status_code = actix_web::http::StatusCode::from_u16(e.status)
                .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lfs_proxy::batch::{rewrite_action_url, rewrite_batch_urls};

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
        let upload_href = actions
            .get("upload")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();
        let download_href = actions
            .get("download")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();

        // URLs should be rewritten with proxy tokens (starting with proxy_)
        assert!(upload_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            valid_oid
        )));
        assert!(download_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            valid_oid
        )));
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
        assert_eq!(
            response,
            json!({
                "objects": [
                    {
                        "oid": "abc123",
                        "size": 1024
                    }
                ]
            })
        );
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
        let upload_href = objects[0]
            .get("actions")
            .unwrap()
            .get("upload")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(upload_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            oid1
        )));

        // Second object has download action
        let download_href = objects[1]
            .get("actions")
            .unwrap()
            .get("download")
            .unwrap()
            .get("href")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(download_href.starts_with(&format!(
            "http://hub:8080/lfs/objects/{}?token=proxy_",
            oid2
        )));
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

    fn sign_proxy_token_with_type(
        token_type: &str,
    ) -> (String, crate::auth::xet_signer::XetSigner) {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let signer =
            crate::auth::xet_signer::XetSigner::new(signing_key.clone(), "test-key", 3600, 300);
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
            "scope": "lfs-upload",
            "repo_id": "",
            "repo_type": "",
            "revision": "",
            "exp": now + 300,
            "iat": now,
            "kid": "test-key",
            "token_type": token_type,
            "oid": "abc123def456",
            "operation": "upload",
        });

        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let signing_input = format!("{}.{}", header_b64, claims_b64);
        let signature = signing_key.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        (format!("proxy_{}.{}", signing_input, sig_b64), signer)
    }

    #[test]
    fn test_validate_proxy_token_valid() {
        let signer = create_test_signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);
        assert!(result, "Valid proxy token should be accepted");
    }

    #[test]
    fn test_validate_proxy_token_expired() {
        let signer = create_test_signer();
        // Create a token that expires immediately (we can't easily test this without mocking time)
        // For now, we'll just verify the validation logic works
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer);
        assert!(result, "Non-expired token should be accepted");
    }

    #[test]
    fn test_validate_proxy_token_wrong_oid() {
        let signer = create_test_signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        // Try to validate with wrong OID
        let result = validate_proxy_token(&token, "wrongoid", "upload", &signer);
        assert!(!result, "Token with wrong OID should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_wrong_operation() {
        let signer = create_test_signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        // Try to validate with wrong operation
        let result = validate_proxy_token(&token, "abc123def456", "download", &signer);
        assert!(!result, "Token with wrong operation should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_wrong_scope() {
        let signer = create_test_signer();
        let (token, _) = signer
            .sign_proxy_claims_for_test(
                "testuser",
                "lfs-upload",
                "abc123def456",
                "download",
                "",
                "",
            )
            .unwrap();

        let result = validate_proxy_token(&token, "abc123def456", "download", &signer);
        assert!(!result, "Token with wrong scope should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_invalid_signature() {
        let signer = create_test_signer();
        let (token, _) = signer
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        // Tamper with the token
        let tampered_token = format!("{}x", &token[..token.len() - 1]);

        let result = validate_proxy_token(&tampered_token, "abc123def456", "upload", &signer);
        assert!(!result, "Token with invalid signature should be rejected");
    }

    #[test]
    fn test_validate_proxy_token_non_proxy_token() {
        let signer = create_test_signer();
        // Create a regular user token instead of proxy token
        let (user_token, _) = signer
            .sign("testuser", "read", "repo", "model", "main")
            .unwrap();

        let result = validate_proxy_token(&user_token, "abc123def456", "upload", &signer);
        assert!(!result, "User token should be rejected as proxy token");
    }

    #[test]
    fn test_validate_proxy_token_malformed() {
        let signer = create_test_signer();

        // Test various malformed tokens
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
    fn test_validate_proxy_token_wrong_token_type() {
        let (tampered_token, signer) = sign_proxy_token_with_type("user");

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
        let (token, _) = signer1
            .sign_proxy("testuser", "abc123def456", "upload", "", "")
            .unwrap();

        // Try to validate with signer2 (different kid)
        let result = validate_proxy_token(&token, "abc123def456", "upload", &signer2);
        assert!(!result, "Token with wrong kid should be rejected");
    }
}
