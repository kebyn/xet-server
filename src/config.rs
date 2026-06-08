//! Configuration management for Xet Storage server

use serde::{Deserialize, Serialize};

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
}

/// HTTP server settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
}

/// Storage backend configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: String, // "s3" or "local"
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_endpoint: Option<String>,
    pub local_path: Option<String>,
}

/// Authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub jwt_secret: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8080,
            },
            storage: StorageConfig {
                backend: "local".to_string(),
                s3_bucket: None,
                s3_region: None,
                s3_endpoint: None,
                local_path: Some("./data".to_string()),
            },
            auth: AuthConfig {
                jwt_secret: "dev-secret-change-in-production".to_string(),
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

        let backend = std::env::var("XET_STORAGE_BACKEND").unwrap_or_else(|_| "local".to_string());
        let s3_bucket = std::env::var("XET_S3_BUCKET").ok();
        let s3_region = std::env::var("XET_S3_REGION").ok();
        let s3_endpoint = std::env::var("XET_S3_ENDPOINT").ok();
        let local_path = std::env::var("XET_LOCAL_PATH").ok();

        let jwt_secret = std::env::var("XET_JWT_SECRET")
            .unwrap_or_else(|_| "dev-secret".to_string());

        Self {
            server: ServerSettings { host, port },
            storage: StorageConfig {
                backend,
                s3_bucket,
                s3_region,
                s3_endpoint,
                local_path,
            },
            auth: AuthConfig { jwt_secret },
        }
    }
}
