use actix_web::{web, App, HttpServer, HttpResponse, middleware::Logger};
use std::sync::Arc;

use crate::auth::token_store::TokenStore;
use crate::auth::xet_signer::XetSigner;
use crate::cas_client::CasClient;
use crate::config::HubConfig;
use crate::metadata::sqlite::SqliteMetadataStore;
use crate::metadata::MetadataStore;

pub async fn start_server(config: HubConfig) -> std::io::Result<()> {
    // Initialize token store (uses same DB as metadata for simplicity)
    let token_store = Arc::new(
        TokenStore::new(&config.metadata.sqlite_path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to create token store: {}", e)))?
    );

    // Initialize metadata store
    let metadata: Arc<dyn MetadataStore> = Arc::new(
        SqliteMetadataStore::new(&config.metadata.sqlite_path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to create metadata store: {}", e)))?
    );

    // Initialize xet signer
    let private_key_pem = std::fs::read(&config.auth.private_key_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to read private key from '{}': {}", config.auth.private_key_path, e)))?;
    let signer = Arc::new(
        XetSigner::from_pem(&private_key_pem, &config.auth.kid, config.auth.token_ttl_seconds)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to create xet signer: {}", e)))?
    );

    // Initialize CAS client
    let cas_client = Arc::new(CasClient::new(&config.cas));

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    println!("Starting Hub API on {}", bind_addr);
    println!("CAS: {}", config.cas.base_url);

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            // Payload size limit: 50MB default for JSON API endpoints.
            // Commit API inline files max ~13.6MB (10MB base64-encoded), 50MB is sufficient.
            // Large LFS files use streaming upload via Git LFS protocol.
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024)) // 50MB
            .app_data(web::Data::new(config.clone()))
            .app_data(web::Data::from(token_store.clone()))
            .app_data(web::Data::from(metadata.clone()))
            .app_data(web::Data::from(signer.clone()))
            .app_data(web::Data::from(cas_client.clone()))
            // Auth
            .route("/api/whoami-v2", web::get().to(crate::api::whoami::whoami))
            // Token exchange — explicit routes for each repo type
            .route("/api/models/{ns}/{repo}/xet-read-token/{rev}", web::get().to(crate::api::token_exchange::exchange_model_read))
            .route("/api/models/{ns}/{repo}/xet-write-token/{rev}", web::get().to(crate::api::token_exchange::exchange_model_write))
            .route("/api/datasets/{ns}/{repo}/xet-read-token/{rev}", web::get().to(crate::api::token_exchange::exchange_dataset_read))
            .route("/api/datasets/{ns}/{repo}/xet-write-token/{rev}", web::get().to(crate::api::token_exchange::exchange_dataset_write))
            .route("/api/spaces/{ns}/{repo}/xet-read-token/{rev}", web::get().to(crate::api::token_exchange::exchange_space_read))
            .route("/api/spaces/{ns}/{repo}/xet-write-token/{rev}", web::get().to(crate::api::token_exchange::exchange_space_write))
            // Repo CRUD
            .route("/api/repos/create", web::post().to(crate::api::repo::create_repo_unified))
            .route("/api/models", web::post().to(crate::api::repo::create_model))
            .route("/api/datasets", web::post().to(crate::api::repo::create_dataset))
            .route("/api/spaces", web::post().to(crate::api::repo::create_space))
            .route("/api/models/{ns}/{repo}", web::get().to(crate::api::repo::get_repo_model))
            .route("/api/datasets/{ns}/{repo}", web::get().to(crate::api::repo::get_repo_dataset))
            .route("/api/spaces/{ns}/{repo}", web::get().to(crate::api::repo::get_repo_space))
            .route("/api/models/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo_model))
            .route("/api/datasets/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo_dataset))
            .route("/api/spaces/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo_space))
            // Revision info (used by hf upload)
            .route("/api/models/{ns}/{repo}/revision/{rev}", web::get().to(crate::api::repo::get_revision_model))
            .route("/api/datasets/{ns}/{repo}/revision/{rev}", web::get().to(crate::api::repo::get_revision_dataset))
            .route("/api/spaces/{ns}/{repo}/revision/{rev}", web::get().to(crate::api::repo::get_revision_space))
            // Commit
            .route("/api/models/{ns}/{repo}/commit/{rev}", web::post().to(crate::api::commit::commit_model))
            .route("/api/datasets/{ns}/{repo}/commit/{rev}", web::post().to(crate::api::commit::commit_dataset))
            .route("/api/spaces/{ns}/{repo}/commit/{rev}", web::post().to(crate::api::commit::commit_space))
            // Preupload
            .route("/api/models/{ns}/{repo}/preupload/{rev}", web::post().to(crate::api::preupload::preupload_model))
            .route("/api/datasets/{ns}/{repo}/preupload/{rev}", web::post().to(crate::api::preupload::preupload_dataset))
            .route("/api/spaces/{ns}/{repo}/preupload/{rev}", web::post().to(crate::api::preupload::preupload_space))
            // Tree
            .route("/api/models/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree_model))
            .route("/api/models/{ns}/{repo}/tree/{rev}", web::get().to(crate::api::tree::tree_model_no_path))
            .route("/api/datasets/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree_dataset))
            .route("/api/datasets/{ns}/{repo}/tree/{rev}", web::get().to(crate::api::tree::tree_dataset_no_path))
            .route("/api/spaces/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree_space))
            .route("/api/spaces/{ns}/{repo}/tree/{rev}", web::get().to(crate::api::tree::tree_space_no_path))
            // File download/resolve - Type-prefixed routes MUST come before generic routes
            .route("/models/{ns}/{repo}/resolve/{rev}/{path:.*}", web::get().to(crate::api::resolve::resolve_model))
            .route("/models/{ns}/{repo}/resolve/{rev}/{path:.*}", web::head().to(crate::api::resolve::resolve_model))
            .route("/datasets/{ns}/{repo}/resolve/{rev}/{path:.*}", web::get().to(crate::api::resolve::resolve_dataset))
            .route("/datasets/{ns}/{repo}/resolve/{rev}/{path:.*}", web::head().to(crate::api::resolve::resolve_dataset))
            .route("/spaces/{ns}/{repo}/resolve/{rev}/{path:.*}", web::get().to(crate::api::resolve::resolve_space))
            .route("/spaces/{ns}/{repo}/resolve/{rev}/{path:.*}", web::head().to(crate::api::resolve::resolve_space))
            // Generic fallback (matches /{ns}/{repo}/resolve/...)
            .route("/{ns}/{repo}/resolve/{rev}/{path:.*}", web::get().to(crate::api::resolve::resolve_model))
            .route("/{ns}/{repo}/resolve/{rev}/{path:.*}", web::head().to(crate::api::resolve::resolve_model))
            // Git LFS proxy
            .route("/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/lfs/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/lfs/objects/{oid}", web::put().to(crate::api::lfs_proxy::lfs_upload))
            .route("/lfs/objects/{oid}", web::get().to(crate::api::lfs_proxy::lfs_download))
            // Git-style LFS endpoints - Type-prefixed routes first
            .route("/models/{ns}/{repo}.git/info/lfs/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/datasets/{ns}/{repo}.git/info/lfs/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/spaces/{ns}/{repo}.git/info/lfs/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/models/{ns}/{repo}.git/info/lfs/objects/{oid}", web::put().to(crate::api::lfs_proxy::lfs_upload))
            .route("/datasets/{ns}/{repo}.git/info/lfs/objects/{oid}", web::put().to(crate::api::lfs_proxy::lfs_upload))
            .route("/spaces/{ns}/{repo}.git/info/lfs/objects/{oid}", web::put().to(crate::api::lfs_proxy::lfs_upload))
            .route("/models/{ns}/{repo}.git/info/lfs/objects/{oid}", web::get().to(crate::api::lfs_proxy::lfs_download))
            .route("/datasets/{ns}/{repo}.git/info/lfs/objects/{oid}", web::get().to(crate::api::lfs_proxy::lfs_download))
            .route("/spaces/{ns}/{repo}.git/info/lfs/objects/{oid}", web::get().to(crate::api::lfs_proxy::lfs_download))
            // Generic fallback
            .route("/{ns}/{repo}.git/info/lfs/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/{ns}/{repo}.git/info/lfs/objects/{oid}", web::put().to(crate::api::lfs_proxy::lfs_upload))
            .route("/{ns}/{repo}.git/info/lfs/objects/{oid}", web::get().to(crate::api::lfs_proxy::lfs_download))
            // Internal API (for CAS GC)
            .route("/internal/referenced-hashes", web::get().to(crate::api::internal::get_referenced_hashes))
            // Health
            .route("/health", web::get().to(|| async { HttpResponse::Ok().json(serde_json::json!({"status": "ok"})) }))
    })
    .bind(&bind_addr)?
    .run()
    .await
}