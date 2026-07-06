use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{App, HttpResponse, HttpServer, middleware::Logger, web};
use std::sync::Arc;

use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;
use crate::metadata::MetadataStore;
use crate::metadata::sqlite::SqliteMetadataStore;
use crate::sqlite_pool::connect_hub_sqlite_pool;

pub async fn start_server(config: HubConfig) -> std::io::Result<()> {
    // M2 fix: Create one shared SQLite pool for both TokenStore and MetadataStore.
    // SQLite only supports one writer at a time; sharing the configured pool keeps
    // total DB connections bounded across both stores.
    let shared_pool =
        connect_hub_sqlite_pool(&config.metadata.sqlite_path, config.metadata.db_pool_size)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to connect to database: {}", e)))?;

    let shared_pool_for_shutdown = shared_pool.clone();
    let shared_pool_for_ready = shared_pool.clone();

    tracing::info!(
        "Using shared SQLite connection pool ({} connections) for TokenStore + MetadataStore",
        config.metadata.db_pool_size
    );

    // Initialize token store with shared pool
    let token_store = Arc::new(
        TokenStore::with_pool(shared_pool.clone())
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to create token store: {}", e)))?,
    );

    // Initialize metadata store with shared pool
    let metadata: Arc<dyn MetadataStore> = Arc::new(
        SqliteMetadataStore::with_pool(shared_pool)
            .await
            .map_err(|e| {
                std::io::Error::other(format!("Failed to create metadata store: {}", e))
            })?,
    );

    // Initialize xet signer
    // M9 fix: Check private key file permissions (private keys are more sensitive than public keys).
    // If the private key is world-readable, other users could forge authentication tokens.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&config.auth.private_key_path) {
            let mode = meta.permissions().mode();
            if mode & 0o044 != 0 {
                tracing::warn!(
                    "SECURITY: Private key file '{}' is readable by group/other (mode {:o}). \
                     An attacker with read access could forge authentication tokens. \
                     Use chmod 600 to restrict access.",
                    config.auth.private_key_path,
                    mode
                );
            }
        }
    }
    let private_key_pem = std::fs::read(&config.auth.private_key_path).map_err(|e| {
        std::io::Error::other(format!(
            "Failed to read private key from '{}': {}",
            config.auth.private_key_path, e
        ))
    })?;
    let signer = Arc::new(
        XetSigner::from_pem_with_internal_ttl(
            &private_key_pem,
            &config.auth.kid,
            config.auth.token_ttl_seconds,
            config.auth.proxy_token_ttl_seconds,
            config.auth.internal_token_ttl_seconds, // C1 fix: configurable internal token TTL
        )
        .map_err(|e| std::io::Error::other(format!("Failed to create xet signer: {}", e)))?,
    );

    // Initialize CAS client
    let cas_client = Arc::new(
        CasClient::new(&config.cas)
            .map_err(|e| std::io::Error::other(format!("Failed to create CAS client: {}", e)))?,
    );

    // Optional: verify CAS connectivity at startup (async, non-blocking)
    // M5 fix: Add timeout to prevent health check from hanging indefinitely
    // M-2 fix: Make timeout configurable via HUB_CAS_HEALTH_CHECK_TIMEOUT_SECS
    let cas_health = cas_client.clone();
    let health_check_timeout = config.cas.health_check_timeout_seconds;
    tokio::spawn(async move {
        match tokio::time::timeout(
            std::time::Duration::from_secs(health_check_timeout),
            cas_health.health_check(),
        )
        .await
        {
            Ok(Ok(true)) => tracing::info!("CAS health check passed"),
            Ok(Ok(false)) => {
                tracing::warn!("CAS health check returned non-success. Verify CAS is running.")
            }
            Ok(Err(e)) => tracing::warn!(
                "CAS health check failed: {}. Hub and CAS may not be able to communicate.",
                e
            ),
            Err(_) => tracing::error!(
                "CAS health check timed out after {} seconds",
                health_check_timeout
            ),
        }
    });

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    tracing::info!("Starting Hub API on {}", bind_addr);
    tracing::info!("CAS: {}", config.cas.base_url);

    if config.server.host == "0.0.0.0" {
        tracing::warn!(
            "Hub is binding to 0.0.0.0 (all interfaces) on port {}. Ensure it sits behind a trusted proxy/firewall; authentication is always enforced.",
            config.server.port
        );
    }
    tracing::info!("Authentication: enforced — all public endpoints require a valid bearer token");

    // Warn about relative paths that depend on process CWD
    if !std::path::Path::new(&config.auth.private_key_path).is_absolute() {
        tracing::warn!(
            "HUB_PRIVATE_KEY_PATH '{}' is a relative path. Resolved to: '{}'. \
            Consider using an absolute path for production deployments.",
            config.auth.private_key_path,
            std::fs::canonicalize(&config.auth.private_key_path)
                .unwrap_or_else(|_| std::path::PathBuf::from("(not found)"))
                .display()
        );
    }
    if !std::path::Path::new(&config.metadata.sqlite_path).is_absolute() {
        tracing::warn!(
            "HUB_SQLITE_PATH '{}' is a relative path. \
            Consider using an absolute path (e.g., /var/lib/xet/hub.db) for production deployments.",
            config.metadata.sqlite_path
        );
    }

    // I5 fix: Configure rate limiting for public API endpoints.
    // Internal endpoints (/internal/*) and health check bypass rate limiting.
    // Uses default PeerIpKeyExtractor for per-IP rate limiting (not global).
    //
    // Governor's rate limiter uses a token bucket algorithm:
    // - per_second(60): Token refill window is 60 seconds
    // - burst_size(rpm): Maximum tokens (requests) allowed per window
    //
    // Example with default rpm=120:
    // - A client can make up to 120 requests in any 60-second window
    // - Tokens refill at 2 per second (120 tokens / 60 seconds)
    // - Burst allows 120 rapid requests, then must wait for refill
    //
    // This is effectively "requests per minute" with burst tolerance.
    let rpm = config.server.rate_limit_rpm;
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(60) // 60-second refill window
        .burst_size(rpm) // configured requests per window
        .finish()
        .ok_or_else(|| std::io::Error::other("Failed to configure rate limiter"))?;

    tracing::info!(
        "Rate limiting: {} requests per 60-second window per IP for public endpoints \
         (internal/health excluded). Burst: {}, refill: {} tokens/second",
        rpm,
        rpm,
        rpm
    );

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            // Payload size limit: 50MB default for JSON API endpoints.
            // Commit API inline files max ~13.6MB (10MB base64-encoded), 50MB is sufficient.
            // Large LFS files use streaming upload via Git LFS protocol.
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024)) // 50MB
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::new(token_store.clone()))
            .app_data(web::Data::new(metadata.clone()))
            .app_data(web::Data::new(signer.clone()))
            .app_data(web::Data::new(cas_client.clone()))
            .app_data(web::Data::new(shared_pool_for_ready.clone()))
            // =============================================================
            // Non-rate-limited endpoints (registered at App level, before scope)
            // =============================================================
            // Health endpoint is non-rate-limited for monitoring purposes.
            .route(
                "/health",
                web::get()
                    .to(|| async { HttpResponse::Ok().json(serde_json::json!({"status": "ok"})) }),
            )
            .route("/ready", web::get().to(readiness_check))
            // =============================================================
            // Public API routes - rate limited via Governor middleware
            // =============================================================
            .service(
                web::scope("")
                    .wrap(Governor::new(&governor_conf))
                    // Auth
                    .route("/api/whoami-v2", web::get().to(crate::api::whoami::whoami))
                    // Token exchange — explicit routes for each repo type
                    .route(
                        "/api/models/{ns}/{repo}/xet-read-token/{rev}",
                        web::get().to(crate::api::token_exchange::exchange_model_read),
                    )
                    .route(
                        "/api/models/{ns}/{repo}/xet-write-token/{rev}",
                        web::get().to(crate::api::token_exchange::exchange_model_write),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}/xet-read-token/{rev}",
                        web::get().to(crate::api::token_exchange::exchange_dataset_read),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}/xet-write-token/{rev}",
                        web::get().to(crate::api::token_exchange::exchange_dataset_write),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}/xet-read-token/{rev}",
                        web::get().to(crate::api::token_exchange::exchange_space_read),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}/xet-write-token/{rev}",
                        web::get().to(crate::api::token_exchange::exchange_space_write),
                    )
                    // Repo CRUD
                    .route(
                        "/api/repos/create",
                        web::post().to(crate::api::repo::create_repo_unified),
                    )
                    .route(
                        "/api/models",
                        web::post().to(crate::api::repo::create_model),
                    )
                    .route(
                        "/api/datasets",
                        web::post().to(crate::api::repo::create_dataset),
                    )
                    .route(
                        "/api/spaces",
                        web::post().to(crate::api::repo::create_space),
                    )
                    .route(
                        "/api/models/{ns}/{repo}",
                        web::get().to(crate::api::repo::get_repo_model),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}",
                        web::get().to(crate::api::repo::get_repo_dataset),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}",
                        web::get().to(crate::api::repo::get_repo_space),
                    )
                    .route(
                        "/api/models/{ns}/{repo}",
                        web::delete().to(crate::api::repo::delete_repo_model),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}",
                        web::delete().to(crate::api::repo::delete_repo_dataset),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}",
                        web::delete().to(crate::api::repo::delete_repo_space),
                    )
                    // Revision info (used by hf upload)
                    .route(
                        "/api/models/{ns}/{repo}/revision/{rev}",
                        web::get().to(crate::api::repo::get_revision_model),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}/revision/{rev}",
                        web::get().to(crate::api::repo::get_revision_dataset),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}/revision/{rev}",
                        web::get().to(crate::api::repo::get_revision_space),
                    )
                    // Commit
                    .route(
                        "/api/models/{ns}/{repo}/commit/{rev}",
                        web::post().to(crate::api::commit::commit_model),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}/commit/{rev}",
                        web::post().to(crate::api::commit::commit_dataset),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}/commit/{rev}",
                        web::post().to(crate::api::commit::commit_space),
                    )
                    // Preupload
                    .route(
                        "/api/models/{ns}/{repo}/preupload/{rev}",
                        web::post().to(crate::api::preupload::preupload_model),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}/preupload/{rev}",
                        web::post().to(crate::api::preupload::preupload_dataset),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}/preupload/{rev}",
                        web::post().to(crate::api::preupload::preupload_space),
                    )
                    // Tree
                    .route(
                        "/api/models/{ns}/{repo}/tree/{rev}/{path:.*}",
                        web::get().to(crate::api::tree::tree_model),
                    )
                    .route(
                        "/api/models/{ns}/{repo}/tree/{rev}",
                        web::get().to(crate::api::tree::tree_model_no_path),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}/tree/{rev}/{path:.*}",
                        web::get().to(crate::api::tree::tree_dataset),
                    )
                    .route(
                        "/api/datasets/{ns}/{repo}/tree/{rev}",
                        web::get().to(crate::api::tree::tree_dataset_no_path),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}/tree/{rev}/{path:.*}",
                        web::get().to(crate::api::tree::tree_space),
                    )
                    .route(
                        "/api/spaces/{ns}/{repo}/tree/{rev}",
                        web::get().to(crate::api::tree::tree_space_no_path),
                    )
                    // File download/resolve - Type-prefixed routes MUST come before generic routes
                    .route(
                        "/models/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::get().to(crate::api::resolve::resolve_model),
                    )
                    .route(
                        "/models/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::head().to(crate::api::resolve::resolve_model),
                    )
                    .route(
                        "/datasets/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::get().to(crate::api::resolve::resolve_dataset),
                    )
                    .route(
                        "/datasets/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::head().to(crate::api::resolve::resolve_dataset),
                    )
                    .route(
                        "/spaces/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::get().to(crate::api::resolve::resolve_space),
                    )
                    .route(
                        "/spaces/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::head().to(crate::api::resolve::resolve_space),
                    )
                    // Generic fallback (matches /{ns}/{repo}/resolve/...)
                    .route(
                        "/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::get().to(crate::api::resolve::resolve_model),
                    )
                    .route(
                        "/{ns}/{repo}/resolve/{rev}/{path:.*}",
                        web::head().to(crate::api::resolve::resolve_model),
                    )
                    // Git LFS proxy
                    .route(
                        "/objects/batch",
                        web::post().to(crate::api::lfs_proxy::lfs_batch),
                    )
                    .route(
                        "/lfs/objects/batch",
                        web::post().to(crate::api::lfs_proxy::lfs_batch),
                    )
                    .route(
                        "/lfs/objects/{oid}",
                        web::put().to(crate::api::lfs_proxy::lfs_upload),
                    )
                    .route(
                        "/lfs/objects/{oid}",
                        web::get().to(crate::api::lfs_proxy::lfs_download),
                    )
                    // Git-style LFS endpoints - Type-prefixed routes first
                    .route(
                        "/models/{ns}/{repo}.git/info/lfs/objects/batch",
                        web::post().to(crate::api::lfs_proxy::lfs_batch),
                    )
                    .route(
                        "/datasets/{ns}/{repo}.git/info/lfs/objects/batch",
                        web::post().to(crate::api::lfs_proxy::lfs_batch),
                    )
                    .route(
                        "/spaces/{ns}/{repo}.git/info/lfs/objects/batch",
                        web::post().to(crate::api::lfs_proxy::lfs_batch),
                    )
                    .route(
                        "/models/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::put().to(crate::api::lfs_proxy::lfs_upload),
                    )
                    .route(
                        "/datasets/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::put().to(crate::api::lfs_proxy::lfs_upload),
                    )
                    .route(
                        "/spaces/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::put().to(crate::api::lfs_proxy::lfs_upload),
                    )
                    .route(
                        "/models/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::get().to(crate::api::lfs_proxy::lfs_download),
                    )
                    .route(
                        "/datasets/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::get().to(crate::api::lfs_proxy::lfs_download),
                    )
                    .route(
                        "/spaces/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::get().to(crate::api::lfs_proxy::lfs_download),
                    )
                    // Generic fallback
                    .route(
                        "/{ns}/{repo}.git/info/lfs/objects/batch",
                        web::post().to(crate::api::lfs_proxy::lfs_batch),
                    )
                    .route(
                        "/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::put().to(crate::api::lfs_proxy::lfs_upload),
                    )
                    .route(
                        "/{ns}/{repo}.git/info/lfs/objects/{oid}",
                        web::get().to(crate::api::lfs_proxy::lfs_download),
                    ),
            )
    })
    .bind(&bind_addr)?
    // M3 fix: Request timeout prevents slow clients from holding connections indefinitely
    .client_request_timeout(std::time::Duration::from_secs(300))
    .client_disconnect_timeout(std::time::Duration::from_secs(5))
    .run()
    .await?;

    // I3 fix: Gracefully close connection pool on shutdown to flush pending transactions
    shared_pool_for_shutdown.close().await;
    tracing::info!("Database connection pool closed");

    Ok(())
}

pub async fn readiness_check(
    pool: web::Data<sqlx::sqlite::SqlitePool>,
    cas_client: web::Data<Arc<CasClient>>,
) -> HttpResponse {
    let database_ok = match sqlx::query("SELECT 1").execute(pool.get_ref()).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Hub database readiness check failed: {}", e);
            false
        }
    };

    let cas_ok = match cas_client.readiness_check().await {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!("Hub CAS readiness check failed: {}", e);
            false
        }
    };

    let ready = database_ok && cas_ok;
    let body = serde_json::json!({
        "status": if ready { "ready" } else { "not_ready" },
        "checks": {
            "database": if database_ok { "ok" } else { "failed" },
            "cas": if cas_ok { "ok" } else { "failed" },
        }
    });

    if ready {
        HttpResponse::Ok().json(body)
    } else {
        HttpResponse::ServiceUnavailable().json(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpResponse, HttpServer, test, web};
    use sqlx::sqlite::SqlitePoolOptions;
    use std::net::TcpListener;

    async fn start_mock_cas_ready(status: actix_web::http::StatusCode) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = HttpServer::new(move || {
            App::new().route(
                "/ready",
                web::get().to(move || async move {
                    HttpResponse::build(status)
                        .json(serde_json::json!({"status": if status.is_success() { "ready" } else { "not_ready" }}))
                }),
            )
        })
        .listen(listener)
        .unwrap()
        .run();
        tokio::spawn(server);
        format!("http://{}", addr)
    }

    #[actix_web::test]
    async fn readiness_check_returns_ready_when_database_and_cas_are_ready() {
        let cas_base_url = start_mock_cas_ready(actix_web::http::StatusCode::OK).await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let cas = Arc::new(
            CasClient::new(&crate::config::CasSettings {
                base_url: cas_base_url,
                internal_timeout_seconds: 5,
                max_download_size: 1024,
                health_check_timeout_seconds: 5,
            })
            .unwrap(),
        );

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(cas))
                .route("/ready", web::get().to(readiness_check)),
        )
        .await;

        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/ready").to_request()).await;
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["status"], "ready");
        assert_eq!(body["checks"]["database"], "ok");
        assert_eq!(body["checks"]["cas"], "ok");
    }

    #[actix_web::test]
    async fn readiness_check_returns_unavailable_when_cas_is_not_ready() {
        let cas_base_url =
            start_mock_cas_ready(actix_web::http::StatusCode::SERVICE_UNAVAILABLE).await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let cas = Arc::new(
            CasClient::new(&crate::config::CasSettings {
                base_url: cas_base_url,
                internal_timeout_seconds: 5,
                max_download_size: 1024,
                health_check_timeout_seconds: 5,
            })
            .unwrap(),
        );

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(cas))
                .route("/ready", web::get().to(readiness_check)),
        )
        .await;

        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/ready").to_request()).await;
        assert_eq!(resp.status(), 503);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["checks"]["database"], "ok");
        assert_eq!(body["checks"]["cas"], "failed");
    }
}
