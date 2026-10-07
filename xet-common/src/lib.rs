//! Shared helpers for the xet-server workspace.
//!
//! Small utilities that would otherwise be copy-pasted between the CAS
//! server (`xet-server`) and the Hub API (`hub-api`): rate-limit math,
//! environment/URL config parsing, and the uniform internal-error message.

use std::str::FromStr;
use std::time::Duration;

/// Uniform message for HTTP 500 responses that must not leak internal details.
pub const INTERNAL_ERROR_MESSAGE: &str = "Internal server error";

const NANOS_PER_MINUTE: u64 = 60_000_000_000;

/// Token replenishment period for a requests-per-minute rate limit.
///
/// `rpm == 0` disables rate limiting (returns `None`); otherwise the period
/// is one minute divided by the RPM, rounded up so the sustained rate never
/// exceeds the configured value.
pub fn rate_limit_period(rpm: u32) -> Option<Duration> {
    if rpm == 0 {
        return None;
    }

    let period_nanos = NANOS_PER_MINUTE.div_ceil(u64::from(rpm));
    Some(Duration::from_nanos(period_nanos))
}

/// Parse an environment variable as `T`, falling back to `default` when unset.
///
/// An explicitly set but invalid value is an error naming the variable, so
/// misconfiguration fails loudly instead of silently falling back.
pub fn parse_env<T>(key: &str, default: T) -> Result<T, String>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(value) => value
            .parse()
            .map_err(|e| format!("{key} '{value}' is not a valid value: {e}")),
        Err(_) => Ok(default),
    }
}

/// Validate that `url` is an http(s) URL with a host.
///
/// `name` identifies the setting in error messages (e.g. an environment
/// variable name).
pub fn validate_http_url(name: &str, url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url)
        .map_err(|error| format!("{} '{}' is not a valid URL: {}", name, url, error))?;
    if parsed.host().is_none() {
        return Err(format!("{} '{}' is missing a valid host", name, url));
    }
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!(
            "{} '{}' uses unsupported scheme '{}'; expected http or https",
            name,
            url,
            parsed.scheme()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_period_matches_requests_per_minute() {
        assert_eq!(rate_limit_period(10), Some(Duration::from_secs(6)));
        assert_eq!(rate_limit_period(60), Some(Duration::from_secs(1)));
        assert_eq!(rate_limit_period(120), Some(Duration::from_millis(500)));
    }

    #[test]
    fn rate_limit_period_rejects_zero_and_rounds_up() {
        assert_eq!(rate_limit_period(0), None);
        assert_eq!(
            rate_limit_period(7),
            Some(Duration::from_nanos(8_571_428_572))
        );
    }

    #[test]
    fn parse_env_returns_default_when_unset_and_names_bad_values() {
        // Unset → default (variable name chosen to be safely absent).
        let value: u32 = parse_env("XET_COMMON_DEFINITELY_UNSET_VAR", 42).unwrap();
        assert_eq!(value, 42);

        unsafe { std::env::set_var("XET_COMMON_TEST_VAR", "not-a-number") };
        let err = parse_env::<u32>("XET_COMMON_TEST_VAR", 42).unwrap_err();
        unsafe { std::env::remove_var("XET_COMMON_TEST_VAR") };
        assert!(err.contains("XET_COMMON_TEST_VAR"));
        assert!(err.contains("not a valid value"));
    }

    #[test]
    fn validate_http_url_accepts_http_and_rejects_other_schemes() {
        validate_http_url("TEST_URL", "http://localhost:8081").unwrap();
        validate_http_url("TEST_URL", "https://example.com/path").unwrap();

        let err = validate_http_url("TEST_URL", "ftp://example.com").unwrap_err();
        assert!(err.contains("unsupported scheme"));

        let err = validate_http_url("TEST_URL", "mailto:user@example.com").unwrap_err();
        assert!(err.contains("missing a valid host"));

        let err = validate_http_url("TEST_URL", "not a url").unwrap_err();
        assert!(err.contains("not a valid URL"));
    }
}
