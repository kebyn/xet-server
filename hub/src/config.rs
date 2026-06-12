use std::env;

/// Server configuration settings
#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_base_url: Option<String>,
}

impl ServerSettings {
    pub fn base_url(&self) -> String {
        self.public_base_url.clone()
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port))
            .trim_end_matches('/').to_string()
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            host: "0.0.0.0".to_string(),
            port: 8080,
            public_base_url: None,
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
    pub inline_threshold_bytes: u64,
    pub lfs_threshold_bytes: u64,
    /// Directory for temporary files during streaming uploads
    pub upload_temp_dir: String,
}

impl Default for StorageSettings {
    fn default() -> Self {
        StorageSettings {
            data_dir: "./data".to_string(),
            inline_threshold_bytes: 1024 * 1024, // 1MB
            lfs_threshold_bytes: 10 * 1024 * 1024,   // 10MB
            upload_temp_dir: "/tmp/hub-uploads".to_string(),
        }
    }
}

/// Main configuration for the Hub API
#[derive(Debug, Clone)]
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
            },
            metadata: MetadataSettings {
                sqlite_path: env::var("HUB_SQLITE_PATH")
                    .unwrap_or_else(|_| "hub.db".to_string()),
            },
            cas: CasSettings {
                base_url: env::var("CAS_BASE_URL")
                    .unwrap_or_else(|_| "http://localhost:3000".to_string()),
                internal_timeout_seconds: env::var("HUB_CAS_TIMEOUT_SECS")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(30),
            },
            storage: StorageSettings {
                data_dir: env::var("HUB_DATA_DIR")
                    .unwrap_or_else(|_| "./data".to_string()),
                inline_threshold_bytes: env::var("HUB_INLINE_THRESHOLD")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(1024 * 1024),
                lfs_threshold_bytes: env::var("HUB_LFS_THRESHOLD")
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(10 * 1024 * 1024),
                upload_temp_dir: env::var("HUB_UPLOAD_TEMP_DIR")
                    .unwrap_or_else(|_| "/tmp/hub-uploads".to_string()),
            },
        }
    }
}