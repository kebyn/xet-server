//! HTTP middleware for metrics collection

use actix_web::{
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    Error,
    middleware::Next,
};

use crate::metrics::GLOBAL_METRICS;

/// Middleware that tracks active connections
///
/// Calls `connection_opened()` when request arrives and `connection_closed()`
/// when handler completes (success or error).
pub async fn metrics_middleware(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    GLOBAL_METRICS.connection_opened();

    let result = next.call(req).await;

    GLOBAL_METRICS.connection_closed();

    result
}
