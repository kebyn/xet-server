//! HTTP middleware for metrics collection

use actix_web::{
    Error,
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    middleware::Next,
};

use crate::metrics::GLOBAL_METRICS;

/// Endpoints excluded from request metrics: high-frequency monitoring probes
/// would dilute the business signals. They are still counted in the in-flight
/// gauge.
const UNMETRICED_PATHS: [&str; 2] = ["/health", "/ready"];

/// RAII guard that ensures the in-flight gauge is decremented when dropped.
///
/// This guarantees the metric is decremented even if the handler panics,
/// preventing active_requests from drifting upward over time.
struct InFlightGuard;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        GLOBAL_METRICS.request_completed();
    }
}

/// Middleware that records request metrics.
///
/// Tracks the in-flight request gauge for every request and — except for
/// /health and /ready — records the request count, status bucket, error count
/// (5xx), and latency once the response resolves. This replaces the former
/// per-handler GLOBAL_METRICS bookkeeping; business-dimension counters
/// (storage operations, upload/download bytes) remain in the handlers.
///
/// Compared to the handler-side recording it replaces, this now also counts
/// Governor 429 rejections, router 404s, and payload-limit 413s, and the
/// latency for streaming responses includes the time until the response
/// future resolves.
pub async fn metrics_middleware(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    GLOBAL_METRICS.request_started();
    let _guard = InFlightGuard;

    let record = !UNMETRICED_PATHS.contains(&req.path());
    let start = std::time::Instant::now();

    let res = match next.call(req).await {
        Ok(res) => res,
        Err(err) => {
            if record {
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
            }
            return Err(err);
        }
    };

    if record {
        let status = res.status().as_u16();
        GLOBAL_METRICS.record_request(status);
        if status >= 500 {
            GLOBAL_METRICS.record_error();
        }
        GLOBAL_METRICS.record_latency(start);
    }
    Ok(res)
}
