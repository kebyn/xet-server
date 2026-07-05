//! Configuration management for Xet Storage server

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub conversion: ConversionConfig,
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
    /// Rate limit for public endpoints in requests per minute per IP.
    /// Configure via `XET_RATE_LIMIT_RPM` environment variable.
    /// Default: 60 RPM.
    pub rate_limit_rpm: u32,
}

impl ServerSettings {
    /// Get the base URL for the server.
    /// Returns `public_base_url` if configured, otherwise constructs from host:port.
    /// Trailing slashes are stripped to prevent malformed URLs when callers append paths.
    ///
    /// URL validation happens at config load time (in `ServerConfig::from_env()`).
    /// This method performs no validation — it trusts that the URL was validated on load.
    /// This avoids redundant parsing on every call (e.g., in batch API URL construction).
    pub fn base_url(&self) -> String {
        self.public_base_url
            .clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port))
            .trim_end_matches('/')
            .to_string()
    }

    /// Validate the base URL configuration.
    /// Called once during config loading to fail fast on misconfiguration.
    ///
    /// # Panics
    /// Panics if `public_base_url` is set and either:
    /// - The URL is not syntactically valid (e.g. malformed scheme or path)
    /// - The URL is missing a host component (e.g. `"http://"`)
    pub fn validate_base_url(&self) {
        if let Some(ref url) = self.public_base_url {
            let url = url.trim_end_matches('/');
            match url::Url::parse(url) {
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
                            url,
                            parsed.scheme()
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
    /// For S3 or if local_path is unset, defaults to `/var/tmp/xet-uploads`.
    /// Configure via `XET_UPLOAD_TEMP_DIR` environment variable.
    pub upload_temp_dir: Option<String>,
    /// Directory for xorb reconstruction temp files.
    /// Used by the download reconstruction pipeline to store xorb chunks while
    /// reassembling them into the final LFS object. Defaults to OS temp dir + "xet-reconstruction".
    /// Configure via `XET_RECONSTRUCTION_TEMP_DIR` environment variable.
    pub reconstruction_temp_dir: Option<String>,
    /// I3: Enable integrity verification on LFS downloads.
    /// When enabled, the server streams the file through SHA-256 hasher before sending
    /// to verify the content matches the OID. This catches storage corruption (bit rot)
    /// but adds CPU overhead. Disable for maximum performance on trusted storage.
    pub verify_download_integrity: bool,
}

impl StorageConfig {
    /// Resolve the directory for streaming upload temp files.
    pub fn resolve_upload_temp_dir(&self) -> PathBuf {
        if let Some(dir) = &self.upload_temp_dir {
            PathBuf::from(dir)
        } else if let Some(local_path) = &self.local_path {
            PathBuf::from(local_path).join(".tmp")
        } else {
            // I1 fix: Use /var/tmp for S3 backend fallback.
            // /var/tmp is preferred over /tmp because:
            // 1. Not cleared on reboot (persists across restarts)
            // 2. Usually on a larger partition than /tmp
            // 3. Still has restricted permissions (1777)
            // Note: For local storage, prefer setting local_path so temp files
            // are on the same filesystem for atomic rename.
            PathBuf::from("/var/tmp/xet-uploads")
        }
    }

    /// Resolve the directory for xorb reconstruction temp files.
    /// Uses configured value, falling back to OS temp dir + "xet-reconstruction".
    pub fn resolve_reconstruction_temp_dir(&self) -> PathBuf {
        if let Some(dir) = &self.reconstruction_temp_dir {
            PathBuf::from(dir)
        } else {
            std::env::temp_dir().join("xet-reconstruction")
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
    /// I5 fix: Optional path to private key PEM for signing proxy tokens.
    /// When set, CAS batch API generates short-lived proxy tokens instead of
    /// passing through the user's long-lived token. This prevents long-lived
    /// token leakage in batch API responses.
    /// If not set, CAS logs a warning and batch API passes through user tokens
    /// (acceptable for development/testing, not recommended for production).
    pub private_key_path: Option<String>,
    /// Kid to use when signing proxy tokens. Must match public key.
    /// Defaults to first entry in trusted_kids.
    pub signing_kid: Option<String>,
}

/// Conversion pipeline configuration
///
/// Controls automatic conversion of raw blobs (uploaded via Git LFS / HF CLI)
/// to Xorb+Shard format for global chunk-level deduplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionConfig {
    /// Enable automatic conversion of raw blobs to xorb/shard format.
    pub enabled: bool,
    /// Compression scheme: "none", "lz4", "bg4lz4"
    pub compression_scheme: String,
    /// Delete raw blob after successful conversion (saves 2x storage).
    /// If false, raw blob is kept alongside xorb/shard copies.
    pub delete_raw_after_conversion: bool,
    /// Minimum file size (bytes) to trigger conversion.
    /// Files smaller than this stay as raw blobs permanently.
    pub min_conversion_size: u64,
    /// Maximum file size (bytes) eligible for conversion.
    /// Files larger than this stay as raw blobs permanently to bound conversion
    /// latency, temporary disk usage, and xorb/shard generation work.
    pub max_conversion_size: u64,
}

impl Default for ConversionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            compression_scheme: "lz4".to_string(),
            delete_raw_after_conversion: true,
            min_conversion_size: 65536, // 64KB — skip tiny files (1KB conversions waste CPU/IO for near-zero dedup value)
            max_conversion_size: 512 * 1024 * 1024, // 512MB — match Hub max_upload_size
        }
    }
}

impl ConversionConfig {
    /// Parse compression scheme string into enum
    pub fn scheme(&self) -> crate::format::compression::CompressionScheme {
        match self.compression_scheme.to_lowercase().as_str() {
            "none" => crate::format::compression::CompressionScheme::None,
            "lz4" => crate::format::compression::CompressionScheme::LZ4,
            "bg4lz4" => crate::format::compression::CompressionScheme::ByteGrouping4LZ4,
            _ => crate::format::compression::CompressionScheme::LZ4,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8081, // Changed from 8080 to avoid conflict with Hub API
                public_base_url: None,
                max_body_size_mb: 2048,
                rate_limit_rpm: 60,
            },
            storage: StorageConfig {
                backend: "local".to_string(),
                s3_bucket: None,
                s3_region: None,
                s3_endpoint: None,
                local_path: Some("./data".to_string()),
                upload_temp_dir: None,
                reconstruction_temp_dir: None,
                verify_download_integrity: false, // Disabled by default for performance
            },
            auth: AuthConfig {
                // M2 fix: Use /etc/xet instead of /tmp for better security
                // /tmp is world-writable and vulnerable to symlink attacks
                public_key_path: "/etc/xet/public-key.pem".to_string(), // Production default
                trusted_kids: vec!["hub-key-1".to_string()], // Changed from "test-kid" to match Hub default
                private_key_path: None, // I5 fix: Optional, set CAS_PRIVATE_KEY_PATH to enable proxy token generation
                signing_kid: None,
            },
            conversion: ConversionConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Validate configuration parameters.
    /// M1 fix: Returns Result instead of panicking for better error handling.
    /// I4 fix: Prevent zero values that would cause service unavailability.
    fn validate(&self) -> Result<(), String> {
        // I4 fix: Validate base URL once at config load time
        self.server.validate_base_url();

        if self.server.rate_limit_rpm == 0 {
            return Err(
                "XET_RATE_LIMIT_RPM must be > 0 (got 0). This would disable rate limiting."
                    .to_string(),
            );
        }
        if self.server.max_body_size_mb == 0 {
            return Err(
                "XET_MAX_BODY_SIZE_MB must be > 0 (got 0). This would prevent all uploads."
                    .to_string(),
            );
        }
        // M6 fix: Warn on invalid compression_scheme instead of silently falling back to LZ4.
        match self.conversion.compression_scheme.to_lowercase().as_str() {
            "none" | "lz4" | "bg4lz4" => {}
            invalid => {
                tracing::warn!(
                    "XET_CONVERSION_SCHEME '{}' is not a valid compression scheme. \
                     Falling back to LZ4. Valid values: none, lz4, bg4lz4",
                    invalid
                );
            }
        }
        // I13 fix: Validate min_conversion_size <= max_conversion_size
        if self.conversion.min_conversion_size > self.conversion.max_conversion_size {
            return Err(format!(
                "XET_MIN_CONVERSION_SIZE ({}) must be <= XET_MAX_CONVERSION_SIZE ({}). \
                 Current values would prevent all conversions from triggering.",
                self.conversion.min_conversion_size, self.conversion.max_conversion_size
            ));
        }
        Ok(())
    }

    /// Load configuration from environment variables with defaults
    pub fn from_env() -> Self {
        let host = std::env::var("XET_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = match std::env::var("XET_PORT") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!(
                    "XET_PORT '{}' is not a valid port number, using default 8081",
                    val
                );
                8081
            }),
            Err(_) => 8081, // Changed from 8080 to avoid conflict with Hub API
        };
        let public_base_url = std::env::var("XET_PUBLIC_BASE_URL").ok();
        let max_body_size_mb = match std::env::var("XET_MAX_BODY_SIZE_MB") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!(
                    "XET_MAX_BODY_SIZE_MB '{}' is not a valid number, using default 2048",
                    val
                );
                2048
            }),
            Err(_) => 2048,
        };
        let rate_limit_rpm = match std::env::var("XET_RATE_LIMIT_RPM") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!(
                    "XET_RATE_LIMIT_RPM '{}' is not a valid number, using default 60",
                    val
                );
                60
            }),
            Err(_) => 60,
        };

        let backend = std::env::var("XET_STORAGE_BACKEND").unwrap_or_else(|_| "local".to_string());
        let s3_bucket = std::env::var("XET_S3_BUCKET").ok();
        let s3_region = std::env::var("XET_S3_REGION").ok();
        let s3_endpoint = std::env::var("XET_S3_ENDPOINT").ok();
        let local_path = std::env::var("XET_LOCAL_PATH").ok();
        let upload_temp_dir = std::env::var("XET_UPLOAD_TEMP_DIR").ok();
        let reconstruction_temp_dir = std::env::var("XET_RECONSTRUCTION_TEMP_DIR").ok();
        let verify_download_integrity = std::env::var("XET_VERIFY_DOWNLOAD_INTEGRITY")
            .ok()
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);

        // CAS-specific auth configuration
        // M2 fix: Use /etc/xet instead of /tmp for better security
        let public_key_path = std::env::var("CAS_PUBLIC_KEY_PATH")
            .unwrap_or_else(|_| "/etc/xet/public-key.pem".to_string());
        let trusted_kids = std::env::var("CAS_TRUSTED_KIDS")
            .ok()
            .map(|s| s.split(',').map(|kid| kid.trim().to_string()).collect())
            .unwrap_or_else(|| {
                tracing::warn!("CAS_TRUSTED_KIDS not set, using default 'hub-key-1'. Ensure this matches Hub's HUB_KID configuration.");
                vec!["hub-key-1".to_string()]  // Changed from "test-kid" to match Hub default
            });
        // I5 fix: Optional private key for signing proxy tokens in batch API responses
        let private_key_path = std::env::var("CAS_PRIVATE_KEY_PATH").ok();
        let signing_kid = std::env::var("CAS_SIGNING_KID").ok();

        // Conversion pipeline configuration
        let conversion_enabled = std::env::var("XET_CONVERSION_ENABLED")
            .ok()
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);
        let conversion_scheme =
            std::env::var("XET_CONVERSION_SCHEME").unwrap_or_else(|_| "lz4".to_string());
        let delete_raw = std::env::var("XET_DELETE_RAW_AFTER_CONVERSION")
            .ok()
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);
        let min_conversion_size = match std::env::var("XET_MIN_CONVERSION_SIZE") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!(
                    "XET_MIN_CONVERSION_SIZE '{}' is not a valid number, using default 65536",
                    val
                );
                65536
            }),
            Err(_) => 65536,
        };
        let max_conversion_size = match std::env::var("XET_MAX_CONVERSION_SIZE") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!(
                    "XET_MAX_CONVERSION_SIZE '{}' is not a valid number, using default 512MB",
                    val
                );
                512 * 1024 * 1024
            }),
            Err(_) => 512 * 1024 * 1024, // 512MB — match Hub max_upload_size
        };

        let config = Self {
            server: ServerSettings {
                host,
                port,
                public_base_url,
                max_body_size_mb,
                rate_limit_rpm,
            },
            storage: StorageConfig {
                backend,
                s3_bucket,
                s3_region,
                s3_endpoint,
                local_path,
                upload_temp_dir,
                reconstruction_temp_dir,
                verify_download_integrity,
            },
            auth: AuthConfig {
                public_key_path,
                trusted_kids,
                private_key_path,
                signing_kid,
            },
            conversion: ConversionConfig {
                enabled: conversion_enabled,
                compression_scheme: conversion_scheme,
                delete_raw_after_conversion: delete_raw,
                min_conversion_size,
                max_conversion_size,
            },
        };
        // M1 fix: Handle validation errors with clear error messages
        if let Err(e) = config.validate() {
            panic!("Configuration validation failed: {}", e);
        }
        config
    }
}

/// Check public key file permissions and return a warning message if insecure.
///
/// Returns `Some(warning)` if the file is world-writable or group-writable,
/// which would allow other users to replace the public key and forge tokens.
/// Returns `None` if permissions are secure (owner-only read/write).
pub fn check_public_key_permissions(path: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return None, // File doesn't exist yet, skip check
    };
    let mode = metadata.permissions().mode();

    let world_writable = mode & 0o002 != 0;
    let group_writable = mode & 0o020 != 0;

    if world_writable || group_writable {
        let mut warnings = Vec::new();
        if world_writable {
            warnings.push("world-writable (any user can modify)");
        }
        if group_writable {
            warnings.push("group-writable (group members can modify)");
        }
        Some(format!(
            "SECURITY WARNING: Public key file '{}' is {}. \
            An attacker could replace this file to forge authentication tokens. \
            Use a secure path (e.g., /etc/xet/) with mode 0644 or 0600.",
            path,
            warnings.join(" and ")
        ))
    } else {
        None
    }
}
