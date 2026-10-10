//! Test-only storage wrapper that injects failures for a target key.

use super::{ObjectKeyStream, StorageBackend, StorageError, StorageResult};
use crate::storage::local::LocalStorage;
use async_trait::async_trait;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Storage failure the [`FlakyStorage`] wrapper injects for its target key.
pub(crate) enum InjectedFailure {
    /// Network-class storage error (retryable by the bounded retry helpers).
    Transient,
    /// The object is absent (must NOT be retried).
    Missing,
    /// Panic in the worker to exercise join error propagation.
    Panic,
}

/// Storage wrapper that fails `get_path` and `download_to_path` for a target
/// key while failures remain, then delegates — simulates a transient storage
/// blip around an otherwise-working backend.
///
/// The handle returned by [`FlakyStorage::new`] counts every call of an
/// injected method for the target key, so tests can pin exact attempt
/// counts.
pub(crate) struct FlakyStorage {
    inner: LocalStorage,
    target_key: String,
    inject: InjectedFailure,
    remaining: AtomicUsize,
    attempts: Arc<AtomicUsize>,
}

impl FlakyStorage {
    /// `failures` is the number of target-key calls that fail before
    /// delegating; `usize::MAX` fails forever.
    pub(crate) fn new(
        inner: LocalStorage,
        target_key: String,
        inject: InjectedFailure,
        failures: usize,
    ) -> (Self, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner,
                target_key,
                inject,
                remaining: AtomicUsize::new(failures),
                attempts: attempts.clone(),
            },
            attempts,
        )
    }

    /// Count a target-key call and, while failures remain, produce the
    /// injected error.
    fn gate(&self, key: &str) -> Option<StorageError> {
        if key != self.target_key {
            return None;
        }
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.remaining.load(Ordering::SeqCst) > 0 {
            self.remaining.fetch_sub(1, Ordering::SeqCst);
            return Some(match self.inject {
                InjectedFailure::Transient => {
                    StorageError::internal(format!("injected transient failure for {key}"))
                }
                InjectedFailure::Missing => StorageError::NotFound(key.to_string()),
                InjectedFailure::Panic => panic!("injected storage task panic"),
            });
        }
        None
    }
}

#[async_trait]
impl StorageBackend for FlakyStorage {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        self.inner.put(key, data).await
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        self.inner.get(key).await
    }

    async fn get_path(&self, key: &str) -> StorageResult<Option<PathBuf>> {
        if let Some(error) = self.gate(key) {
            return Err(error);
        }
        self.inner.get_path(key).await
    }

    async fn exists(&self, key: &str) -> StorageResult<bool> {
        self.inner.exists(key).await
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        self.inner.delete(key).await
    }

    async fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        self.inner.list_objects(prefix).await
    }

    fn list_objects_stream<'a>(&'a self, prefix: &'a str) -> ObjectKeyStream<'a> {
        self.inner.list_objects_stream(prefix)
    }

    async fn get_size(&self, key: &str) -> StorageResult<u64> {
        self.inner.get_size(key).await
    }

    async fn download_to_path(&self, key: &str, dest: &Path) -> StorageResult<()> {
        if let Some(error) = self.gate(key) {
            return Err(error);
        }
        self.inner.download_to_path(key, dest).await
    }
}
