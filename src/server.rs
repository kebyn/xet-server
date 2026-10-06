//! HTTP server implementation

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{
    App, HttpResponse, HttpServer,
    middleware::{Logger, from_fn},
    web,
};
use parking_lot::RwLock;
use std::{sync::Arc, time::Duration};

use crate::api::auth::AuthVerifier;
use crate::api::guard::{AuthNeed, require_auth};
use crate::config::ServerConfig;
use crate::conversion::ConvertingOids;
use crate::middleware::metrics_middleware;
use crate::storage::{StorageBackend, create_storage};

const NANOS_PER_MINUTE: u64 = 60_000_000_000;

fn rate_limit_period(rpm: u32) -> Option<Duration> {
    if rpm == 0 {
        return None;
    }

    let period_nanos = NANOS_PER_MINUTE.div_ceil(u64::from(rpm));
    Some(Duration::from_nanos(period_nanos))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexReadiness {
    Rebuilding,
    Ready { shard_count: usize },
    Failed { message: String },
}

impl IndexReadiness {
    fn check_label(&self) -> &'static str {
        match self {
            Self::Rebuilding => "rebuilding",
            Self::Ready { .. } => "ok",
            Self::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadinessState {
    index: Arc<RwLock<IndexReadiness>>,
}

impl ReadinessState {
    pub fn new() -> Self {
        Self {
            index: Arc::new(RwLock::new(IndexReadiness::Rebuilding)),
        }
    }

    pub fn set_index_ready(&self, shard_count: usize) {
        *self.index.write() = IndexReadiness::Ready { shard_count };
    }

    pub fn set_index_failed(&self, message: impl Into<String>) {
        *self.index.write() = IndexReadiness::Failed {
            message: message.into(),
        };
    }

    pub fn index_state(&self) -> IndexReadiness {
        self.index.read().clone()
    }
}

impl Default for ReadinessState {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn start_server(config: ServerConfig) -> std::io::Result<()> {
    // Load auth keys once at startup (avoid per-request file I/O)
    let auth_verifier =
        Arc::new(AuthVerifier::from_config(&config.auth).map_err(|e| {
            std::io::Error::other(format!("Failed to load auth public key: {}", e))
        })?);

    // Check every active public key file once. In legacy mode several kids may
    // intentionally share one path.
    let mut checked_public_key_paths = std::collections::HashSet::new();
    for (_, path) in config
        .auth
        .verification_key_paths()
        .map_err(std::io::Error::other)?
    {
        if checked_public_key_paths.insert(path) {
            if let Some(warning) = crate::config::check_public_key_permissions(path) {
                tracing::warn!("{}", warning);
            }
        }
    }

    // Validate storage backend
    match config.storage.backend.as_str() {
        "local" | "s3" => {}
        invalid => {
            return Err(std::io::Error::other(format!(
                "Invalid XET_STORAGE_BACKEND '{}'. Must be 'local' or 's3'.",
                invalid
            )));
        }
    }

    let storage: Arc<Box<dyn StorageBackend>> =
        Arc::new(create_storage(&config.storage).await.map_err(|e| {
            std::io::Error::other(format!("Failed to create storage backend: {}", e))
        })?);

    // Pre-create the temp directories once at startup. Upload handlers check
    // disk space via statvfs before writing, and statvfs requires the path to
    // exist — without this, the very first upload on a fresh deployment would
    // fail with 507 because the temp dir is otherwise only created lazily by
    // TempFile::create (after the check).
    for temp_dir in [
        config.storage.resolve_upload_temp_dir(),
        config.storage.resolve_reconstruction_temp_dir(),
    ] {
        tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| {
            std::io::Error::other(format!(
                "Failed to create temp dir {}: {}",
                temp_dir.display(),
                e
            ))
        })?;
    }

    let index = Arc::new(crate::index::MetadataIndex::new());
    let readiness = ReadinessState::new();

    // Rebuild MetadataIndex from stored shards (stateless server)
    // I1 fix: Pass Arc clone for parallel shard fetching
    let rebuild_temp_dir = config.storage.resolve_reconstruction_temp_dir();
    match index
        .rebuild_from_storage(storage.clone(), rebuild_temp_dir)
        .await
    {
        Ok(count) => {
            readiness.set_index_ready(count);
            tracing::info!("Rebuilt metadata index: {} shards loaded", count);
        }
        Err(e) => {
            readiness.set_index_failed(e.clone());
            if config.server.index_rebuild_strict {
                return Err(std::io::Error::other(format!(
                    "Failed to rebuild index and XET_INDEX_REBUILD_STRICT=true: {}",
                    e
                )));
            }
            tracing::warn!("Failed to rebuild index: {}", e);
        }
    }

    // Concurrent conversion tracker (in-memory, resets on restart)
    let converting = Arc::new(ConvertingOids::new());

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);

    tracing::info!("Starting Xet Storage server on {}", bind_addr);

    // Warn if CAS is bound to localhost only — common gotcha for distributed deployments
    if config.server.host == "127.0.0.1" || config.server.host == "localhost" {
        tracing::warn!(
            "CAS server bound to {} only. Remote clients (including Hub on another host) cannot connect. \
            Set XET_HOST=0.0.0.0 for remote access.",
            config.server.host
        );
    }

    tracing::info!("Storage backend: {}", config.storage.backend);
    tracing::info!("Max upload size: {} MB", config.server.max_body_size_mb);
    tracing::info!(
        "Conversion: {}",
        if config.conversion.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );

    // Configure rate limiting for public endpoints only.
    // Internal endpoints (/internal/*) bypass rate limiting to avoid
    // disrupting Hub-to-CAS communication.
    //
    // Governor replenishes one token per configured period, so the period must be
    // 60 seconds divided by RPM. The burst capacity remains RPM.
    //
    // The default PeerIpKeyExtractor uses the direct TCP peer address and deliberately
    // ignores Forwarded/X-Forwarded-For headers. Behind a reverse proxy, all clients
    // therefore share the proxy's bucket. Enforce per-client limits at the trusted proxy
    // or implement a key extractor that validates a trusted-proxy allowlist.
    let rpm = config.server.rate_limit_rpm;
    let period = rate_limit_period(rpm)
        .ok_or_else(|| std::io::Error::other("Rate limit RPM must be greater than zero"))?;
    let governor_conf = GovernorConfigBuilder::default()
        .period(period)
        .burst_size(rpm)
        .finish()
        .ok_or_else(|| std::io::Error::other("Failed to configure rate limiter"))?;

    tracing::info!(
        "Rate limiting: {} sustained requests/minute per peer IP for public endpoints \
         (internal endpoints excluded). Burst: {}, token period: {:.6}s",
        rpm,
        rpm,
        period.as_secs_f64()
    );

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .wrap(from_fn(metrics_middleware))
            // I3 fix: PayloadConfig bounds non-upload routes (web::Bytes, web::Json).
            // Upload handlers use web::Payload which bypasses this limit and
            // enforce max_body_size_bytes manually via streaming byte counting.
            //
            // The 10MB limit applies to JSON/Bytes payloads (commit API, batch API, etc.)
            // and is intentionally separate from max_body_size_mb which controls file uploads.
            // Most JSON payloads are well under 10MB; increase if needed for large commits.
            .app_data(web::PayloadConfig::new(10 * 1024 * 1024))
            .app_data(web::Data::from(auth_verifier.clone()))
            .app_data(web::Data::from(storage.clone()))
            .app_data(web::Data::from(index.clone()))
            .app_data(web::Data::new(converting.clone()))
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::new(config.conversion.clone()))
            .app_data(web::Data::new(readiness.clone()))
            // =============================================================
            // Internal endpoints (Hub-to-CAS communication) - NO rate limiting
            // These are registered at App level, BEFORE the public scope,
            // so they match first and bypass the Governor middleware.
            // =============================================================
            .route(
                "/internal/state/{oid}",
                web::get().to(crate::api::internal::get_blob_state),
            )
            .route(
                "/internal/blob/{oid}",
                web::head().to(crate::api::internal::head_blob),
            )
            // Health and metrics endpoints - no rate limiting
            .route("/health", web::get().to(health_check))
            .route("/ready", web::get().to(readiness_check))
            .route("/metrics", web::get().to(metrics_endpoint))
            // =============================================================
            // Public API routes - rate limited via Governor middleware.
            // The scope wraps all public routes with rate limiting.
            // =============================================================
            .service(
                web::scope("")
                    .wrap(Governor::new(&governor_conf))
                    .route(
                        "/v1/xorbs/{prefix}/{hash}",
                        web::post().to(crate::api::xorb::upload_xorb),
                    )
                    .route(
                        "/v1/xorbs/{prefix}/{hash}",
                        web::put().to(crate::api::xorb::upload_xorb),
                    )
                    .route(
                        "/v1/xorbs/{prefix}/{hash}/download",
                        web::get().to(crate::api::xorb::download_xorb),
                    )
                    .route(
                        "/lfs/objects/{oid}",
                        web::put().to(crate::api::lfs::upload_lfs_object),
                    )
                    .route(
                        "/lfs/objects/{oid}",
                        web::get().to(crate::api::lfs::download_lfs_object),
                    )
                    .route(
                        "/v1/shards",
                        web::post().to(crate::api::shard::upload_shard),
                    )
                    .route(
                        "/v1/reconstructions/{file_id}",
                        web::get().to(crate::api::reconstruction::get_reconstruction_v1),
                    )
                    .route(
                        "/v2/reconstructions/{file_id}",
                        web::get().to(crate::api::reconstruction::get_reconstruction),
                    )
                    .route(
                        "/v1/chunks/{prefix}/{hash}",
                        web::get().to(crate::api::global_dedup::query_chunk_dedup),
                    )
                    .route(
                        "/objects/batch",
                        web::post().to(crate::api::batch::batch_operation),
                    )
                    .route(
                        "/lfs/objects/batch",
                        web::post().to(crate::api::batch::batch_operation),
                    ),
            )
    })
    .bind(&bind_addr)?
    .run()
    .await
}

pub async fn health_check() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok"
    }))
}

pub async fn readiness_check(
    storage: web::Data<Box<dyn StorageBackend>>,
    readiness: web::Data<ReadinessState>,
) -> HttpResponse {
    let storage_status = match storage.health_check().await {
        Ok(()) => "ok",
        Err(e) => {
            tracing::warn!("Storage readiness check failed: {}", e);
            "failed"
        }
    };
    let index_state = readiness.index_state();
    let index_status = index_state.check_label();
    let ready = storage_status == "ok" && matches!(index_state, IndexReadiness::Ready { .. });

    let mut body = serde_json::json!({
        "status": if ready { "ready" } else { "not_ready" },
        "checks": {
            "storage": storage_status,
            "index": index_status,
        }
    });

    if let IndexReadiness::Ready { shard_count } = index_state {
        body["index_shard_count"] = serde_json::json!(shard_count);
    }

    if ready {
        HttpResponse::Ok().json(body)
    } else {
        HttpResponse::ServiceUnavailable().json(body)
    }
}

/// Prometheus metrics endpoint
///
/// # Security Note
/// This endpoint requires authentication (internal scope).
/// In production environments, use tokens with "internal" scope for monitoring systems.
pub async fn metrics_endpoint(
    auth: web::Data<AuthVerifier>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();

    // Extract, verify, and authorize the caller in one step.
    // Metrics are internal-only and require the full internal service token shape.
    if let Err(rej) = require_auth(
        &req,
        &auth,
        AuthNeed::Internal("Insufficient scope: requires 'internal'"),
    ) {
        return rej.respond(start);
    }

    let metrics = crate::metrics::GLOBAL_METRICS.export_metrics();
    crate::metrics::GLOBAL_METRICS.record_request(200);
    crate::metrics::GLOBAL_METRICS.record_latency(start);
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(metrics)
}

#[cfg(test)]
mod tests {
    use super::rate_limit_period;
    use std::time::Duration;

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
}
