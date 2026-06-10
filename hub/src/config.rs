use std::env;

/// Server configuration settings
#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
}

impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            host: "0.0.0.0".to_string(),
            port: 8080,
        }
    }
}

/// Authentication settings
#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub private_key_path: String,
    pub kid: String,
    pub token_ttl_seconds: u64,
}

impl Default for AuthSettings {
    fn default() -> Self {
        AuthSettings {
            private_key_path: "private_key.pem".to_string(),
            kid: "hub-key-1".to_string(),
            token_ttl_seconds: 3600,
        }
    }
}

/// Metadata store settings
#[derive(Debug, Clone)]
pub struct MetadataSettings {
    pub sqlite_path: String,
}

impl Default for MetadataSettings {
    fn default() -> Self {
        MetadataSettings {
            sqlite_path: "hub.db".to_string(),
        }
    }
}

/// CAS (Content Addressable Storage) settings
#[derive(Debug, Clone)]
pub struct CasSettings {
    pub base_url: String,
    pub internal_timeout_seconds: u64,
}

impl Default for CasSettings {
    fn default() -> Self {
        CasSettings {
            base_url: "http://localhost:3000".to_string(),
            internal_timeout_seconds: 30,
        }
    }
}

/// Storage settings
#[derive(Debug, Clone)]
pub struct StorageSettings {
    pub data_dir: String,
}

impl Default for StorageSettings {
    fn default() -> Self {
        StorageSettings {
            data_dir: "./data".to_string(),
        }
    }
}

/// Main configuration for the Hub API
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub server: ServerSettings,
    pub auth: AuthSettings,
    pub metadata: MetadataSettings,
    pub cas: CasSettings,
    pub storage: StorageSettings,
}

impl Default for HubConfig {
    fn default() -> Self {
        HubConfig {
            server: ServerSettings::default(),
            auth: AuthSettings::default(),
            metadata: MetadataSettings::default(),
            cas: CasSettings::default(),
            storage: StorageSettings::default(),
        }
    }
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
            },
            auth: AuthSettings {
                private_key_path: env::var("HUB_PRIVATE_KEY_PATH")
                    .unwrap_or_else(|_| "private_key.pem".to_string()),
                kid: env::var("HUB_KEY_ID")
                    .unwrap_or_else(|_| "hub-key-1".to_string()),
                token_ttl_seconds: env::var("HUB_TOKEN_TTL_SECS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(3600),
            },
            metadata: MetadataSettings {
                sqlite_path: env::var("HUB_DATABASE_URL")
                    .unwrap_or_else(|_| "hub.db".to_string()),
            },
            cas: CasSettings {
                base_url: env::var("HUB_CAS_URL")
                    .unwrap_or_else(|_| "http://localhost:3000".to_string()),
                internal_timeout_seconds: env::var("HUB_CAS_TIMEOUT_SECS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(30),
            },
            storage: StorageSettings {
                data_dir: env::var("HUB_DATA_DIR")
                    .unwrap_or_else(|_| "./data".to_string()),
            },
        }
    }
}