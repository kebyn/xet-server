//! HTTP middleware for metrics collection

use actix_web::{
    Error,
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    middleware::Next,
};

use crate::metrics::GLOBAL_METRICS;

/// RAII guard that ensures connection_closed() is called when dropped.
///
/// This guarantees the metric is decremented even if the handler panics,
/// preventing active_connections from drifting upward over time.
struct ConnectionGuard;

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        GLOBAL_METRICS.connection_closed();
    }
}

/// Middleware that tracks active connections
///
/// Calls `connection_opened()` when request arrives and `connection_closed()`
/// when handler completes (success, error, or panic).
pub async fn metrics_middleware(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    GLOBAL_METRICS.connection_opened();
    let _guard = ConnectionGuard;

    next.call(req).await
    // _guard is dropped here, calling connection_closed()
}
