use crate::config::CasSettings;
use crate::error::HubError;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

const BLOB_SIZE_HEADER: &str = "X-Blob-Size";
const MAX_CAS_CONTROL_RESPONSE_SIZE: u64 = 8 * 1024 * 1024;
const MAX_CAS_ERROR_RESPONSE_SIZE: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
enum ResponseBodyError {
    #[error("upstream response size {actual} bytes exceeds limit of {max} bytes")]
    TooLarge { actual: u64, max: u64 },
    #[error("failed to read upstream response body: {0}")]
    Read(#[from] reqwest::Error),
}

/// Classify a send-time transport error: timeouts surface as a distinct
/// variant so callers can answer 504 instead of a flat 502.
/// Classify a response-body read failure: a mid-body timeout surfaces as
/// `HubError::CasTimeout` so callers answer 504 instead of a flat 502.
/// Size-limit violations (TooLarge) are permanent and stay CasError.
fn cas_read_error(context: &str, error: ResponseBodyError) -> HubError {
    match error {
        ResponseBodyError::Read(source) if source.is_timeout() => HubError::CasTimeout(source),
        error => HubError::CasError(format!("{}: {}", context, error)),
    }
}

/// Classify an upload send failure: timeouts carry status 504 so the API
/// mappings can answer Gateway Timeout; other transport failures stay 502.
fn cas_upload_send_error(e: reqwest::Error) -> CasUploadError {
    CasUploadError {
        status: if e.is_timeout() { 504 } else { 502 },
        message: format!("CAS request failed: {}", e),
    }
}

fn cas_send_error(e: reqwest::Error) -> HubError {
    if e.is_timeout() {
        HubError::CasTimeout(e)
    } else {
        HubError::CasError(format!("CAS request failed: {}", e))
    }
}

async fn read_response_body_limited(
    response: reqwest::Response,
    max_size: u64,
) -> Result<Bytes, ResponseBodyError> {
    if let Some(declared_size) = response.content_length()
        && declared_size > max_size
    {
        return Err(ResponseBodyError::TooLarge {
            actual: declared_size,
            max: max_size,
        });
    }

    let mut stream = response.bytes_stream();
    let mut body = BytesMut::new();
    let mut received = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let chunk_size = u64::try_from(chunk.len()).map_err(|_| ResponseBodyError::TooLarge {
            actual: u64::MAX,
            max: max_size,
        })?;
        received = received
            .checked_add(chunk_size)
            .ok_or(ResponseBodyError::TooLarge {
                actual: u64::MAX,
                max: max_size,
            })?;
        if received > max_size {
            return Err(ResponseBodyError::TooLarge {
                actual: received,
                max: max_size,
            });
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body.freeze())
}

fn parse_blob_size(headers: &reqwest::header::HeaderMap) -> Result<u64, HubError> {
    let value = headers.get(BLOB_SIZE_HEADER).ok_or_else(|| {
        HubError::CasError(format!(
            "CAS HEAD response omitted required {} header",
            BLOB_SIZE_HEADER
        ))
    })?;
    let value = value.to_str().map_err(|_| {
        HubError::CasError(format!(
            "CAS HEAD response contained a non-text {} header",
            BLOB_SIZE_HEADER
        ))
    })?;

    value.parse::<u64>().map_err(|_| {
        HubError::CasError(format!(
            "CAS HEAD response contained an invalid {} header",
            BLOB_SIZE_HEADER
        ))
    })
}

/// Error returned by CAS upload operations, preserving HTTP status codes
/// for proper error propagation to clients.
#[derive(Debug)]
pub struct CasUploadError {
    pub status: u16,
    pub message: String,
}

/// Blob state from CAS
#[derive(Debug, Deserialize)]
pub struct BlobState {
    pub state: String,
    pub xet_file_id: Option<String>,
    pub size: u64,
    pub sha256: String,
}

/// Trait defining the CAS client interface.
/// Allows mocking for unit tests without requiring a real CAS server.
#[async_trait::async_trait]
pub trait CasClientTrait: Send + Sync {
    /// HEAD a blob to check existence and state
    async fn head_blob(&self, oid: &str, internal_token: &str) -> Result<BlobState, HubError>;

    /// Proxy LFS upload to CAS
    async fn proxy_lfs_upload(
        &self,
        oid: &str,
        data: bytes::Bytes,
        token: &str,
    ) -> Result<(), CasUploadError>;
}

/// CAS HTTP client for communicating with the content addressable storage.
///
/// Uses reqwest with connection pooling for efficient HTTP communication.
/// The client is `Send + Sync + Clone` and can be safely shared across tasks.
pub struct CasClient {
    base_url: String,
    max_download_size: u64,
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl CasClientTrait for CasClient {
    /// HEAD a blob to check existence and state
    async fn head_blob(&self, oid: &str, internal_token: &str) -> Result<BlobState, HubError> {
        let url = format!("{}/internal/blob/{}", self.base_url, oid);
        let resp = self
            .client
            .head(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await
            .map_err(cas_send_error)?;

        let status = resp.status().as_u16();
        match status {
            200 => {
                let size = parse_blob_size(resp.headers())?;
                let state = resp
                    .headers()
                    .get("X-Storage-State")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("raw_only")
                    .to_string();
                let file_id = resp
                    .headers()
                    .get("X-File-Id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                Ok(BlobState {
                    state,
                    xet_file_id: file_id,
                    size,
                    sha256: oid.to_string(),
                })
            }
            404 => Err(HubError::NotFound(format!("Blob not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Upload a blob to CAS via LFS endpoint (buffered version)
    async fn proxy_lfs_upload(
        &self,
        oid: &str,
        data: bytes::Bytes,
        token: &str,
    ) -> Result<(), CasUploadError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await
            .map_err(cas_upload_send_error)?;

        let status = resp.status().as_u16();
        if resp.status().is_success() {
            Ok(())
        } else {
            let body = read_response_body_limited(resp, MAX_CAS_ERROR_RESPONSE_SIZE)
                .await
                .map_err(|e| CasUploadError {
                    status,
                    message: format!("CAS error response rejected: {}", e),
                })?;
            Err(CasUploadError {
                status,
                message: String::from_utf8_lossy(&body).into_owned(),
            })
        }
    }
}

impl CasClient {
    /// Create a new CAS client from settings
    pub fn new(settings: &CasSettings) -> Result<Self, HubError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.internal_timeout_seconds))
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(|e| HubError::Internal(format!("Failed to build CAS HTTP client: {}", e)))?;

        Ok(Self {
            base_url: settings.base_url.trim_end_matches('/').to_string(),
            max_download_size: settings.max_download_size,
            client,
        })
    }

    /// Get full blob state via internal API
    pub async fn get_state(
        &self,
        oid: &str,
        internal_token: &str,
    ) -> Result<Option<BlobState>, HubError> {
        let url = format!("{}/internal/state/{}", self.base_url, oid);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await
            .map_err(cas_send_error)?;

        let status = resp.status().as_u16();
        match status {
            200 => {
                let body = read_response_body_limited(resp, MAX_CAS_CONTROL_RESPONSE_SIZE)
                    .await
                    .map_err(|e| cas_read_error("CAS state response rejected", e))?;
                let state: BlobState = serde_json::from_slice(&body).map_err(|e| {
                    HubError::CasError(format!("Invalid CAS state response: {}", e))
                })?;
                Ok(Some(state))
            }
            404 => Ok(None),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Proxy a Git LFS batch request to CAS
    pub async fn proxy_batch(
        &self,
        body: &serde_json::Value,
        token: &str,
    ) -> Result<serde_json::Value, HubError> {
        let url = format!("{}/objects/batch", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/vnd.git-lfs+json")
            .json(body)
            .send()
            .await
            .map_err(cas_send_error)?;

        let status = resp.status().as_u16();
        let response_body = read_response_body_limited(resp, MAX_CAS_CONTROL_RESPONSE_SIZE)
            .await
            .map_err(|e| cas_read_error("CAS batch response rejected", e))?;
        let response_body: serde_json::Value = serde_json::from_slice(&response_body)
            .map_err(|e| HubError::CasError(format!("Invalid CAS batch response: {}", e)))?;

        if status >= 400 {
            return Err(HubError::CasError(format!(
                "CAS batch error: {}",
                response_body
            )));
        }

        Ok(response_body)
    }

    /// Upload a blob to CAS from a file path (streaming version)
    ///
    /// Uses reqwest's streaming body support to send file contents without
    /// buffering the entire file in memory. Memory usage is O(chunk_size).
    pub async fn proxy_lfs_upload_from_path(
        &self,
        oid: &str,
        file_path: &std::path::Path,
        file_size: u64,
        token: &str,
    ) -> Result<(), CasUploadError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);

        let file = tokio::fs::File::open(file_path)
            .await
            .map_err(|e| CasUploadError {
                status: 500,
                message: format!("Failed to open temp file: {}", e),
            })?;

        let stream = tokio_util::io::ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);

        let resp = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", file_size)
            .body(body)
            .send()
            .await
            .map_err(cas_upload_send_error)?;

        let status = resp.status().as_u16();
        if resp.status().is_success() {
            Ok(())
        } else {
            let body = read_response_body_limited(resp, MAX_CAS_ERROR_RESPONSE_SIZE)
                .await
                .map_err(|e| CasUploadError {
                    status,
                    message: format!("CAS error response rejected: {}", e),
                })?;
            Err(CasUploadError {
                status,
                message: String::from_utf8_lossy(&body).into_owned(),
            })
        }
    }

    /// Download and verify a blob from CAS via the LFS endpoint.
    ///
    /// This buffered path is only for small inline resolve responses. The
    /// snapshot size is used as the read limit, then both the exact size and
    /// SHA-256 OID are checked before any bytes cross the Hub trust boundary.
    pub async fn proxy_lfs_download(
        &self,
        oid: &str,
        expected_size: u64,
        token: &str,
    ) -> Result<bytes::Bytes, HubError> {
        if expected_size > self.max_download_size {
            return Err(HubError::CasError(format!(
                "Expected CAS object size {} exceeds configured download limit {}",
                expected_size, self.max_download_size
            )));
        }

        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(cas_send_error)?;

        match resp.status().as_u16() {
            200 => {
                let body = read_response_body_limited(resp, expected_size)
                    .await
                    .map_err(|e| cas_read_error("CAS download rejected", e))?;

                let actual_size = u64::try_from(body.len()).map_err(|_| {
                    HubError::CasError("CAS download size cannot be represented as u64".to_string())
                })?;
                if actual_size != expected_size {
                    return Err(HubError::CasError(format!(
                        "CAS download size mismatch for {}: expected {}, received {}",
                        oid, expected_size, actual_size
                    )));
                }

                let actual_oid = hex::encode(Sha256::digest(&body));
                if !actual_oid.eq_ignore_ascii_case(oid) {
                    return Err(HubError::CasError(format!(
                        "CAS download hash mismatch for {}: received {}",
                        oid, actual_oid
                    )));
                }

                Ok(body)
            }
            404 => Err(HubError::NotFound(format!("Object not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Download a blob from CAS via LFS endpoint (streaming version)
    /// Returns a streaming response to avoid loading entire file into memory.
    /// Memory usage is O(chunk_size) regardless of file size.
    pub async fn proxy_lfs_download_streaming(
        &self,
        oid: &str,
        token: &str,
    ) -> Result<(u64, reqwest::Response), HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(cas_send_error)?;

        match resp.status().as_u16() {
            200 => {
                // Get content length if available
                let content_length = resp.content_length().unwrap_or(0);

                // Defense-in-depth: validate Content-Length against max size
                // CAS is a trusted internal service, but this protects against CAS bugs
                // that could cause unbounded streaming
                if content_length > self.max_download_size {
                    return Err(HubError::CasError(format!(
                        "Content-Length too large: {} bytes (max: {} bytes)",
                        content_length, self.max_download_size
                    )));
                }

                Ok((content_length, resp))
            }
            404 => Err(HubError::NotFound(format!("Object not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Check CAS health endpoint
    pub async fn health_check(&self) -> Result<bool, HubError> {
        let url = format!("{}/health", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(_) => Ok(false),
            Err(e) => Err(HubError::CasError(format!("Health check failed: {}", e))),
        }
    }

    /// Check CAS readiness endpoint.
    pub async fn readiness_check(&self) -> Result<bool, HubError> {
        let url = format!("{}/ready", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(_) => Ok(false),
            Err(e) => Err(HubError::CasError(format!("Readiness check failed: {}", e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CasSettings;
    use actix_web::{App, HttpResponse, HttpServer, web};
    use futures_util::StreamExt;
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::net::TcpListener;

    #[test]
    fn test_client_creation() {
        let settings = CasSettings {
            base_url: "http://localhost:3000".to_string(),
            internal_timeout_seconds: 30,
            max_download_size: 512 * 1024 * 1024,
            health_check_timeout_seconds: 10,
        };
        let client = CasClient::new(&settings).expect("CAS client should be created");
        assert_eq!(client.base_url, "http://localhost:3000");
    }

    #[test]
    fn test_client_trims_base_url_slash() {
        let settings = CasSettings {
            base_url: "http://localhost:3000/".to_string(),
            internal_timeout_seconds: 30,
            max_download_size: 512 * 1024 * 1024,
            health_check_timeout_seconds: 10,
        };
        let client = CasClient::new(&settings).expect("CAS client should be created");
        assert_eq!(client.base_url, "http://localhost:3000");
    }

    #[test]
    fn blob_size_header_is_required_and_must_be_a_u64() {
        let headers = HeaderMap::new();
        let missing = parse_blob_size(&headers).expect_err("missing size header must fail");
        assert!(missing.to_string().contains("omitted required X-Blob-Size"));

        let mut headers = HeaderMap::new();
        headers.insert(BLOB_SIZE_HEADER, HeaderValue::from_static("not-a-size"));
        let invalid = parse_blob_size(&headers).expect_err("invalid size header must fail");
        assert!(invalid.to_string().contains("invalid X-Blob-Size"));

        headers.insert(
            BLOB_SIZE_HEADER,
            HeaderValue::from_static("18446744073709551615"),
        );
        assert_eq!(parse_blob_size(&headers).unwrap(), u64::MAX);
    }

    async fn endless_oversized_body() -> HttpResponse {
        let first = futures_util::stream::once(async {
            Ok::<_, actix_web::Error>(web::Bytes::from(vec![b'x'; 2048]))
        });
        let never_finishes =
            futures_util::stream::pending::<Result<web::Bytes, actix_web::Error>>();
        HttpResponse::Ok().streaming(first.chain(never_finishes))
    }

    #[actix_web::test]
    async fn buffered_download_rejects_runtime_limit_without_waiting_for_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = HttpServer::new(|| {
            App::new().route("/lfs/objects/{oid}", web::get().to(endless_oversized_body))
        })
        .listen(listener)
        .unwrap()
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        let client = CasClient::new(&CasSettings {
            base_url: format!("http://{address}"),
            internal_timeout_seconds: 30,
            max_download_size: 1024,
            health_check_timeout_seconds: 10,
        })
        .unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.proxy_lfs_download(&"a".repeat(64), 1024, "token"),
        )
        .await;
        handle.stop(false).await;

        let error = result
            .expect("size enforcement must not wait for upstream EOF")
            .expect_err("body above the runtime limit must be rejected");
        assert!(error.to_string().contains("exceeds limit"));
    }

    #[actix_web::test]
    async fn streaming_download_timeout_is_cas_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let server = HttpServer::new(|| {
            App::new().route(
                "/lfs/objects/{oid}",
                web::get().to(|| async {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    HttpResponse::Ok().body("late")
                }),
            )
        })
        .listen(listener)
        .unwrap()
        .run();
        tokio::spawn(server);

        let client = CasClient::new(&CasSettings {
            base_url: format!("http://{address}"),
            internal_timeout_seconds: 1,
            max_download_size: 1024,
            health_check_timeout_seconds: 5,
        })
        .unwrap();

        let error = client
            .proxy_lfs_download_streaming(&"a".repeat(64), "token")
            .await
            .expect_err("a 2s response against a 1s timeout must fail");
        assert!(
            matches!(error, HubError::CasTimeout(_)),
            "expected a timeout variant, got: {error}"
        );
        assert!(error.to_string().contains("timed out"));
    }
    #[actix_web::test]
    async fn buffered_download_body_read_timeout_is_cas_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let server = HttpServer::new(|| {
            App::new().route(
                "/lfs/objects/{oid}",
                web::get().to(|| async {
                    // Headers (plus one body chunk) arrive immediately; the
                    // body then stalls forever, so the client-level timeout
                    // fires mid-read rather than at send time.
                    let first = futures_util::stream::once(async {
                        Ok::<_, actix_web::Error>(web::Bytes::from(vec![b'x'; 8]))
                    });
                    let never_finishes =
                        futures_util::stream::pending::<Result<web::Bytes, actix_web::Error>>();
                    HttpResponse::Ok()
                        .insert_header(("Content-Length", "64"))
                        .streaming(first.chain(never_finishes))
                }),
            )
        })
        .listen(listener)
        .unwrap()
        .run();
        tokio::spawn(server);

        let client = CasClient::new(&CasSettings {
            base_url: format!("http://{address}"),
            internal_timeout_seconds: 1,
            max_download_size: 1024,
            health_check_timeout_seconds: 5,
        })
        .unwrap();

        let error = client
            .proxy_lfs_download(&"a".repeat(64), 64, "token")
            .await
            .expect_err("a stalled body against a 1s timeout must fail");
        assert!(
            matches!(error, HubError::CasTimeout(_)),
            "expected a body-read timeout, got: {error}"
        );
        assert!(error.to_string().contains("timed out"));
    }
}
