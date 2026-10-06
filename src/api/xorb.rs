//! Xorb Upload API
//!
//! POST /v1/xorbs/{prefix}/{hash} - Upload xorb objects (streaming)

use actix_web::{HttpResponse, web};
use futures_util::StreamExt;
use serde::Serialize;
use tracing::{error, info};

use crate::api::auth::AuthVerifier;
use crate::api::guard::{AuthNeed, require_auth};
use crate::config::ServerConfig;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::{StorageBackend, StorageError};
use crate::types::MerkleHash;
use crate::util::TempFile;

#[derive(Serialize)]
struct XorbUploadResponse {
    was_inserted: bool,
}

/// Upload a xorb object via streaming.
///
/// Data is streamed to a temp file with incremental BLAKE3 hashing,
/// then verified from disk and moved to final storage via rename.
pub async fn upload_xorb(
    path: web::Path<(String, String)>,
    mut payload: web::Payload,
    storage: web::Data<Box<dyn StorageBackend>>,
    auth: web::Data<AuthVerifier>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let (prefix, hash_str) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Parse hash
    let expected_hash = match MerkleHash::from_hex(&hash_str) {
        Ok(h) => h,
        Err(e) => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid hash format: {}", e)
            }));
        }
    };

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(&req, &auth, AuthNeed::Scope("write")) {
        return rej.respond();
    }

    // M7 fix: Use a more reasonable pre-check threshold instead of max_body_size_bytes.
    // Previously checked for 2GB free space before every upload, rejecting small uploads
    // on disks with limited (but sufficient) space. Use min(max_body_size, 100MB) as a
    // practical minimum: enough for most uploads, without being overly conservative.
    let temp_dir = config.storage.resolve_upload_temp_dir();
    let check_bytes = std::cmp::min(
        config.server.max_body_size_bytes() as u64,
        100 * 1024 * 1024,
    );
    if let Err(e) = crate::util::disk::ensure_dir_and_check_space(&temp_dir, check_bytes).await {
        error!("Insufficient disk space: {}", e);
        return HttpResponse::InsufficientStorage().json(serde_json::json!({
            "error": "Insufficient storage"
        }));
    }

    // Stream payload to temp file with incremental BLAKE3 hashing
    let mut temp_file = match TempFile::create(&temp_dir).await {
        Ok(tf) => tf,
        Err(e) => {
            error!("Failed to create temp file: {}", e);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }));
        }
    };

    let max_bytes = config.server.max_body_size_bytes() as u64;
    let mut total_bytes: u64 = 0;

    while let Some(chunk_result) = payload.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                error!("Payload stream error: {}", e);
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "Invalid upload stream"
                }));
            }
        };

        total_bytes += chunk.len() as u64;
        if total_bytes > max_bytes {
            return HttpResponse::PayloadTooLarge().json(serde_json::json!({
                "error": format!("Upload exceeds maximum size of {} MB", config.server.max_body_size_mb)
            }));
        }

        if let Err(e) = temp_file.write_all(&chunk).await {
            error!("Failed to write to temp file: {}", e);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }));
        }
    }

    if let Err(e) = temp_file.sync_all().await {
        error!("Failed to sync temp file: {}", e);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": crate::api::INTERNAL_ERROR_MESSAGE
        }));
    }

    // Verify xorb structure and identity from temp file on disk.
    // Run off the async runtime: the check reads and hashes the whole file
    // synchronously (up to max_body_size), which would stall a tokio worker.
    let temp_path = temp_file.path().to_path_buf();
    let verify_path = temp_path.clone();
    let xorb_info = match tokio::task::spawn_blocking(move || {
        crate::format::xorb::verify_xorb_from_file_with_info(&verify_path)
    })
    .await
    {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            error!(
                "Xorb verification failed for {}: {}",
                temp_path.display(),
                e
            );
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Xorb verification failed"
            }));
        }
        Err(join_err) => {
            error!(
                "Xorb verification task failed for {}: {}",
                temp_path.display(),
                join_err
            );
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }));
        }
    };

    if xorb_info.xorb_hash != expected_hash {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("Hash mismatch: expected {}, got {}", expected_hash.to_hex(), xorb_info.xorb_hash.to_hex())
        }));
    }

    // Check if xorb already exists.
    // Note: There is a TOCTOU race between exists() and put_from_path() below.
    // For content-addressed storage this is acceptable because:
    // 1. Same hash = same content, so concurrent uploads are idempotent
    // 2. The was_inserted field is best-effort under concurrency — it may not
    //    reflect which concurrent writer actually won the race, but this only
    //    affects metrics/dedup accounting, not data integrity.
    // For strict dedup accounting, storage backends should implement put_if_absent.
    // C1 fix: Use xorbs/{hash} format to match conversion pipeline and LFS download.
    let xorb_hash_hex = xorb_info.xorb_hash.to_hex();
    let xorb_key = format!("xorbs/{}", xorb_hash_hex);
    let already_exists = match storage.exists(&xorb_key).await {
        Ok(exists) => exists,
        Err(e) => {
            error!("Failed to check xorb existence: {}", e);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }));
        }
    };

    if already_exists {
        GLOBAL_METRICS.record_storage_operation();
        // temp_file auto-cleaned by Drop
        return HttpResponse::Ok().json(XorbUploadResponse {
            was_inserted: false,
        });
    }

    // Local storage may rename the file; remote storage uploads it and leaves
    // source cleanup to TempFile's RAII ownership.
    if let Err(e) = temp_file.store(storage.get_ref().as_ref(), &xorb_key).await {
        error!("Failed to store xorb: {}", e);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": crate::api::INTERNAL_ERROR_MESSAGE
        }));
    }

    info!("Uploaded xorb {} ({} bytes)", xorb_hash_hex, total_bytes);

    GLOBAL_METRICS.record_storage_operation();
    GLOBAL_METRICS.record_upload_bytes(total_bytes);

    HttpResponse::Ok().json(XorbUploadResponse { was_inserted: true })
}

/// Download a xorb object
pub async fn download_xorb(
    path: web::Path<(String, String)>,
    storage: web::Data<Box<dyn StorageBackend>>,
    auth: web::Data<AuthVerifier>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let (prefix, hash_str) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Parse and canonicalize the hash so downloads use the same storage key as
    // uploads, which store verified xorb identities in lowercase hex.
    let xorb_hash = match MerkleHash::from_hex(&hash_str) {
        Ok(h) => h,
        Err(e) => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("Invalid hash format: {}", e)
            }));
        }
    };
    let xorb_hash_hex = xorb_hash.to_hex();

    // Extract, verify, and authorize the caller in one step.
    if let Err(rej) = require_auth(&req, &auth, AuthNeed::Scope("read")) {
        return rej.respond();
    }

    // Prefer zero-copy local streaming. Remote backends stream into a guarded
    // temporary file so memory usage does not scale with xorb size.
    let xorb_key = format!("xorbs/{}", xorb_hash_hex);

    // Try streaming path (local storage: zero-copy file access)
    match storage.get_path(&xorb_key).await {
        Ok(Some(path)) => {
            let file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Xorb not found: {}", hash_str)
                    }));
                }
                Err(e) => {
                    error!(
                        "Failed to open xorb file for streaming {}: {}",
                        path.display(),
                        e
                    );
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": crate::api::INTERNAL_ERROR_MESSAGE
                    }));
                }
            };
            let metadata = match file.metadata().await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Xorb not found: {}", hash_str)
                    }));
                }
                Err(e) => {
                    error!("Failed to get xorb file metadata: {}", e);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": crate::api::INTERNAL_ERROR_MESSAGE
                    }));
                }
            };
            if !metadata.is_file() {
                error!("Xorb storage path is not a file: {}", path.display());
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": crate::api::INTERNAL_ERROR_MESSAGE
                }));
            }
            let file_size = metadata.len();

            use tokio_util::io::ReaderStream;
            let stream = ReaderStream::new(file);
            let body = actix_web::body::SizedStream::new(file_size, stream);

            info!("Streaming xorb {} ({} bytes)", hash_str, file_size);
            GLOBAL_METRICS.record_storage_operation();
            GLOBAL_METRICS.record_download_bytes(file_size);

            HttpResponse::Ok()
                .content_type("application/octet-stream")
                .body(body)
        }
        Ok(None) => {
            let temp_dir = config.storage.resolve_reconstruction_temp_dir();
            let (file_size, stream) = match crate::util::download_to_temp_stream(
                storage.get_ref().as_ref(),
                &xorb_key,
                &temp_dir,
                "xorb-download",
            )
            .await
            {
                Ok(download) => download,
                Err(StorageError::NotFound(_)) => {
                    return HttpResponse::NotFound().json(serde_json::json!({
                        "error": format!("Xorb not found: {}", hash_str)
                    }));
                }
                Err(e) => {
                    error!("Failed to stream remote xorb {}: {}", hash_str, e);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": crate::api::INTERNAL_ERROR_MESSAGE
                    }));
                }
            };

            let body = actix_web::body::SizedStream::new(file_size, stream);
            GLOBAL_METRICS.record_storage_operation();
            GLOBAL_METRICS.record_download_bytes(file_size);
            HttpResponse::Ok()
                .content_type("application/octet-stream")
                .body(body)
        }
        Err(StorageError::NotFound(_)) => HttpResponse::NotFound().json(serde_json::json!({
            "error": format!("Xorb not found: {}", hash_str)
        })),
        Err(e) => {
            error!("Failed to get path for xorb {}: {}", hash_str, e);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": crate::api::INTERNAL_ERROR_MESSAGE
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{AuthVerifier, KeyPair, XetClaims, sign_xet_token};
    use crate::config::AuthConfig;
    use crate::storage::local::LocalStorage;
    use crate::storage::{StorageError, StorageResult};
    use actix_web::{App, test, web};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;

    fn create_test_config() -> (KeyPair, AuthVerifier, ServerConfig) {
        let kp = KeyPair::generate();
        let public_key_pem = KeyPair::public_key_to_pem(&kp.verifying_key()).unwrap();

        // Use a temp file inside a tempdir to ensure cleanup
        let temp_dir = tempdir().unwrap();
        let temp_path = temp_dir.path().join(format!("pubkey-{}.pem", kp.kid()));
        std::fs::write(&temp_path, &public_key_pem).unwrap();

        // Keep temp_dir alive by leaking it (test scope is short)
        let temp_path_str = temp_path.to_str().unwrap().to_string();
        std::mem::forget(temp_dir); // Keep temp dir alive for test duration

        let auth_config = AuthConfig {
            public_key_path: temp_path_str,
            public_keys: Vec::new(),
            trusted_kids: vec![kp.kid()],
            private_key_path: None,
            signing_kid: None,
        };

        let auth_verifier = AuthVerifier::from_config(&auth_config).unwrap();

        let config = ServerConfig {
            auth: auth_config,
            ..Default::default()
        };
        (kp, auth_verifier, config)
    }

    fn create_test_token(kp: &KeyPair, scope: &str) -> String {
        let kid = kp.kid();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = XetClaims {
            sub: "test".to_string(),
            scope: scope.to_string(),
            repo_id: "test/repo".to_string(),
            repo_type: "model".to_string(),
            revision: "main".to_string(),
            exp: now + 3600,
            iat: now,
            kid,
            token_type: "user".to_string(),
            oid: None,
            operation: None,
        };
        sign_xet_token(&claims, kp).unwrap()
    }

    struct StreamingOnlyStorage {
        data: Bytes,
        download_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl StorageBackend for StreamingOnlyStorage {
        async fn put(&self, _key: &str, _data: Bytes) -> StorageResult<()> {
            Err(StorageError::Internal("unexpected put".to_string()))
        }

        async fn get(&self, _key: &str) -> StorageResult<Bytes> {
            Err(StorageError::Internal(
                "unbounded get must not be used for downloads".to_string(),
            ))
        }

        async fn exists(&self, _key: &str) -> StorageResult<bool> {
            Ok(true)
        }

        async fn get_size(&self, _key: &str) -> StorageResult<u64> {
            Ok(self.data.len() as u64)
        }

        async fn delete(&self, _key: &str) -> StorageResult<()> {
            Ok(())
        }

        async fn download_to_path(&self, _key: &str, dest: &Path) -> StorageResult<()> {
            self.download_called.store(true, Ordering::SeqCst);
            tokio::fs::write(dest, &self.data)
                .await
                .map_err(|error| StorageError::Internal(error.to_string()))
        }
    }

    #[actix_web::test]
    async fn test_upload_xorb_unauthorized() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

        let (_, auth, config) = create_test_config();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route("/v1/xorbs/{prefix}/{hash}", web::post().to(upload_xorb)),
        )
        .await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::post()
            .uri(&format!("/v1/xorbs/default/{}", hash))
            .set_payload(vec![0u8; 100])
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_upload_xorb_invalid_prefix() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());

        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read write");

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route("/v1/xorbs/{prefix}/{hash}", web::post().to(upload_xorb)),
        )
        .await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::post()
            .uri(&format!("/v1/xorbs/invalid/{}", hash))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(vec![0u8; 100])
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn remote_xorb_download_uses_bounded_streaming_path() {
        let (kp, auth, mut config) = create_test_config();
        let temp_dir = tempdir().unwrap();
        config.storage.reconstruction_temp_dir =
            Some(temp_dir.path().to_str().unwrap().to_string());
        let token = create_test_token(&kp, "read");
        let data = Bytes::from_static(b"remote xorb bytes");
        let download_called = Arc::new(AtomicBool::new(false));
        let storage: Box<dyn StorageBackend> = Box::new(StreamingOnlyStorage {
            data: data.clone(),
            download_called: download_called.clone(),
        });

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route(
                    "/v1/xorbs/{prefix}/{hash}/download",
                    web::get().to(download_xorb),
                ),
        )
        .await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/xorbs/default/{hash}/download"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), 200);
        assert_eq!(test::read_body(resp).await, data);
        assert!(download_called.load(Ordering::SeqCst));
    }

    #[actix_web::test]
    async fn missing_local_xorb_download_returns_not_found() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> =
            Box::new(LocalStorage::new(dir.path().to_str().unwrap()).unwrap());
        let (kp, auth, config) = create_test_config();
        let token = create_test_token(&kp, "read");

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(auth))
                .app_data(web::Data::new(config))
                .route(
                    "/v1/xorbs/{prefix}/{hash}/download",
                    web::get().to(download_xorb),
                ),
        )
        .await;

        let hash = "b".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/xorbs/default/{hash}/download"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), 404);
    }
}
