//! Tests for configuration module

use xet_server::config::{ServerConfig, StorageConfig, AuthConfig};

#[test]
fn test_config_default() {
    let config = ServerConfig::default();
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 8081);  // Changed from 8080 to avoid conflict with Hub API
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
    assert_eq!(config.server.rate_limit_rpm, 60, "Default CAS rate limit should be 60 RPM");
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
    assert!(result.is_some(), "World-writable key file should produce a warning");
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
    assert!(result.is_none(), "Secure key file should not produce a warning");
}

#[test]
fn test_cas_default_host_is_localhost() {
    let config = ServerConfig::default();
    assert_eq!(config.server.host, "127.0.0.1", "CAS should default to localhost for dev safety");
}

#[test]
fn test_min_conversion_size_default_64kb() {
    let config = ServerConfig::default();
    assert_eq!(config.conversion.min_conversion_size, 65536,
        "Default min_conversion_size should be 64KB (65536 bytes)");
}

#[test]
fn test_gc_token_validation() {
    use xet_server::config::validate_gc_config;

    // GC disabled — token can be empty
    let config = ServerConfig {
        gc: xet_server::config::GcConfig { enabled: false, hub_internal_token: String::new(), ..Default::default() },
        ..Default::default()
    };
    assert!(validate_gc_config(&config).is_empty());

    // GC enabled but token empty — should warn
    let config = ServerConfig {
        gc: xet_server::config::GcConfig { enabled: true, hub_internal_token: String::new(), ..Default::default() },
        ..Default::default()
    };
    let warnings = validate_gc_config(&config);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("GC_HUB_INTERNAL_TOKEN"));

    // GC enabled with token — no warning
    let config = ServerConfig {
        gc: xet_server::config::GcConfig { enabled: true, hub_internal_token: "secret".to_string(), ..Default::default() },
        ..Default::default()
    };
    assert!(validate_gc_config(&config).is_empty());
}

#[test]
fn test_gc_http_timeout_default() {
    let config = ServerConfig::default();
    assert_eq!(config.gc.http_timeout_seconds, 300);
}
