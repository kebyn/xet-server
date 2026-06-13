use serde::Deserialize;
use std::time::Duration;
use crate::config::CasSettings;
use crate::error::HubError;

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

/// CAS HTTP client for communicating with the content addressable storage.
///
/// Uses reqwest with connection pooling for efficient HTTP communication.
/// The client is `Send + Sync + Clone` and can be safely shared across tasks.
pub struct CasClient {
    base_url: String,
    client: reqwest::Client,
}

impl CasClient {
    /// Create a new CAS client from settings
    pub fn new(settings: &CasSettings) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.internal_timeout_seconds))
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .expect("Failed to build reqwest client");

        Self {
            base_url: settings.base_url.trim_end_matches('/').to_string(),
            client,
        }
    }

    /// HEAD a blob to check existence and state
    pub async fn head_blob(&self, oid: &str, internal_token: &str) -> Result<BlobState, HubError> {
        let url = format!("{}/internal/blob/{}", self.base_url, oid);
        let resp = self.client
            .head(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await
            .map_err(|e| HubError::CasError(format!("CAS request failed: {}", e)))?;

        let status = resp.status().as_u16();
        match status {
            200 => {
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
                    size: 0,
                    sha256: oid.to_string(),
                })
            }
            404 => Err(HubError::NotFound(format!("Blob not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Get full blob state via internal API
    pub async fn get_state(&self, oid: &str, internal_token: &str) -> Result<Option<BlobState>, HubError> {
        let url = format!("{}/internal/state/{}", self.base_url, oid);
        let resp = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await
            .map_err(|e| HubError::CasError(format!("CAS request failed: {}", e)))?;

        let status = resp.status().as_u16();
        match status {
            200 => {
                let state: BlobState = resp
                    .json()
                    .await
                    .map_err(|e| HubError::CasError(e.to_string()))?;
                Ok(Some(state))
            }
            404 => Ok(None),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Proxy a Git LFS batch request to CAS
    pub async fn proxy_batch(&self, body: &serde_json::Value, token: &str) -> Result<serde_json::Value, HubError> {
        let url = format!("{}/objects/batch", self.base_url);
        let resp = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/vnd.git-lfs+json")
            .json(body)
            .send()
            .await
            .map_err(|e| HubError::CasError(format!("CAS request failed: {}", e)))?;

        let status = resp.status().as_u16();
        let resp_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| HubError::CasError(e.to_string()))?;

        if status >= 400 {
            return Err(HubError::CasError(format!("CAS batch error: {}", resp_body)));
        }

        Ok(resp_body)
    }

    /// Upload a blob to CAS via LFS endpoint (buffered version)
    pub async fn proxy_lfs_upload(&self, oid: &str, data: bytes::Bytes, token: &str) -> Result<(), CasUploadError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self.client
            .put(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await
            .map_err(|e| CasUploadError { status: 502, message: format!("CAS request failed: {}", e) })?;

        let status = resp.status().as_u16();
        if resp.status().is_success() {
            Ok(())
        } else {
            let body = resp.text().await.map_err(|e| CasUploadError { status, message: format!("Failed to read CAS response: {}", e) })?;
            Err(CasUploadError { status, message: body })
        }
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

        let file = tokio::fs::File::open(file_path).await
            .map_err(|e| CasUploadError { status: 500, message: format!("Failed to open temp file: {}", e) })?;

        let stream = tokio_util::io::ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);

        let resp = self.client
            .put(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", file_size)
            .body(body)
            .send()
            .await
            .map_err(|e| CasUploadError { status: 502, message: format!("CAS request failed: {}", e) })?;

        let status = resp.status().as_u16();
        if resp.status().is_success() {
            Ok(())
        } else {
            let body = resp.text().await.map_err(|e| CasUploadError { status, message: format!("Failed to read CAS response: {}", e) })?;
            Err(CasUploadError { status, message: body })
        }
    }

    /// Download a blob from CAS via LFS endpoint (buffered version)
    /// Loads entire file into memory. Use proxy_lfs_download_streaming for large files.
    pub async fn proxy_lfs_download(&self, oid: &str, token: &str) -> Result<bytes::Bytes, HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(|e| HubError::CasError(format!("CAS request failed: {}", e)))?;

        match resp.status().as_u16() {
            200 => {
                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| HubError::CasError(e.to_string()))?;

                // Check size limit (512MB)
                const MAX_DOWNLOAD_SIZE: u64 = 512 * 1024 * 1024;
                if body.len() as u64 > MAX_DOWNLOAD_SIZE {
                    return Err(HubError::CasError(format!("Download too large: {} bytes", body.len())));
                }

                Ok(body)
            }
            404 => Err(HubError::NotFound(format!("Object not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Download a blob from CAS via LFS endpoint (streaming version)
    /// I6: Returns a streaming response to avoid loading entire file into memory.
    /// Memory usage is O(chunk_size) regardless of file size.
    pub async fn proxy_lfs_download_streaming(
        &self,
        oid: &str,
        token: &str,
    ) -> Result<(u64, reqwest::Response), HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .map_err(|e| HubError::CasError(format!("CAS request failed: {}", e)))?;

        match resp.status().as_u16() {
            200 => {
                // Get content length if available
                let content_length = resp.content_length().unwrap_or(0);
                Ok((content_length, resp))
            }
            404 => Err(HubError::NotFound(format!("Object not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CasSettings;

    #[test]
    fn test_client_creation() {
        let settings = CasSettings {
            base_url: "http://localhost:3000".to_string(),
            internal_timeout_seconds: 30,
        };
        let client = CasClient::new(&settings);
        assert_eq!(client.base_url, "http://localhost:3000");
    }

    #[test]
    fn test_client_trims_base_url_slash() {
        let settings = CasSettings {
            base_url: "http://localhost:3000/".to_string(),
            internal_timeout_seconds: 30,
        };
        let client = CasClient::new(&settings);
        assert_eq!(client.base_url, "http://localhost:3000");
    }
}
