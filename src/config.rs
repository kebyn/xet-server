//! Configuration management for Xet Storage server

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub state: StateConfig,
}

/// HTTP server settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    /// Public-facing base URL for generating client-facing links (e.g. batch API action URLs).
    /// Required when the server is behind a reverse proxy, load balancer, or NAT.
    /// If unset, falls back to `http://{host}:{port}` which only works for direct access.
    pub public_base_url: Option<String>,
    /// Maximum HTTP request body size in megabytes.
    /// The entire body is buffered into RAM by actix-web's PayloadConfig, so this
    /// directly bounds per-request memory usage. Defaults to 2048 MB (2 GB).
    /// Increase for larger model file uploads; decrease to tighten memory safety.
    /// Configure via `XET_MAX_BODY_SIZE_MB` environment variable.
    pub max_body_size_mb: u64,
}

impl ServerSettings {
    /// Get the base URL for the server.
    /// Returns `public_base_url` if configured, otherwise constructs from host:port.
    /// Trailing slashes are stripped to prevent malformed URLs when callers append paths.
    pub fn base_url(&self) -> String {
        let url = self.public_base_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port));
        url.trim_end_matches('/').to_string()
    }

    /// Get the maximum request body size in bytes.
    pub fn max_body_size_bytes(&self) -> usize {
        // Saturate to usize::MAX on overflow (unlikely for realistic MB values)
        self.max_body_size_mb
            .saturating_mul(1024 * 1024)
            .try_into()
            .unwrap_or(usize::MAX)
    }
}

/// Storage backend configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: String, // "s3" or "local"
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_endpoint: Option<String>,
    pub local_path: Option<String>,
    /// Directory for streaming upload temp files.
    /// For local storage, defaults to `{local_path}/.tmp` (same filesystem -> atomic rename).
    /// For S3 or if unset, defaults to `/tmp/xet-uploads`.
    /// Configure via `XET_UPLOAD_TEMP_DIR` environment variable.
    pub upload_temp_dir: Option<String>,
}

impl StorageConfig {
    /// Resolve the directory for streaming upload temp files.
    pub fn resolve_upload_temp_dir(&self) -> PathBuf {
        if let Some(dir) = &self.upload_temp_dir {
            PathBuf::from(dir)
        } else if let Some(local_path) = &self.local_path {
            PathBuf::from(local_path).join(".tmp")
        } else {
            PathBuf::from("/tmp/xet-uploads")
        }
    }
}

/// Authentication configuration (Ed25519-based)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Path to the public key PEM file for token verification
    pub public_key_path: String,
    /// List of trusted key IDs (kid values) that are accepted
    pub trusted_kids: Vec<String>,
}

/// State management configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    /// Path to the SQLite database for state tracking
    pub sqlite_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8080,
                public_base_url: None,
                max_body_size_mb: 2048,
            },
            storage: StorageConfig {
                backend: "local".to_string(),
                s3_bucket: None,
                s3_region: None,
                s3_endpoint: None,
                local_path: Some("./data".to_string()),
                upload_temp_dir: None,
            },
            auth: AuthConfig {
                public_key_path: "/tmp/xet-test-public-key.pem".to_string(),
                trusted_kids: vec!["test-kid".to_string()],
            },
            state: StateConfig {
                sqlite_path: "/tmp/xet-state.db".to_string(),
            },
        }
    }
}

impl ServerConfig {
    /// Load configuration from environment variables with defaults
    pub fn from_env() -> Self {
        let host = std::env::var("XET_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = std::env::var("XET_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8080);
        let public_base_url = std::env::var("XET_PUBLIC_BASE_URL").ok();
        let max_body_size_mb = std::env::var("XET_MAX_BODY_SIZE_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2048);

        let backend = std::env::var("XET_STORAGE_BACKEND").unwrap_or_else(|_| "local".to_string());
        let s3_bucket = std::env::var("XET_S3_BUCKET").ok();
        let s3_region = std::env::var("XET_S3_REGION").ok();
        let s3_endpoint = std::env::var("XET_S3_ENDPOINT").ok();
        let local_path = std::env::var("XET_LOCAL_PATH").ok();
        let upload_temp_dir = std::env::var("XET_UPLOAD_TEMP_DIR").ok();

        // CAS-specific auth configuration
        let public_key_path = std::env::var("CAS_PUBLIC_KEY_PATH")
            .unwrap_or_else(|_| "/tmp/xet-public-key.pem".to_string());
        let trusted_kids = std::env::var("CAS_TRUSTED_KIDS")
            .ok()
            .map(|s| s.split(',').map(|kid| kid.trim().to_string()).collect())
            .unwrap_or_else(|| vec!["test-kid".to_string()]);

        // State database configuration
        let sqlite_path = std::env::var("CAS_STATE_DB_PATH")
            .unwrap_or_else(|_| "/tmp/xet-state.db".to_string());

        Self {
            server: ServerSettings { host, port, public_base_url, max_body_size_mb },
            storage: StorageConfig {
                backend,
                s3_bucket,
                s3_region,
                s3_endpoint,
                local_path,
                upload_temp_dir,
            },
            auth: AuthConfig {
                public_key_path,
                trusted_kids,
            },
            state: StateConfig {
                sqlite_path,
            },
        }
    }
}
