use std::sync::Mutex;

use hub_api::config::HubConfig;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
fn test_try_from_file_or_env_rejects_invalid_public_base_url_without_panic() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _config_file = ScopedEnv::remove("HUB_CONFIG_FILE");
    let _url = ScopedEnv::set("HUB_PUBLIC_BASE_URL", "http://");

    let err = HubConfig::try_from_file_or_env()
        .expect_err("invalid Hub public base URL should be rejected");

    assert!(err.contains("HUB_PUBLIC_BASE_URL"));
    assert!(err.contains("valid URL") || err.contains("valid host"));
}

#[test]
fn test_try_from_file_or_env_rejects_invalid_cas_base_url_without_panic() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _config_file = ScopedEnv::remove("HUB_CONFIG_FILE");
    let _url = ScopedEnv::set("CAS_BASE_URL", "not a url");

    let err =
        HubConfig::try_from_file_or_env().expect_err("invalid CAS base URL should be rejected");

    assert!(err.contains("CAS_BASE_URL"));
    assert!(err.contains("valid URL"));
}

#[test]
fn test_try_from_file_or_env_rejects_zero_rate_limit_without_panic() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _config_file = ScopedEnv::remove("HUB_CONFIG_FILE");
    let _rate_limit = ScopedEnv::set("HUB_RATE_LIMIT_RPM", "0");

    let err =
        HubConfig::try_from_file_or_env().expect_err("zero Hub rate limit should be rejected");

    assert!(err.contains("HUB_RATE_LIMIT_RPM must be > 0"));
}

#[test]
fn test_try_from_file_or_env_rejects_invalid_numeric_values_without_fallback() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _config_file = ScopedEnv::remove("HUB_CONFIG_FILE");
    let _public_base_url = ScopedEnv::remove("HUB_PUBLIC_BASE_URL");
    let _cas_base_url = ScopedEnv::remove("CAS_BASE_URL");

    for (key, value) in [
        ("HUB_PORT", "not-a-port"),
        ("HUB_RATE_LIMIT_RPM", "fast"),
        ("HUB_DB_POOL_SIZE", "many"),
        ("HUB_TOKEN_TTL_SECONDS", "soon"),
        ("HUB_PROXY_TOKEN_TTL_SECONDS", "brief"),
        ("HUB_INTERNAL_TOKEN_TTL_SECONDS", "long"),
        ("HUB_CAS_TIMEOUT_SECS", "slow"),
        ("HUB_MAX_DOWNLOAD_SIZE", "large"),
        ("HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS", "later"),
        ("HUB_INLINE_THRESHOLD", "tiny"),
        ("HUB_MAX_UPLOAD_SIZE", "huge"),
    ] {
        let scoped = ScopedEnv::set(key, value);
        let err = match HubConfig::try_from_file_or_env() {
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
