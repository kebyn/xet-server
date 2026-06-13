use std::env;
use serde::{Deserialize, Serialize};

/// Server configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_base_url: Option<String>,
    /// Rate limit for public endpoints in requests per minute per IP.
    /// Configure via `HUB_RATE_LIMIT_RPM` environment variable.
    /// Default: 120 RPM.
    pub rate_limit_rpm: u32,
}

impl ServerSettings {
    /// Get the base URL for the server.
    /// Returns `public_base_url` if configured, otherwise constructs from host:port.
    /// Trailing slashes are stripped to prevent malformed URLs when callers append paths.
    ///
    /// # Panics
    /// Panics if `public_base_url` is set but not a valid URL.
    pub fn base_url(&self) -> String {
        let url = self.public_base_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port));
        let url = url.trim_end_matches('/').to_string();

        // I1: Validate URL format using proper URL parsing if explicitly configured
        // I2: Panic on invalid URL to fail fast at startup rather than at first request
        if self.public_base_url.is_some() {
            match url::Url::parse(&url) {
                Ok(parsed) => {
                    if parsed.host().is_none() {
                        panic!(
                            "public_base_url '{}' is missing a valid host. \
                            This will cause client connection failures.",
                            url
                        );
                    }
                    if parsed.scheme() != "http" && parsed.scheme() != "https" {
                        tracing::warn!(
                            "public_base_url '{}' uses non-HTTP scheme '{}'. \
                            This may cause issues with client URLs.",
                            url, parsed.scheme()
                        );
                    }
                }
                Err(e) => {
                    panic!(
                        "public_base_url '{}' is not a valid URL: {}. \
                        This will cause client connection failures.",
                        url, e
                    );
                }
            }
        }

        url
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            host: "0.0.0.0".to_string(),
            port: 8080,
            public_base_url: None,
            rate_limit_rpm: 120,
        }
    }
}

/// Authentication settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSettings {
    pub private_key_path: String,
    pub kid: String,
    pub token_ttl_seconds: u64,
    /// TTL for short-lived proxy tokens (LFS operations).
    /// Configure via `HUB_PROXY_TOKEN_TTL_SECONDS` environment variable.
    /// Default: 300 (5 minutes).
    pub proxy_token_ttl_seconds: u64,
}

impl Default for AuthSettings {
    fn default() -> Self {
        AuthSettings {
            private_key_path: "private_key.pem".to_string(),
            kid: "hub-key-1".to_string(),
            token_ttl_seconds: 3600,
            proxy_token_ttl_seconds: 300,
        }
    }
}

/// Metadata store settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataSettings {
    pub sqlite_path: String,
    /// SQLite connection pool size.
    /// Configure via `HUB_DB_POOL_SIZE` env var. Default: 5.
    pub db_pool_size: u32,
}

impl Default for MetadataSettings {
    fn default() -> Self {
        MetadataSettings {
            sqlite_path: "hub.db".to_string(),
            db_pool_size: 5,
        }
    }
}

/// CAS (Content Addressable Storage) settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasSettings {
    pub base_url: String,
    pub internal_timeout_seconds: u64,
    /// Maximum download size in bytes for CAS responses.
    /// Should match or exceed HUB_MAX_UPLOAD_SIZE.
    /// Configure via `HUB_MAX_DOWNLOAD_SIZE` env var. Default: 512MB.
    pub max_download_size: u64,
}

impl Default for CasSettings {
    fn default() -> Self {
        CasSettings {
            base_url: "http://localhost:8081".to_string(),  // Changed from 3000 to match CAS default port
            internal_timeout_seconds: 30,
            max_download_size: 512 * 1024 * 1024,
        }
    }
}

/// Storage settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    pub inline_threshold_bytes: u64,
    /// Directory for temporary files during streaming uploads
    pub upload_temp_dir: String,
    /// M2: Maximum upload size in bytes. Defaults to 512MB.
    /// Configure via HUB_MAX_UPLOAD_SIZE environment variable.
    pub max_upload_size: u64,
}

impl Default for StorageSettings {
    fn default() -> Self {
        StorageSettings {
            inline_threshold_bytes: 1024 * 1024, // 1MB
            upload_temp_dir: "/tmp/hub-uploads".to_string(),
            max_upload_size: 512 * 1024 * 1024, // 512MB
        }
    }
}

/// Main configuration for the Hub API
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct HubConfig {
    pub server: ServerSettings,
    pub auth: AuthSettings,
    pub metadata: MetadataSettings,
    pub cas: CasSettings,
    pub storage: StorageSettings,
}


impl HubConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        HubConfig {
            server: ServerSettings {
                host: env::var("HUB_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
                port: env::var("HUB_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(8080),
                public_base_url: env::var("HUB_PUBLIC_BASE_URL").ok(),
                rate_limit_rpm: env::var("HUB_RATE_LIMIT_RPM")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(120),
            },
            auth: AuthSettings {
                private_key_path: env::var("HUB_PRIVATE_KEY_PATH")
                    .unwrap_or_else(|_| "private_key.pem".to_string()),
                kid: env::var("HUB_KID")
                    .unwrap_or_else(|_| "hub-key-1".to_string()),
                token_ttl_seconds: env::var("HUB_TOKEN_TTL_SECONDS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(3600),
                proxy_token_ttl_seconds: env::var("HUB_PROXY_TOKEN_TTL_SECONDS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(300),
            },
            metadata: MetadataSettings {
                sqlite_path: env::var("HUB_SQLITE_PATH")
                    .unwrap_or_else(|_| "hub.db".to_string()),
                db_pool_size: env::var("HUB_DB_POOL_SIZE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(5),
            },
            cas: CasSettings {
                base_url: env::var("CAS_BASE_URL")
                    .unwrap_or_else(|_| "http://localhost:8081".to_string()),  // Changed from 3000 to match CAS default port
                internal_timeout_seconds: env::var("HUB_CAS_TIMEOUT_SECS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(30),
                max_download_size: env::var("HUB_MAX_DOWNLOAD_SIZE")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(512 * 1024 * 1024),
            },
            storage: StorageSettings {
                inline_threshold_bytes: env::var("HUB_INLINE_THRESHOLD")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(1024 * 1024),
                upload_temp_dir: env::var("HUB_UPLOAD_TEMP_DIR")
                    .unwrap_or_else(|_| "/tmp/hub-uploads".to_string()),
                max_upload_size: env::var("HUB_MAX_UPLOAD_SIZE")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(512 * 1024 * 1024),
            },
        }
    }

    /// M3: Load configuration from a TOML file
    pub fn from_file(path: &str) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file {}: {}", path, e))?;
        toml::from_str(&content)
            .map_err(|e| format!("Failed to parse config file {}: {}", path, e))
    }

    /// M3: Load configuration from file (if path provided) with environment variable overrides.
    /// Priority: environment variables > file > defaults
    pub fn from_file_or_env() -> Self {
        // Start with file-based config if HUB_CONFIG_FILE is set
        let mut config = env::var("HUB_CONFIG_FILE")
            .ok()
            .and_then(|path| Self::from_file(&path).ok())
            .unwrap_or_else(Self::from_env);

        // Override with environment variables (env takes precedence)
        if let Some(host) = env::var("HUB_HOST").ok() {
            config.server.host = host;
        }
        if let Some(port) = env::var("HUB_PORT").ok().and_then(|p| p.parse().ok()) {
            config.server.port = port;
        }
        if let Some(url) = env::var("HUB_PUBLIC_BASE_URL").ok() {
            // M-3: Validate URL format when set via environment variable
            // I2: Panic on invalid URL to fail fast at startup
            // M1 FIX: Parse URL only once using match instead of parsing twice
            let parsed = match url::Url::parse(&url) {
                Ok(p) => p,
                Err(e) => {
                    panic!(
                        "HUB_PUBLIC_BASE_URL '{}' is not a valid URL: {}",
                        url, e
                    );
                }
            };
            // Validate host is present
            if parsed.host().is_none() {
                panic!(
                    "HUB_PUBLIC_BASE_URL '{}' is missing a valid host",
                    url
                );
            }
            config.server.public_base_url = Some(url);
        }
        if let Some(rpm) = env::var("HUB_RATE_LIMIT_RPM").ok().and_then(|v| v.parse().ok()) {
            config.server.rate_limit_rpm = rpm;
        }
        if let Some(path) = env::var("HUB_PRIVATE_KEY_PATH").ok() {
            config.auth.private_key_path = path;
        }
        if let Some(kid) = env::var("HUB_KID").ok() {
            config.auth.kid = kid;
        }
        if let Some(ttl) = env::var("HUB_TOKEN_TTL_SECONDS").ok().and_then(|t| t.parse().ok()) {
            config.auth.token_ttl_seconds = ttl;
        }
        if let Some(ttl) = env::var("HUB_PROXY_TOKEN_TTL_SECONDS").ok().and_then(|t| t.parse().ok()) {
            config.auth.proxy_token_ttl_seconds = ttl;
        }
        if let Some(path) = env::var("HUB_SQLITE_PATH").ok() {
            config.metadata.sqlite_path = path;
        }
        if let Some(size) = env::var("HUB_DB_POOL_SIZE").ok().and_then(|v| v.parse().ok()) {
            config.metadata.db_pool_size = size;
        }
        if let Some(url) = env::var("CAS_BASE_URL").ok() {
            config.cas.base_url = url;
        }
        if let Some(timeout) = env::var("HUB_CAS_TIMEOUT_SECS").ok().and_then(|t| t.parse().ok()) {
            config.cas.internal_timeout_seconds = timeout;
        }
        if let Some(size) = env::var("HUB_MAX_DOWNLOAD_SIZE").ok().and_then(|t| t.parse().ok()) {
            config.cas.max_download_size = size;
        }
        if let Some(threshold) = env::var("HUB_INLINE_THRESHOLD").ok().and_then(|t| t.parse().ok()) {
            config.storage.inline_threshold_bytes = threshold;
        }
        if let Some(dir) = env::var("HUB_UPLOAD_TEMP_DIR").ok() {
            config.storage.upload_temp_dir = dir;
        }
        if let Some(size) = env::var("HUB_MAX_UPLOAD_SIZE").ok().and_then(|t| t.parse().ok()) {
            config.storage.max_upload_size = size;
        }

        config
    }
}
