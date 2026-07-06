use serde::{Deserialize, Serialize};
use std::env;
use std::str::FromStr;

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
    /// M5 fix: Cached base URL computed at config load time, avoiding repeated clone+trim.
    #[serde(skip)]
    cached_base_url: Option<String>,
}

impl ServerSettings {
    /// Get the base URL for the server.
    /// M5 fix: Returns cached value if available, otherwise computes it.
    pub fn base_url(&self) -> String {
        if let Some(ref cached) = self.cached_base_url {
            return cached.clone();
        }
        self.public_base_url
            .clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port))
            .trim_end_matches('/')
            .to_string()
    }

    /// Compute and cache the base URL. Called after config is fully loaded.
    pub fn cache_base_url(&mut self) {
        self.cached_base_url = Some(
            self.public_base_url
                .clone()
                .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port))
                .trim_end_matches('/')
                .to_string(),
        );
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            host: "0.0.0.0".to_string(),
            port: 8080,
            public_base_url: None,
            rate_limit_rpm: 120,
            cached_base_url: None,
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
            base_url: "http://localhost:8081".to_string(), // Changed from 3000 to match CAS default port
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
            inline_threshold_bytes: 1024 * 1024,               // 1MB
            upload_temp_dir: "./data/hub-uploads".to_string(), // I1 fix: Use app-specific dir instead of /tmp
            max_upload_size: 512 * 1024 * 1024,                // 512MB
        }
    }
}

/// Main configuration for the Hub API
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HubConfig {
    pub server: ServerSettings,
    pub auth: AuthSettings,
    pub metadata: MetadataSettings,
    pub cas: CasSettings,
    pub storage: StorageSettings,
}

impl HubConfig {
    fn parse_env<T>(key: &str, default: T) -> Result<T, String>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        match env::var(key) {
            Ok(value) => value
                .parse()
                .map_err(|e| format!("{key} '{value}' is not a valid value: {e}")),
            Err(_) => Ok(default),
        }
    }

    fn parse_optional_env<T>(key: &str) -> Result<Option<T>, String>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        match env::var(key) {
            Ok(value) => value
                .parse()
                .map(Some)
                .map_err(|e| format!("{key} '{value}' is not a valid value: {e}")),
            Err(_) => Ok(None),
        }
    }

    /// Validate configuration parameters.
    /// Validate configuration parameters.
    /// M1 fix: Returns Result instead of panicking for better error handling.
    /// I4 fix: Prevent zero values that would cause service unavailability.
    fn validate(&self) -> Result<(), String> {
        if self.server.rate_limit_rpm == 0 {
            return Err(
                "HUB_RATE_LIMIT_RPM must be > 0 (got 0). This would disable rate limiting."
                    .to_string(),
            );
        }
        if self.metadata.db_pool_size == 0 {
            return Err(
                "HUB_DB_POOL_SIZE must be > 0 (got 0). This would prevent all database operations."
                    .to_string(),
            );
        }
        if self.auth.token_ttl_seconds == 0 {
            return Err(
                "HUB_TOKEN_TTL_SECONDS must be > 0 (got 0). Tokens would expire immediately."
                    .to_string(),
            );
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
            return Err(
                "HUB_MAX_UPLOAD_SIZE must be > 0 (got 0). This would prevent all uploads."
                    .to_string(),
            );
        }
        if self.cas.health_check_timeout_seconds == 0 {
            return Err("HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS must be > 0 (got 0). Health check would never complete.".to_string());
        }
        Ok(())
    }

    fn validate_url_with_host(name: &str, url: &str) -> Result<(), String> {
        let parsed = url::Url::parse(url)
            .map_err(|e| format!("{} '{}' is not a valid URL: {}", name, url, e))?;
        if parsed.host().is_none() {
            return Err(format!("{} '{}' is missing a valid host", name, url));
        }
        Ok(())
    }

    fn validate_url(name: &str, url: &str) -> Result<(), String> {
        url::Url::parse(url)
            .map(|_| ())
            .map_err(|e| format!("{} '{}' is not a valid URL: {}", name, url, e))
    }

    /// Load configuration from environment variables.
    pub fn try_from_env() -> Result<Self, String> {
        let config = HubConfig {
            server: ServerSettings {
                host: env::var("HUB_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
                port: Self::parse_env("HUB_PORT", 8080)?,
                public_base_url: env::var("HUB_PUBLIC_BASE_URL").ok(),
                rate_limit_rpm: Self::parse_env("HUB_RATE_LIMIT_RPM", 120)?,
                cached_base_url: None,
            },
            auth: AuthSettings {
                private_key_path: env::var("HUB_PRIVATE_KEY_PATH")
                    .unwrap_or_else(|_| "private_key.pem".to_string()),
                kid: env::var("HUB_KID").unwrap_or_else(|_| "hub-key-1".to_string()),
                token_ttl_seconds: Self::parse_env("HUB_TOKEN_TTL_SECONDS", 3600)?,
                proxy_token_ttl_seconds: Self::parse_env("HUB_PROXY_TOKEN_TTL_SECONDS", 300)?,
                internal_token_ttl_seconds: Self::parse_env(
                    "HUB_INTERNAL_TOKEN_TTL_SECONDS",
                    86400,
                )?,
            },
            metadata: MetadataSettings {
                sqlite_path: env::var("HUB_SQLITE_PATH").unwrap_or_else(|_| "hub.db".to_string()),
                db_pool_size: Self::parse_env("HUB_DB_POOL_SIZE", 5)?,
            },
            cas: CasSettings {
                base_url: {
                    let url = env::var("CAS_BASE_URL")
                        .unwrap_or_else(|_| "http://localhost:8081".to_string());
                    Self::validate_url("CAS_BASE_URL", &url)?;
                    url
                },
                internal_timeout_seconds: Self::parse_env("HUB_CAS_TIMEOUT_SECS", 30)?,
                max_download_size: Self::parse_env("HUB_MAX_DOWNLOAD_SIZE", 512 * 1024 * 1024)?,
                health_check_timeout_seconds: Self::parse_env(
                    "HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS",
                    10,
                )?,
            },
            storage: StorageSettings {
                inline_threshold_bytes: Self::parse_env("HUB_INLINE_THRESHOLD", 1024 * 1024)?,
                upload_temp_dir: env::var("HUB_UPLOAD_TEMP_DIR")
                    .unwrap_or_else(|_| "./data/hub-uploads".to_string()), // I1 fix: Use app-specific dir instead of /tmp
                max_upload_size: Self::parse_env("HUB_MAX_UPLOAD_SIZE", 512 * 1024 * 1024)?,
            },
        };

        if let Some(ref url) = config.server.public_base_url {
            Self::validate_url_with_host("HUB_PUBLIC_BASE_URL", url)?;
        }
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from environment variables.
    ///
    /// Prefer [`HubConfig::try_from_env`] in production entrypoints so startup
    /// errors are returned instead of panicking.
    pub fn from_env() -> Self {
        Self::try_from_env().unwrap_or_else(|e| panic!("Configuration validation failed: {}", e))
    }

    /// M3: Load configuration from a TOML file
    pub fn from_file(path: &str) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file {}: {}", path, e))?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse config file {}: {}", path, e))?;

        if let Some(ref url) = config.server.public_base_url {
            Self::validate_url_with_host("public_base_url", url)?;
        }

        Ok(config)
    }

    /// M3: Load configuration from file (if path provided) with environment variable overrides.
    /// Priority: environment variables > file > defaults.
    pub fn try_from_file_or_env() -> Result<Self, String> {
        // Start with file-based config if HUB_CONFIG_FILE is set
        let mut config = match env::var("HUB_CONFIG_FILE") {
            Ok(path) => match Self::from_file(&path) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return Err(format!(
                        "HUB_CONFIG_FILE '{}' is set but config could not be loaded: {}",
                        path, e
                    ));
                }
            },
            Err(_) => Self::try_from_env()?,
        };

        // Override with environment variables (env takes precedence)
        if let Ok(host) = env::var("HUB_HOST") {
            config.server.host = host;
        }
        if let Some(port) = Self::parse_optional_env("HUB_PORT")? {
            config.server.port = port;
        }
        if let Ok(url) = env::var("HUB_PUBLIC_BASE_URL") {
            Self::validate_url_with_host("HUB_PUBLIC_BASE_URL", &url)?;
            config.server.public_base_url = Some(url);
        }
        if let Some(rpm) = Self::parse_optional_env("HUB_RATE_LIMIT_RPM")? {
            config.server.rate_limit_rpm = rpm;
        }
        if let Ok(path) = env::var("HUB_PRIVATE_KEY_PATH") {
            config.auth.private_key_path = path;
        }
        if let Ok(kid) = env::var("HUB_KID") {
            config.auth.kid = kid;
        }
        if let Some(ttl) = Self::parse_optional_env("HUB_TOKEN_TTL_SECONDS")? {
            config.auth.token_ttl_seconds = ttl;
        }
        if let Some(ttl) = Self::parse_optional_env("HUB_PROXY_TOKEN_TTL_SECONDS")? {
            config.auth.proxy_token_ttl_seconds = ttl;
        }
        if let Some(ttl) = Self::parse_optional_env("HUB_INTERNAL_TOKEN_TTL_SECONDS")? {
            config.auth.internal_token_ttl_seconds = ttl;
        }
        if let Ok(path) = env::var("HUB_SQLITE_PATH") {
            config.metadata.sqlite_path = path;
        }
        if let Some(size) = Self::parse_optional_env("HUB_DB_POOL_SIZE")? {
            config.metadata.db_pool_size = size;
        }
        if let Ok(url) = env::var("CAS_BASE_URL") {
            Self::validate_url("CAS_BASE_URL", &url)?;
            config.cas.base_url = url;
        }
        if let Some(timeout) = Self::parse_optional_env("HUB_CAS_TIMEOUT_SECS")? {
            config.cas.internal_timeout_seconds = timeout;
        }
        if let Some(size) = Self::parse_optional_env("HUB_MAX_DOWNLOAD_SIZE")? {
            config.cas.max_download_size = size;
        }
        if let Some(timeout) = Self::parse_optional_env("HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS")? {
            config.cas.health_check_timeout_seconds = timeout;
        }
        if let Some(threshold) = Self::parse_optional_env("HUB_INLINE_THRESHOLD")? {
            config.storage.inline_threshold_bytes = threshold;
        }
        if let Ok(dir) = env::var("HUB_UPLOAD_TEMP_DIR") {
            config.storage.upload_temp_dir = dir;
        }
        if let Some(size) = Self::parse_optional_env("HUB_MAX_UPLOAD_SIZE")? {
            config.storage.max_upload_size = size;
        }

        config.validate()?;
        // M5 fix: Cache computed base URL to avoid repeated allocation
        config.server.cache_base_url();
        Ok(config)
    }

    /// M3: Load configuration from file (if path provided) with environment variable overrides.
    /// Priority: environment variables > file > defaults.
    ///
    /// Prefer [`HubConfig::try_from_file_or_env`] in production entrypoints so startup
    /// errors are returned instead of panicking.
    pub fn from_file_or_env() -> Self {
        Self::try_from_file_or_env()
            .unwrap_or_else(|e| panic!("Configuration validation failed: {}", e))
    }
}
