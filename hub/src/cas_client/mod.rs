use reqwest::Client;
use serde::Deserialize;
use crate::config::CasSettings;
use crate::error::HubError;

/// Blob state from CAS
#[derive(Debug, Deserialize)]
pub struct BlobState {
    pub state: String,
    pub xet_file_id: Option<String>,
    pub size: u64,
    pub sha256: String,
}

/// CAS HTTP client for communicating with the content addressable storage
pub struct CasClient {
    http: Client,
    base_url: String,
}

impl CasClient {
    /// Create a new CAS client from settings
    pub fn new(settings: &CasSettings) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(settings.internal_timeout_seconds))
            .build()
            .expect("Failed to build HTTP client");
        Self {
            http,
            base_url: settings.base_url.trim_end_matches('/').to_string(),
        }
    }

    /// HEAD a blob to check existence and state
    pub async fn head_blob(&self, oid: &str, internal_token: &str) -> Result<BlobState, HubError> {
        let url = format!("{}/internal/blob/{}", self.base_url, oid);
        let resp = self.http
            .head(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await?;

        match resp.status().as_u16() {
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
        let resp = self.http
            .get(&url)
            .header("Authorization", format!("Bearer {}", internal_token))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(Some(
                resp.json()
                    .await
                    .map_err(|e| HubError::CasError(e.to_string()))?
            )),
            404 => Ok(None),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }

    /// Proxy a Git LFS batch request to CAS
    pub async fn proxy_batch(&self, body: &serde_json::Value, token: &str) -> Result<serde_json::Value, HubError> {
        let url = format!("{}/objects/batch", self.base_url);
        let resp = self.http
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(body)
            .send()
            .await?;

        let status = resp.status().as_u16();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| HubError::CasError(e.to_string()))?;

        if status >= 400 {
            return Err(HubError::CasError(format!("CAS batch error: {}", body)));
        }

        Ok(body)
    }

    /// Upload a blob to CAS via LFS endpoint
    pub async fn proxy_lfs_upload(&self, oid: &str, data: bytes::Bytes, token: &str) -> Result<(), HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self.http
            .put(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(HubError::CasError(format!("CAS upload failed: {}", resp.status())))
        }
    }

    /// Download a blob from CAS via LFS endpoint
    pub async fn proxy_lfs_download(&self, oid: &str, token: &str) -> Result<bytes::Bytes, HubError> {
        let url = format!("{}/lfs/objects/{}", self.base_url, oid);
        let resp = self.http
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp
                .bytes()
                .await
                .map_err(|e| HubError::CasError(e.to_string()))?),
            404 => Err(HubError::NotFound(format!("Object not found: {}", oid))),
            code => Err(HubError::CasError(format!("CAS returned {}", code))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CasSettings;

    fn create_test_client() -> CasClient {
        CasClient::new(&CasSettings::default())
    }

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