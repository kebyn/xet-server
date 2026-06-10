//! Tests for configuration module

use xet_server::config::{ServerConfig, StorageConfig};

#[test]
fn test_config_default() {
    let config = ServerConfig::default();
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 8080);
    assert_eq!(config.storage.backend, "local");
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
        },
        ..Default::default()
    };

    assert_eq!(config.storage.backend, "s3");
    assert_eq!(config.storage.s3_bucket, Some("test-bucket".to_string()));
    assert_eq!(config.storage.s3_region, Some("us-east-1".to_string()));
}

#[test]
fn test_config_serialization() {
    let config = ServerConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: ServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.server.port, config.server.port);
}
