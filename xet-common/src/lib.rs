//! Shared helpers for the xet-server workspace.
//!
//! Small utilities that would otherwise be copy-pasted between the CAS
//! server (`xet-server`) and the Hub API (`hub-api`): rate-limit math,
//! environment/URL config parsing, and the uniform internal-error message.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use thiserror::Error;

/// Uniform message for HTTP 500 responses that must not leak internal details.
pub const INTERNAL_ERROR_MESSAGE: &str = "Internal server error";

/// A process-local ledger for temporary disk usage. Reservations are charged
/// before bytes are written, so concurrent writers cannot collectively exceed
/// the configured budget. The ledger is deliberately process-local; deployments
/// that need hard isolation should use separate filesystems or quotas.
#[derive(Clone, Debug)]
pub struct TempQuotaLedger {
    quota_bytes: u64,
    min_free_bytes: u64,
    charged_bytes: Arc<AtomicU64>,
    reserved_bytes: Arc<AtomicU64>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TempQuotaError {
    #[error("temporary quota exhausted")]
    QuotaExhausted,
    #[error("temporary filesystem free space is below the configured reserve")]
    InsufficientFreeSpace,
    #[error("temporary quota arithmetic overflow")]
    Overflow,
}

impl TempQuotaLedger {
    pub fn new(quota_bytes: u64, min_free_bytes: u64) -> Result<Self, TempQuotaError> {
        if quota_bytes == 0 || min_free_bytes == 0 {
            return Err(TempQuotaError::Overflow);
        }
        Ok(Self {
            quota_bytes,
            min_free_bytes,
            charged_bytes: Arc::new(AtomicU64::new(0)),
            reserved_bytes: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }
    pub fn min_free_bytes(&self) -> u64 {
        self.min_free_bytes
    }
    pub fn charged_bytes(&self) -> u64 {
        self.charged_bytes.load(Ordering::Acquire)
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes.load(Ordering::Acquire)
    }

    /// Reconcile bytes left by a previous process run before accepting writes.
    pub fn account_existing(&self, bytes: u64) -> Result<(), TempQuotaError> {
        let next = self
            .charged_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(bytes)
            })
            .map_err(|_| TempQuotaError::Overflow)?;
        if next > self.quota_bytes {
            self.charged_bytes.fetch_sub(bytes, Ordering::AcqRel);
            return Err(TempQuotaError::QuotaExhausted);
        }
        Ok(())
    }

    pub fn reserve(&self, bytes: u64) -> Result<TempReservation, TempQuotaError> {
        if bytes == 0 {
            return Ok(TempReservation::empty(self.clone()));
        }
        loop {
            let charged = self.charged_bytes.load(Ordering::Acquire);
            let reserved = self.reserved_bytes.load(Ordering::Acquire);
            let total = charged
                .checked_add(reserved)
                .and_then(|v| v.checked_add(bytes))
                .ok_or(TempQuotaError::Overflow)?;
            if total > self.quota_bytes {
                return Err(TempQuotaError::QuotaExhausted);
            }
            if self
                .reserved_bytes
                .compare_exchange(
                    reserved,
                    reserved + bytes,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(TempReservation {
                    ledger: self.clone(),
                    reserved: bytes,
                    charged: 0,
                });
            }
        }
    }

    fn commit(&self, bytes: u64) -> Result<(), TempQuotaError> {
        if bytes == 0 {
            return Ok(());
        }
        self.reserved_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.charged_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(bytes)
            })
            .map_err(|_| TempQuotaError::Overflow)?;
        Ok(())
    }

    fn release_reserved(&self, bytes: u64) {
        if bytes != 0 {
            self.reserved_bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
    fn release_charged(&self, bytes: u64) {
        if bytes != 0 {
            self.charged_bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

/// A reservation held by one temporary file. Unknown-length streams can grow
/// it incrementally; dropping it releases every byte that has not been
/// explicitly committed as written.
#[derive(Debug)]
pub struct TempReservation {
    ledger: TempQuotaLedger,
    reserved: u64,
    charged: u64,
}

impl TempReservation {
    fn empty(ledger: TempQuotaLedger) -> Self {
        Self {
            ledger,
            reserved: 0,
            charged: 0,
        }
    }

    pub fn reserve_additional(&mut self, bytes: u64) -> Result<(), TempQuotaError> {
        let additional = self.ledger.reserve(bytes)?;
        self.reserved = self
            .reserved
            .checked_add(additional.reserved)
            .ok_or(TempQuotaError::Overflow)?;
        std::mem::forget(additional);
        Ok(())
    }

    pub fn commit_written(&mut self, bytes: u64) -> Result<(), TempQuotaError> {
        if bytes > self.reserved {
            return Err(TempQuotaError::Overflow);
        }
        self.ledger.commit(bytes)?;
        self.reserved -= bytes;
        self.charged = self
            .charged
            .checked_add(bytes)
            .ok_or(TempQuotaError::Overflow)?;
        Ok(())
    }

    pub fn release_written(&mut self, bytes: u64) -> Result<(), TempQuotaError> {
        if bytes > self.charged {
            return Err(TempQuotaError::Overflow);
        }
        self.ledger.release_charged(bytes);
        self.charged -= bytes;
        Ok(())
    }

    pub fn reserved_bytes(&self) -> u64 {
        self.reserved
    }
    pub fn charged_bytes(&self) -> u64 {
        self.charged
    }
}

impl Drop for TempReservation {
    fn drop(&mut self) {
        self.ledger.release_reserved(self.reserved);
        self.ledger.release_charged(self.charged);
        self.reserved = 0;
        self.charged = 0;
    }
}

/// Directory-aware wrapper used by services to enforce the free-space reserve
/// before taking a reservation. The ledger itself remains shareable across
/// upload and reconstruction directories on the same filesystem.
#[derive(Clone, Debug)]
pub struct TempResourceManager {
    directory: PathBuf,
    ledger: TempQuotaLedger,
}

/// Process-exclusive lock for a managed temporary directory. The lock file is
/// intentionally separate from quota accounting and is removed on clean drop.
#[derive(Debug)]
pub struct TempDirectoryLock {
    #[cfg(not(unix))]
    path: PathBuf,
    #[cfg(unix)]
    file: Option<nix::fcntl::Flock<std::fs::File>>,
    #[cfg(not(unix))]
    file: Option<std::fs::File>,
}

pub fn acquire_temp_directory_lock(directory: &Path) -> std::io::Result<TempDirectoryLock> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(".xet-temp.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    #[cfg(unix)]
    let mut file = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, error)| std::io::Error::other(error))?;
    #[cfg(not(unix))]
    let mut file = file;
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(TempDirectoryLock {
        #[cfg(not(unix))]
        path,
        file: Some(file),
    })
}

/// Remove residual files owned by the current temp-file manager and return the
/// bytes that could not be removed. Unknown files and the lock file are left
/// untouched; callers can charge the returned bytes before accepting work.
pub fn cleanup_temp_directory(directory: &Path) -> std::io::Result<u64> {
    let mut retained = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name == ".xet-temp.lock")
        {
            continue;
        }
        let managed_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                [
                    "upload-",
                    "blob-",
                    "shard-",
                    "reconstruct-",
                    "validate-xorb-",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix))
                    && name.ends_with(".tmp")
            });
        if !managed_name {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(_) => retained = retained.saturating_add(metadata.len()),
        }
    }
    Ok(retained)
}

impl Drop for TempDirectoryLock {
    fn drop(&mut self) {
        self.file.take();
        #[cfg(not(unix))]
        let _ = std::fs::remove_file(&self.path);
    }
}

impl TempResourceManager {
    pub fn new(
        directory: impl Into<PathBuf>,
        quota_bytes: u64,
        min_free_bytes: u64,
    ) -> Result<Self, TempQuotaError> {
        let ledger = TempQuotaLedger::new(quota_bytes, min_free_bytes)?;
        Ok(Self {
            directory: directory.into(),
            ledger,
        })
    }

    pub fn with_ledger(directory: impl Into<PathBuf>, ledger: TempQuotaLedger) -> Self {
        Self {
            directory: directory.into(),
            ledger,
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn ledger(&self) -> &TempQuotaLedger {
        &self.ledger
    }

    pub fn reserve(&self, bytes: u64) -> Result<TempReservation, TempQuotaError> {
        let available = available_bytes(&self.directory).unwrap_or(u64::MAX);
        let needed = self
            .ledger
            .min_free_bytes()
            .checked_add(self.ledger.reserved_bytes())
            .and_then(|v| v.checked_add(bytes))
            .ok_or(TempQuotaError::Overflow)?;
        if available < needed {
            return Err(TempQuotaError::InsufficientFreeSpace);
        }
        self.ledger.reserve(bytes)
    }
}

#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
    nix::sys::statvfs::statvfs(path)
        .ok()
        .map(|stat| stat.fragment_size().saturating_mul(stat.blocks_available()))
}

#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

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

    #[test]
    fn temporary_quota_reservations_are_bounded_and_released() {
        let ledger = TempQuotaLedger::new(10, 1).unwrap();
        let mut first = ledger.reserve(6).unwrap();
        assert_eq!(ledger.reserved_bytes(), 6);
        assert!(matches!(
            ledger.reserve(5),
            Err(TempQuotaError::QuotaExhausted)
        ));
        first.commit_written(4).unwrap();
        assert_eq!(ledger.charged_bytes(), 4);
        assert_eq!(ledger.reserved_bytes(), 2);
        drop(first);
        assert_eq!(ledger.charged_bytes(), 0);
        assert_eq!(ledger.reserved_bytes(), 0);
    }

    #[test]
    fn temporary_quota_supports_unknown_length_growth() {
        let ledger = TempQuotaLedger::new(8, 1).unwrap();
        let mut reservation = ledger.reserve(0).unwrap();
        reservation.reserve_additional(3).unwrap();
        reservation.commit_written(3).unwrap();
        reservation.reserve_additional(5).unwrap();
        assert_eq!(reservation.reserved_bytes(), 5);
        assert!(matches!(
            ledger.reserve(1),
            Err(TempQuotaError::QuotaExhausted)
        ));
        drop(reservation);
        assert_eq!(ledger.charged_bytes(), 0);
    }

    #[test]
    fn temporary_quota_rejects_zero_configuration() {
        assert!(matches!(
            TempQuotaLedger::new(0, 1),
            Err(TempQuotaError::Overflow)
        ));
        assert!(matches!(
            TempQuotaLedger::new(1, 0),
            Err(TempQuotaError::Overflow)
        ));
    }
}
