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
    pub gc: GcConfig,
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
    /// # Panics
    /// Panics if `public_base_url` is set but not a valid URL.
    pub fn base_url(&self) -> String {
        let url = self.public_base_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port));
        let url = url.trim_end_matches('/').to_string();

        // M1: Validate URL format using proper URL parsing if explicitly configured
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
    /// Files larger than this stay as raw blobs permanently to prevent OOM
    /// (conversion loads the entire blob into memory for CDC chunking).
    pub max_conversion_size: u64,
}

impl Default for ConversionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            compression_scheme: "lz4".to_string(),
            delete_raw_after_conversion: true,
            min_conversion_size: 1024,           // 1KB — skip tiny files
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

/// Garbage collection configuration for cleaning up orphaned blobs
///
/// GC runs as a background task that periodically scans storage for blobs
/// that are no longer referenced by any file_tree entry in the Hub metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcConfig {
    /// Enable background GC task
    pub enabled: bool,
    /// GC run interval in seconds
    pub interval_seconds: u64,
    /// Grace period in seconds for newly uploaded blobs.
    /// Blobs younger than this are never deleted, preventing race conditions
    /// where a blob is uploaded but the commit hasn't been written to file_tree yet.
    pub grace_period_seconds: u64,
    /// Dry-run mode: report stats but don't actually delete
    pub dry_run: bool,
    /// Hub API base URL (for querying referenced hashes)
    pub hub_base_url: String,
    /// Internal token for authenticating with Hub's /internal/* endpoints
    pub hub_internal_token: String,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            enabled: false,             // Disabled by default, must opt-in
            interval_seconds: 3600,     // Every hour
            grace_period_seconds: 600,  // 10 minutes grace period
            dry_run: true,              // Dry-run by default for safety
            hub_base_url: "http://localhost:8080".to_string(),
            hub_internal_token: String::new(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSettings {
                host: "127.0.0.1".to_string(),
                port: 8081,  // Changed from 8080 to avoid conflict with Hub API
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
                verify_download_integrity: false, // Disabled by default for performance
            },
            auth: AuthConfig {
                public_key_path: "/tmp/xet-public-key.pem".to_string(),  // Unified with from_env() default
                trusted_kids: vec!["hub-key-1".to_string()],  // Changed from "test-kid" to match Hub default
            },
            conversion: ConversionConfig::default(),
            gc: GcConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Load configuration from environment variables with defaults
    pub fn from_env() -> Self {
        let host = std::env::var("XET_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = match std::env::var("XET_PORT") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("XET_PORT '{}' is not a valid port number, using default 8081", val);
                8081
            }),
            Err(_) => 8081,  // Changed from 8080 to avoid conflict with Hub API
        };
        let public_base_url = std::env::var("XET_PUBLIC_BASE_URL").ok();
        let max_body_size_mb = match std::env::var("XET_MAX_BODY_SIZE_MB") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("XET_MAX_BODY_SIZE_MB '{}' is not a valid number, using default 2048", val);
                2048
            }),
            Err(_) => 2048,
        };
        let rate_limit_rpm = match std::env::var("XET_RATE_LIMIT_RPM") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("XET_RATE_LIMIT_RPM '{}' is not a valid number, using default 60", val);
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
        let verify_download_integrity = std::env::var("XET_VERIFY_DOWNLOAD_INTEGRITY")
            .ok()
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);

        // CAS-specific auth configuration
        let public_key_path = std::env::var("CAS_PUBLIC_KEY_PATH")
            .unwrap_or_else(|_| "/tmp/xet-public-key.pem".to_string());
        let trusted_kids = std::env::var("CAS_TRUSTED_KIDS")
            .ok()
            .map(|s| s.split(',').map(|kid| kid.trim().to_string()).collect())
            .unwrap_or_else(|| {
                tracing::warn!("CAS_TRUSTED_KIDS not set, using default 'hub-key-1'. Ensure this matches Hub's HUB_KID configuration.");
                vec!["hub-key-1".to_string()]  // Changed from "test-kid" to match Hub default
            });

        // Conversion pipeline configuration
        let conversion_enabled = std::env::var("XET_CONVERSION_ENABLED")
            .ok()
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);
        let conversion_scheme = std::env::var("XET_CONVERSION_SCHEME")
            .unwrap_or_else(|_| "lz4".to_string());
        let delete_raw = std::env::var("XET_DELETE_RAW_AFTER_CONVERSION")
            .ok()
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);
        let min_conversion_size = match std::env::var("XET_MIN_CONVERSION_SIZE") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("XET_MIN_CONVERSION_SIZE '{}' is not a valid number, using default 1024", val);
                1024
            }),
            Err(_) => 1024,
        };
        let max_conversion_size = match std::env::var("XET_MAX_CONVERSION_SIZE") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("XET_MAX_CONVERSION_SIZE '{}' is not a valid number, using default 512MB", val);
                512 * 1024 * 1024
            }),
            Err(_) => 512 * 1024 * 1024,  // 512MB — match Hub max_upload_size
        };

        // GC configuration
        let gc_enabled = std::env::var("GC_ENABLED")
            .ok()
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);
        let gc_interval = match std::env::var("GC_INTERVAL_SECONDS") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("GC_INTERVAL_SECONDS '{}' is not a valid number, using default 3600", val);
                3600
            }),
            Err(_) => 3600,
        };
        let gc_grace_period = match std::env::var("GC_GRACE_PERIOD_SECONDS") {
            Ok(val) => val.parse().unwrap_or_else(|_| {
                tracing::warn!("GC_GRACE_PERIOD_SECONDS '{}' is not a valid number, using default 600", val);
                600
            }),
            Err(_) => 600,
        };
        let gc_dry_run = std::env::var("GC_DRY_RUN")
            .ok()
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);
        let gc_hub_base_url = std::env::var("GC_HUB_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());
        let gc_hub_internal_token = std::env::var("GC_HUB_INTERNAL_TOKEN")
            .unwrap_or_default();

        Self {
            server: ServerSettings { host, port, public_base_url, max_body_size_mb, rate_limit_rpm },
            storage: StorageConfig {
                backend,
                s3_bucket,
                s3_region,
                s3_endpoint,
                local_path,
                upload_temp_dir,
                verify_download_integrity,
            },
            auth: AuthConfig {
                public_key_path,
                trusted_kids,
            },
            conversion: ConversionConfig {
                enabled: conversion_enabled,
                compression_scheme: conversion_scheme,
                delete_raw_after_conversion: delete_raw,
                min_conversion_size,
                max_conversion_size,
            },
            gc: GcConfig {
                enabled: gc_enabled,
                interval_seconds: gc_interval,
                grace_period_seconds: gc_grace_period,
                dry_run: gc_dry_run,
                hub_base_url: gc_hub_base_url,
                hub_internal_token: gc_hub_internal_token,
            },
        }
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
