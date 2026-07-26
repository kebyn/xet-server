//! Tests for configuration module

use xet_server::config::{AuthConfig, ServerConfig, StorageConfig};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ScopedEnv {
    key: &'static str,
    previous: Option<String>,
}

impl ScopedEnv {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        unsafe {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

#[test]
fn test_config_default() {
    let config = ServerConfig::default();
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 8081); // Changed from 8080 to avoid conflict with Hub API
    assert_eq!(config.storage.backend, "local");
    assert!(!config.auth.trusted_kids.is_empty());
    assert!(!config.auth.public_key_path.is_empty());
}

#[test]
fn test_config_s3_settings() {
    let config = ServerConfig {
        storage: StorageConfig {
            backend: "s3".to_string(),
            s3_bucket: Some("test-bucket".to_string()),
            s3_region: Some("us-east-1".to_string()),
            s3_endpoint: Some("http://localhost:9000".to_string()),
            local_path: None,
            upload_temp_dir: None,
            reconstruction_temp_dir: None,
            verify_download_integrity: false,
        },
        ..Default::default()
    };

    assert_eq!(config.storage.backend, "s3");
    assert_eq!(config.storage.s3_bucket, Some("test-bucket".to_string()));
    assert_eq!(config.storage.s3_region, Some("us-east-1".to_string()));
}

#[test]
fn test_config_auth_settings() {
    let config = ServerConfig {
        auth: AuthConfig {
            public_key_path: "/path/to/key.pem".to_string(),
            trusted_kids: vec!["kid1".to_string(), "kid2".to_string()],
            private_key_path: None,
            signing_kid: None,
        },
        ..Default::default()
    };

    assert_eq!(config.auth.public_key_path, "/path/to/key.pem");
    assert_eq!(config.auth.trusted_kids.len(), 2);
}

#[test]
fn test_config_rate_limit_default() {
    let config = ServerConfig::default();
    assert_eq!(
        config.server.rate_limit_rpm, 60,
        "Default CAS rate limit should be 60 RPM"
    );
    assert!(
        !config.server.index_rebuild_strict,
        "CAS index rebuild strict mode should default to false for compatibility"
    );
}

#[test]
fn test_try_from_env_rejects_invalid_public_base_url_without_panic() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _url = ScopedEnv::set("XET_PUBLIC_BASE_URL", "http://");

    let err = ServerConfig::try_from_env().expect_err("invalid base URL should be rejected");

    assert!(err.contains("public_base_url"));
    assert!(err.contains("valid URL") || err.contains("valid host"));
}

#[test]
fn test_try_from_env_rejects_zero_rate_limit_without_panic() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _rate_limit = ScopedEnv::set("XET_RATE_LIMIT_RPM", "0");

    let err = ServerConfig::try_from_env().expect_err("zero rate limit should be rejected");

    assert!(err.contains("XET_RATE_LIMIT_RPM must be > 0"));
}

#[test]
fn test_try_from_env_rejects_invalid_numeric_values_without_fallback() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _public_base_url = ScopedEnv::remove("XET_PUBLIC_BASE_URL");

    for (key, value) in [
        ("XET_PORT", "not-a-port"),
        ("XET_MAX_BODY_SIZE_MB", "huge"),
        ("XET_RATE_LIMIT_RPM", "fast"),
        ("XET_MIN_CONVERSION_SIZE", "small"),
        ("XET_MAX_CONVERSION_SIZE", "large"),
    ] {
        let scoped = ScopedEnv::set(key, value);
        let err = match ServerConfig::try_from_env() {
            Ok(_) => panic!("{key}={value} should be rejected"),
            Err(err) => err,
        };
        assert!(
            err.contains(key) && err.contains("valid"),
            "unexpected error for {key}: {err}"
        );
        drop(scoped);
    }
}

#[test]
fn test_try_from_env_rejects_invalid_index_rebuild_strict_bool() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _public_base_url = ScopedEnv::remove("XET_PUBLIC_BASE_URL");
    let _strict = ScopedEnv::set("XET_INDEX_REBUILD_STRICT", "maybe");

    let err = ServerConfig::try_from_env()
        .expect_err("invalid XET_INDEX_REBUILD_STRICT should be rejected");

    assert!(err.contains("XET_INDEX_REBUILD_STRICT"));
    assert!(err.contains("valid boolean"));
}

#[test]
fn test_try_from_env_rejects_ambiguous_boolean_values() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _public_base_url = ScopedEnv::remove("XET_PUBLIC_BASE_URL");

    for key in [
        "XET_VERIFY_DOWNLOAD_INTEGRITY",
        "XET_CONVERSION_ENABLED",
        "XET_DELETE_RAW_AFTER_CONVERSION",
    ] {
        let scoped = ScopedEnv::set(key, "yes");
        let err = ServerConfig::try_from_env().expect_err("ambiguous boolean should be rejected");
        assert!(err.contains(key), "unexpected error for {key}: {err}");
        assert!(err.contains("valid boolean"));
        drop(scoped);
    }
}

#[test]
fn test_try_from_env_rejects_invalid_storage_and_conversion_settings() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _public_base_url = ScopedEnv::remove("XET_PUBLIC_BASE_URL");

    {
        let _backend = ScopedEnv::set("XET_STORAGE_BACKEND", "local");
        let _path = ScopedEnv::set("XET_LOCAL_PATH", "");
        let err = ServerConfig::try_from_env().expect_err("empty local path should be rejected");
        assert!(err.contains("XET_LOCAL_PATH"));
    }
    {
        let _backend = ScopedEnv::set("XET_STORAGE_BACKEND", "s3");
        let _bucket = ScopedEnv::remove("XET_S3_BUCKET");
        let err = ServerConfig::try_from_env().expect_err("missing S3 bucket should be rejected");
        assert!(err.contains("XET_S3_BUCKET"));
    }
    {
        let _scheme = ScopedEnv::set("XET_CONVERSION_SCHEME", "gzip");
        let err =
            ServerConfig::try_from_env().expect_err("unknown conversion scheme should be rejected");
        assert!(err.contains("XET_CONVERSION_SCHEME"));
    }
}

#[test]
fn test_try_from_env_rejects_invalid_auth_key_ids() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _public_base_url = ScopedEnv::remove("XET_PUBLIC_BASE_URL");

    for value in ["", "kid-1,,kid-2", "kid-1,kid-1"] {
        let scoped = ScopedEnv::set("CAS_TRUSTED_KIDS", value);
        let err = ServerConfig::try_from_env().expect_err("invalid key IDs should be rejected");
        assert!(err.contains("CAS_TRUSTED_KIDS"), "unexpected error: {err}");
        drop(scoped);
    }

    let _kids = ScopedEnv::set("CAS_TRUSTED_KIDS", "kid-1");
    let _signing = ScopedEnv::set("CAS_SIGNING_KID", "kid-2");
    let err = ServerConfig::try_from_env().expect_err("unknown signing key ID should be rejected");
    assert!(err.contains("CAS_SIGNING_KID"));
}

#[test]
fn test_try_from_env_requires_http_urls() {
    let _guard = ENV_LOCK.lock().unwrap();

    {
        let _url = ScopedEnv::set("XET_PUBLIC_BASE_URL", "ftp://example.com");
        let err = ServerConfig::try_from_env().expect_err("FTP public URL should be rejected");
        assert!(err.contains("unsupported scheme"));
    }
    {
        let _public_base_url = ScopedEnv::remove("XET_PUBLIC_BASE_URL");
        let _backend = ScopedEnv::set("XET_STORAGE_BACKEND", "s3");
        let _bucket = ScopedEnv::set("XET_S3_BUCKET", "bucket");
        let _endpoint = ScopedEnv::set("XET_S3_ENDPOINT", "ftp://example.com");
        let err = ServerConfig::try_from_env().expect_err("FTP S3 endpoint should be rejected");
        assert!(err.contains("XET_S3_ENDPOINT"));
        assert!(err.contains("unsupported scheme"));
    }
}

#[test]
fn test_config_serialization() {
    let config = ServerConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: ServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.server.port, config.server.port);
    assert_eq!(deserialized.auth.trusted_kids, config.auth.trusted_kids);
}

#[test]
fn test_check_public_key_permissions_insecure() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();

    let result = xet_server::config::check_public_key_permissions(path);
    assert!(
        result.is_some(),
        "World-writable key file should produce a warning"
    );
    assert!(result.unwrap().contains("world-writable"));
}

#[test]
fn test_check_public_key_permissions_secure() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();

    let result = xet_server::config::check_public_key_permissions(path);
    assert!(
        result.is_none(),
        "Secure key file should not produce a warning"
    );
}

#[test]
fn test_cas_default_host_is_localhost() {
    let config = ServerConfig::default();
    assert_eq!(
        config.server.host, "127.0.0.1",
        "CAS should default to localhost for dev safety"
    );
}

#[test]
fn test_min_conversion_size_default_64kb() {
    let config = ServerConfig::default();
    assert_eq!(
        config.conversion.min_conversion_size, 65536,
        "Default min_conversion_size should be 64KB (65536 bytes)"
    );
}

// GC config tests removed — GC module was removed from the project.
