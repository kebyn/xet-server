use serde::Deserialize;
use std::sync::Once;
use crate::config::CasSettings;
use crate::error::HubError;

static RUSTLS_INIT: Once = Once::new();

/// Blob state from CAS
#[derive(Debug, Deserialize)]
pub struct BlobState {
    pub state: String,
    pub xet_file_id: Option<String>,
    pub size: u64,
    pub sha256: String,
}

/// CAS HTTP client for communicating with the content addressable storage.
/// Uses awc (actix-web client) to avoid runtime conflicts with actix-web.
/// Note: awc::Client uses Rc internally and is not Send+Sync, so we create
/// a new client per request. For high-throughput scenarios, consider switching
/// to reqwest or using a connection pool with a Send-safe client.
pub struct CasClient {
    base_url: String,
    timeout: std::time::Duration,
}

impl CasClient {
    /// Create a new CAS client from settings
    pub fn new(settings: &CasSettings) -> Self {
        Self {
            base_url: settings.base_url.trim_end_matches('/').to_string(),
            timeout: std::time::Duration::from_secs(settings.internal_timeout_seconds),
        }
    }

    /// Create an awc client for a single request.
    fn client(&self) -> awc::Client {
        // Ensure rustls crypto provider is installed (needed for HTTPS connections)
        RUSTLS_INIT.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        awc::Client::builder()
            .timeout(self.timeout)
            .finish()
    }

    /// HEAD a blob to check existence and state
    pub async fn head_blob(&self, oid: &str, internal_token: &str) -> Result<BlobState, HubError> {
        let url = format!("{}/internal/blob/{}", self.base_url, oid);
        let resp = self.client()
            .head(&url)
            .insert_header(("Authorization", format!("Bearer {}", internal_token)))
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
        let mut resp = self.client()
            .get(&url)
            .insert_header(("Authorization", format!("Bearer {}", internal_token)))
            .send()
            .await
            .map_err(|e| HubError::CasError(format!("CAS request failed: {}", e)))?;

        let status = resp.status().as_u16();
        match status {
            200 => {
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| HubError::CasError(e.to_string()))?;
                let state: BlobState = serde_json::from_value(body)
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
        let mut resp = self.client()
            .post(&url)
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/vnd.git-lfs+json"))
            .send_json(&body)
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
    pub async fn proxy_lfs_upload(&self, oid: &str, data: bytes::Bytes, token: &str) -> Result<(), (u16, String)> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let mut resp = self.client()
            .put(&url)
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Content-Type", "application/octet-stream"))
            .send_body(data)
            .await
            .map_err(|e| (502u16, format!("CAS request failed: {}", e)))?;

        let status = resp.status().as_u16();
        if resp.status().is_success() {
            Ok(())
        } else {
            let body = resp.body().await.map_err(|e| (status, format!("Failed to read CAS response: {}", e)))?;
            let error_msg = String::from_utf8_lossy(&body).to_string();
            Err((status, error_msg))
        }
    }

    /// Download a blob from CAS via LFS endpoint
    pub async fn proxy_lfs_download(&self, oid: &str, token: &str) -> Result<bytes::Bytes, HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let mut resp = self.client()
            .get(&url)
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .send()
            .await
            .map_err(|e| HubError::CasError(format!("CAS request failed: {}", e)))?;

        match resp.status().as_u16() {
            200 => {
                let body = resp
                    .body()
                    .limit(512 * 1024 * 1024) // 512MB
                    .await
                    .map_err(|e| HubError::CasError(e.to_string()))?;
                Ok(body)
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
