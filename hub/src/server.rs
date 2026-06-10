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
            .expect("Failed to create token store")
    );

    // Initialize metadata store
    let metadata: Arc<dyn MetadataStore> = Arc::new(
        SqliteMetadataStore::new(&config.metadata.sqlite_path)
            .expect("Failed to create metadata store")
    );

    // Initialize xet signer
    let private_key_pem = std::fs::read(&config.auth.private_key_path)
        .expect("Failed to read private key");
    let signer = Arc::new(
        XetSigner::from_pem(&private_key_pem, &config.auth.kid, config.auth.token_ttl_seconds)
            .expect("Failed to create xet signer")
    );

    // Initialize CAS client
    let cas_client = Arc::new(CasClient::new(&config.cas));

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    println!("Starting Hub API on {}", bind_addr);
    println!("CAS: {}", config.cas.base_url);

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
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
            .route("/api/models", web::post().to(crate::api::repo::create_model))
            .route("/api/datasets", web::post().to(crate::api::repo::create_dataset))
            .route("/api/spaces", web::post().to(crate::api::repo::create_space))
            .route("/api/models/{ns}/{repo}", web::get().to(crate::api::repo::get_repo_model))
            .route("/api/datasets/{ns}/{repo}", web::get().to(crate::api::repo::get_repo_dataset))
            .route("/api/spaces/{ns}/{repo}", web::get().to(crate::api::repo::get_repo_space))
            .route("/api/models/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo_model))
            .route("/api/datasets/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo_dataset))
            .route("/api/spaces/{ns}/{repo}", web::delete().to(crate::api::repo::delete_repo_space))
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
            .route("/api/datasets/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree_dataset))
            .route("/api/spaces/{ns}/{repo}/tree/{rev}/{path:.*}", web::get().to(crate::api::tree::tree_space))
            // File download/resolve
            .route("/{ns}/{repo}/resolve/{rev}/{path:.*}", web::get().to(crate::api::resolve::resolve_model))
            // Git LFS proxy
            .route("/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/lfs/objects/batch", web::post().to(crate::api::lfs_proxy::lfs_batch))
            .route("/lfs/objects/{oid}", web::put().to(crate::api::lfs_proxy::lfs_upload))
            .route("/lfs/objects/{oid}", web::get().to(crate::api::lfs_proxy::lfs_download))
            // Health
            .route("/health", web::get().to(|| async { HttpResponse::Ok().json(serde_json::json!({"status": "ok"})) }))
    })
    .bind(&bind_addr)?
    .run()
    .await
}