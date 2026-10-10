use futures_util::FutureExt;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use parking_lot::RwLock;

/// Tracks OIDs currently being converted (prevents duplicate concurrent conversions).
/// In-memory only — resets on restart (acceptable: reconversion is idempotent).
///
/// Uses a `parking_lot::RwLock` (like the metadata index and readiness state):
/// parking_lot locks are not poisoned, so the former std-lock poison-recovery
/// boilerplate is unnecessary.
pub struct ConvertingOids {
    inner: RwLock<HashSet<String>>,
    scheduler: RwLock<Option<std::sync::Arc<ConversionScheduler>>>,
}

impl ConvertingOids {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashSet::new()),
            scheduler: RwLock::new(None),
        }
    }

    /// Try to mark an OID as converting. Returns true if successfully marked
    /// (not already being converted by another task).
    pub fn try_acquire(&self, oid: &str) -> bool {
        self.inner.write().insert(oid.to_string())
    }

    /// Release the conversion lock for an OID.
    pub fn release(&self, oid: &str) {
        self.inner.write().remove(oid);
    }

    pub fn set_scheduler(&self, scheduler: std::sync::Arc<ConversionScheduler>) {
        *self.scheduler.write() = Some(scheduler);
    }

    pub fn scheduler(&self) -> Option<std::sync::Arc<ConversionScheduler>> {
        self.scheduler.read().clone()
    }

    pub fn try_enqueue(&self, oid: &str) -> Option<EnqueueResult> {
        self.scheduler().map(|scheduler| scheduler.try_enqueue(oid))
    }
}

impl Default for ConvertingOids {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    Accepted,
    Duplicate,
    QueueFull,
    ShuttingDown,
}

/// A bounded, process-wide FIFO scheduler for lazy conversions. Worker tasks
/// execute conversions directly, so no unbounded set of semaphore-waiting
/// futures is created when the queue is busy.
pub struct ConversionScheduler {
    sender: Mutex<Option<mpsc::Sender<String>>>,
    state: std::sync::Arc<parking_lot::Mutex<HashSet<String>>>,
    cancel: CancellationToken,
    workers: parking_lot::Mutex<JoinSet<()>>,
    pub(crate) queued: AtomicU64,
    pub(crate) running: AtomicU64,
    pub(crate) accepted: AtomicU64,
    pub(crate) rejected_full: AtomicU64,
    pub(crate) rejected_duplicate: AtomicU64,
    closed: AtomicBool,
}

impl std::fmt::Debug for ConversionScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversionScheduler")
            .field("queued", &self.queued.load(Ordering::Relaxed))
            .field("running", &self.running.load(Ordering::Relaxed))
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl ConversionScheduler {
    pub fn new(
        pipeline: std::sync::Arc<super::ConversionPipeline>,
        concurrency: usize,
        queue_capacity: usize,
    ) -> Result<std::sync::Arc<Self>, String> {
        if concurrency == 0 || queue_capacity == 0 {
            return Err("conversion scheduler limits must be greater than zero".to_string());
        }
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let state = std::sync::Arc::new(parking_lot::Mutex::new(HashSet::new()));
        let cancel = CancellationToken::new();
        let scheduler = std::sync::Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            state: state.clone(),
            cancel: cancel.clone(),
            workers: parking_lot::Mutex::new(JoinSet::new()),
            queued: AtomicU64::new(0),
            running: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            rejected_full: AtomicU64::new(0),
            rejected_duplicate: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });

        let receiver = std::sync::Arc::new(tokio::sync::Mutex::new(receiver));
        for _ in 0..concurrency {
            let pipeline = pipeline.clone();
            let state = state.clone();
            let cancel = cancel.clone();
            let scheduler_ref = scheduler.clone();
            let receiver = receiver.clone();
            scheduler.workers.lock().spawn(async move {
                loop {
                    let oid = tokio::select! {
                        _ = cancel.cancelled() => break,
                        oid = async {
                            let mut receiver = receiver.lock().await;
                            receiver.recv().await
                        } => match oid { Some(oid) => oid, None => break },
                    };
                    scheduler_ref.queued.fetch_sub(1, Ordering::Relaxed);
                    crate::metrics::GLOBAL_METRICS
                        .conversion_queued
                        .fetch_sub(1, Ordering::Relaxed);
                    scheduler_ref.running.fetch_add(1, Ordering::Relaxed);
                    crate::metrics::GLOBAL_METRICS
                        .conversion_running
                        .fetch_add(1, Ordering::Relaxed);
                    let result = tokio::select! {
                        result = std::panic::AssertUnwindSafe(pipeline.convert(&oid)).catch_unwind() => result,
                        _ = cancel.cancelled() => {
                            state.lock().remove(&oid);
                            scheduler_ref.running.fetch_sub(1, Ordering::Relaxed);
                            crate::metrics::GLOBAL_METRICS.conversion_running.fetch_sub(1, Ordering::Relaxed);
                            continue;
                        }
                    };
                    scheduler_ref.running.fetch_sub(1, Ordering::Relaxed);
                    crate::metrics::GLOBAL_METRICS
                        .conversion_running
                        .fetch_sub(1, Ordering::Relaxed);
                    state.lock().remove(&oid);
                    match result {
                        Ok(Ok(result)) => {
                            tracing::info!(oid = %oid, chunks = result.num_chunks, "lazy conversion completed")
                        }
                        Ok(Err(error)) => {
                            crate::metrics::GLOBAL_METRICS
                                .conversion_failures
                                .fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(oid = %oid, error = %error, "lazy conversion failed; raw blob preserved")
                        }
                        Err(_) => {
                            crate::metrics::GLOBAL_METRICS
                                .conversion_failures
                                .fetch_add(1, Ordering::Relaxed);
                            tracing::error!(oid = %oid, "lazy conversion panicked; raw blob preserved")
                        }
                    }
                }
            });
        }
        Ok(scheduler)
    }

    pub fn try_enqueue(&self, oid: &str) -> EnqueueResult {
        if self.closed.load(Ordering::Acquire) {
            return EnqueueResult::ShuttingDown;
        }
        {
            let mut state = self.state.lock();
            if !state.insert(oid.to_string()) {
                self.rejected_duplicate.fetch_add(1, Ordering::Relaxed);
                return EnqueueResult::Duplicate;
            }
        }
        let sender = self.sender.try_lock();
        let Ok(sender) = sender else {
            self.state.lock().remove(oid);
            self.rejected_full.fetch_add(1, Ordering::Relaxed);
            return EnqueueResult::QueueFull;
        };
        let Some(sender) = sender.as_ref() else {
            self.state.lock().remove(oid);
            return EnqueueResult::ShuttingDown;
        };
        match sender.try_send(oid.to_string()) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::Relaxed);
                crate::metrics::GLOBAL_METRICS
                    .conversion_queued
                    .fetch_add(1, Ordering::Relaxed);
                self.accepted.fetch_add(1, Ordering::Relaxed);
                EnqueueResult::Accepted
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.state.lock().remove(oid);
                self.rejected_full.fetch_add(1, Ordering::Relaxed);
                crate::metrics::GLOBAL_METRICS
                    .conversion_enqueue_rejected
                    .fetch_add(1, Ordering::Relaxed);
                EnqueueResult::QueueFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.state.lock().remove(oid);
                EnqueueResult::ShuttingDown
            }
        }
    }

    pub fn queued_count(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }
    pub fn running_count(&self) -> u64 {
        self.running.load(Ordering::Relaxed)
    }

    pub async fn shutdown(&self, grace: Duration) {
        self.closed.store(true, Ordering::Release);
        self.sender.lock().await.take();
        self.cancel.cancel();
        let mut workers = std::mem::take(&mut *self.workers.lock());
        let queued_before_shutdown = self.queued.swap(0, Ordering::AcqRel);
        crate::metrics::GLOBAL_METRICS
            .conversion_queued
            .fetch_sub(queued_before_shutdown, Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            tokio::select! {
                result = workers.join_next() => {
                    if result.is_none() { break; }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    workers.abort_all();
                    while workers.join_next().await.is_some() {}
                    break;
                }
            }
        }
        self.state.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{StorageBackend, StorageResult};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[test]
    fn test_converting_oids_acquire_release() {
        let oids = ConvertingOids::new();

        // First acquire should succeed
        assert!(oids.try_acquire("abc123"));

        // Second acquire of same OID should fail (already converting)
        assert!(!oids.try_acquire("abc123"));

        // Different OID should succeed
        assert!(oids.try_acquire("def456"));

        // Release and re-acquire should work
        oids.release("abc123");
        assert!(oids.try_acquire("abc123"));

        // Other OID still held
        assert!(!oids.try_acquire("def456"));
    }

    struct BlockingStorage {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl StorageBackend for BlockingStorage {
        async fn put(&self, _key: &str, _data: Bytes) -> StorageResult<()> {
            Ok(())
        }
        async fn get(&self, _key: &str) -> StorageResult<Bytes> {
            Ok(Bytes::new())
        }
        async fn get_path(&self, _key: &str) -> StorageResult<Option<PathBuf>> {
            Ok(None)
        }
        async fn get_size(&self, _key: &str) -> StorageResult<u64> {
            self.started.notify_one();
            std::future::pending().await
        }
        async fn exists(&self, _key: &str) -> StorageResult<bool> {
            Ok(false)
        }
        async fn delete(&self, _key: &str) -> StorageResult<()> {
            Ok(())
        }
        async fn download_to_path(&self, _key: &str, _dest: &Path) -> StorageResult<()> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn scheduler_deduplicates_and_bounds_queue() {
        let started = Arc::new(tokio::sync::Notify::new());
        let storage: Arc<Box<dyn StorageBackend>> = Arc::new(Box::new(BlockingStorage {
            started: started.clone(),
        }));
        let pipeline = Arc::new(super::super::ConversionPipeline::new(
            storage,
            Arc::new(crate::index::MetadataIndex::new()),
            crate::config::ConversionConfig::default(),
        ));
        let scheduler = ConversionScheduler::new(pipeline, 1, 1).unwrap();
        assert_eq!(scheduler.try_enqueue("a"), EnqueueResult::Accepted);
        started.notified().await;
        assert_eq!(scheduler.try_enqueue("b"), EnqueueResult::Accepted);
        assert_eq!(scheduler.try_enqueue("b"), EnqueueResult::Duplicate);
        assert_eq!(scheduler.try_enqueue("c"), EnqueueResult::QueueFull);
        assert_eq!(scheduler.queued_count(), 1);
        scheduler.shutdown(Duration::from_millis(50)).await;
        assert_eq!(scheduler.try_enqueue("d"), EnqueueResult::ShuttingDown);
    }
}
