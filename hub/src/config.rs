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
    /// URL validation happens at config load time (see `from_file_or_env()`).
    /// This method performs no validation — it trusts that the URL was validated on load.
    pub fn base_url(&self) -> String {
        self.public_base_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port))
            .trim_end_matches('/')
            .to_string()
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
    /// C1 fix: TTL for internal tokens (Hub-to-CAS communication, used by GC).
    /// Configure via `HUB_INTERNAL_TOKEN_TTL_SECONDS` environment variable.
    /// Default: 86400 (24 hours). Previous hardcoded value was 60 seconds,
    /// which caused GC to fail because GC runs hourly and tokens expired before next run.
    pub internal_token_ttl_seconds: u64,
}

impl Default for AuthSettings {
    fn default() -> Self {
        AuthSettings {
            private_key_path: "private_key.pem".to_string(),
            kid: "hub-key-1".to_string(),
            token_ttl_seconds: 3600,
            proxy_token_ttl_seconds: 300,
            internal_token_ttl_seconds: 86400, // C1 fix: 24 hours (was hardcoded 60s)
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
    /// Startup health check timeout in seconds.
    /// Hub spawns an async task to verify CAS connectivity at startup.
    /// If CAS doesn't respond within this window, an error is logged.
    /// Configure via `HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS` env var.
    /// Default: 10.
    pub health_check_timeout_seconds: u64,
}

impl Default for CasSettings {
    fn default() -> Self {
        CasSettings {
            base_url: "http://localhost:8081".to_string(),  // Changed from 3000 to match CAS default port
            internal_timeout_seconds: 30,
            max_download_size: 512 * 1024 * 1024,
            health_check_timeout_seconds: 10,
        }
    }
}

/// Storage settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    pub inline_threshold_bytes: u64,
    /// Directory for temporary files during streaming uploads.
    /// I1 fix: Use application-specific directory instead of /tmp for security.
    /// /tmp is world-writable and vulnerable to symlink attacks.
    /// Configure via HUB_UPLOAD_TEMP_DIR environment variable.
    pub upload_temp_dir: String,
    /// M2: Maximum upload size in bytes. Defaults to 512MB.
    /// Configure via HUB_MAX_UPLOAD_SIZE environment variable.
    pub max_upload_size: u64,
}

impl Default for StorageSettings {
    fn default() -> Self {
        StorageSettings {
            inline_threshold_bytes: 1024 * 1024, // 1MB
            upload_temp_dir: "./data/hub-uploads".to_string(),  // I1 fix: Use app-specific dir instead of /tmp
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
    /// Validate configuration parameters.
    /// Validate configuration parameters.
    /// M1 fix: Returns Result instead of panicking for better error handling.
    /// I4 fix: Prevent zero values that would cause service unavailability.
    fn validate(&self) -> Result<(), String> {
        if self.server.rate_limit_rpm == 0 {
            return Err("HUB_RATE_LIMIT_RPM must be > 0 (got 0). This would disable rate limiting.".to_string());
        }
        if self.metadata.db_pool_size == 0 {
            return Err("HUB_DB_POOL_SIZE must be > 0 (got 0). This would prevent all database operations.".to_string());
        }
        if self.auth.token_ttl_seconds == 0 {
            return Err("HUB_TOKEN_TTL_SECONDS must be > 0 (got 0). Tokens would expire immediately.".to_string());
        }
        if self.auth.proxy_token_ttl_seconds == 0 {
            return Err("HUB_PROXY_TOKEN_TTL_SECONDS must be > 0 (got 0). Proxy tokens would expire immediately.".to_string());
        }
        // C1 fix: Validate internal token TTL (must be long enough for GC interval)
        if self.auth.internal_token_ttl_seconds == 0 {
            return Err("HUB_INTERNAL_TOKEN_TTL_SECONDS must be > 0 (got 0). Internal tokens would expire immediately.".to_string());
        }
        if self.auth.internal_token_ttl_seconds < 3600 {
            tracing::warn!(
                "HUB_INTERNAL_TOKEN_TTL_SECONDS is {} (less than 1 hour). \
                GC runs hourly by default. Consider increasing to at least 86400 (24 hours).",
                self.auth.internal_token_ttl_seconds
            );
        }
        if self.storage.max_upload_size == 0 {
            return Err("HUB_MAX_UPLOAD_SIZE must be > 0 (got 0). This would prevent all uploads.".to_string());
        }
        if self.cas.health_check_timeout_seconds == 0 {
            return Err("HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS must be > 0 (got 0). Health check would never complete.".to_string());
        }
        Ok(())
    }

    /// Load configuration from environment variables.
    /// Note: Does NOT call validate() - caller (from_file_or_env) is responsible for validation.
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
                internal_token_ttl_seconds: env::var("HUB_INTERNAL_TOKEN_TTL_SECONDS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(86400),
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
                health_check_timeout_seconds: env::var("HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(10),
            },
            storage: StorageSettings {
                inline_threshold_bytes: env::var("HUB_INLINE_THRESHOLD")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(1024 * 1024),
                upload_temp_dir: env::var("HUB_UPLOAD_TEMP_DIR")
                    .unwrap_or_else(|_| "./data/hub-uploads".to_string()),  // I1 fix: Use app-specific dir instead of /tmp
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
        let config: Self = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse config file {}: {}", path, e))?;

        // M1 fix: Validate URL at load time (same validation as from_file_or_env)
        if let Some(ref url) = config.server.public_base_url {
            let parsed = url::Url::parse(url)
                .map_err(|e| format!("public_base_url '{}' is not a valid URL: {}", url, e))?;
            if parsed.host().is_none() {
                return Err(format!("public_base_url '{}' is missing a valid host", url));
            }
        }

        Ok(config)
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
        if let Ok(host) = env::var("HUB_HOST") {
            config.server.host = host;
        }
        if let Some(port) = env::var("HUB_PORT").ok().and_then(|p| p.parse().ok()) {
            config.server.port = port;
        }
        if let Ok(url) = env::var("HUB_PUBLIC_BASE_URL") {
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
        if let Ok(path) = env::var("HUB_PRIVATE_KEY_PATH") {
            config.auth.private_key_path = path;
        }
        if let Ok(kid) = env::var("HUB_KID") {
            config.auth.kid = kid;
        }
        if let Some(ttl) = env::var("HUB_TOKEN_TTL_SECONDS").ok().and_then(|t| t.parse().ok()) {
            config.auth.token_ttl_seconds = ttl;
        }
        if let Some(ttl) = env::var("HUB_PROXY_TOKEN_TTL_SECONDS").ok().and_then(|t| t.parse().ok()) {
            config.auth.proxy_token_ttl_seconds = ttl;
        }
        if let Some(ttl) = env::var("HUB_INTERNAL_TOKEN_TTL_SECONDS").ok().and_then(|t| t.parse().ok()) {
            config.auth.internal_token_ttl_seconds = ttl;
        }
        if let Ok(path) = env::var("HUB_SQLITE_PATH") {
            config.metadata.sqlite_path = path;
        }
        if let Some(size) = env::var("HUB_DB_POOL_SIZE").ok().and_then(|v| v.parse().ok()) {
            config.metadata.db_pool_size = size;
        }
        if let Ok(url) = env::var("CAS_BASE_URL") {
            config.cas.base_url = url;
        }
        if let Some(timeout) = env::var("HUB_CAS_TIMEOUT_SECS").ok().and_then(|t| t.parse().ok()) {
            config.cas.internal_timeout_seconds = timeout;
        }
        if let Some(size) = env::var("HUB_MAX_DOWNLOAD_SIZE").ok().and_then(|t| t.parse().ok()) {
            config.cas.max_download_size = size;
        }
        if let Some(timeout) = env::var("HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS").ok().and_then(|t| t.parse().ok()) {
            config.cas.health_check_timeout_seconds = timeout;
        }
        if let Some(threshold) = env::var("HUB_INLINE_THRESHOLD").ok().and_then(|t| t.parse().ok()) {
            config.storage.inline_threshold_bytes = threshold;
        }
        if let Ok(dir) = env::var("HUB_UPLOAD_TEMP_DIR") {
            config.storage.upload_temp_dir = dir;
        }
        if let Some(size) = env::var("HUB_MAX_UPLOAD_SIZE").ok().and_then(|t| t.parse().ok()) {
            config.storage.max_upload_size = size;
        }

        // M1 fix: Handle validation errors with clear error messages
        if let Err(e) = config.validate() {
            panic!("Configuration validation failed: {}", e);
        }
        config
    }
}
