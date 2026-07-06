use crate::auth::extract::{AuthRead, AuthUser};
use crate::config::HubConfig;
use crate::metadata::{MetadataStore, RepoType};
use crate::services::resolve::{ResolveFileRequest, ResolveService, ResolveServiceError};
use actix_web::{HttpRequest, HttpResponse, web};
use std::sync::Arc;

fn resolve_service(metadata: &web::Data<Arc<dyn MetadataStore>>) -> ResolveService {
    ResolveService::new(metadata.get_ref().clone())
}

fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}

fn resolve_service_error_response(err: ResolveServiceError) -> HttpResponse {
    match err {
        ResolveServiceError::NotFound(msg) => {
            HttpResponse::NotFound().json(error_json(msg, "NotFoundError"))
        }
        ResolveServiceError::Internal(msg) => {
            HttpResponse::InternalServerError().json(error_json(msg, "InternalError"))
        }
    }
}

/// Internal helper for file resolve/download
async fn handle_resolve(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    repo_type: RepoType,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    let (namespace, repo_name, revision, file_path) = path.into_inner();
    let service = resolve_service(&metadata);
    let resolved = match service
        .resolve_file(ResolveFileRequest {
            username: &auth.info.username,
            namespace: &namespace,
            repo_name: &repo_name,
            repo_type,
            revision: &revision,
            file_path: &file_path,
        })
        .await
    {
        Ok(resolved) => resolved,
        Err(err) => return resolve_service_error_response(err),
    };
    let commit_id = resolved.commit_id;
    let file_entry = resolved.file_entry;

    // I8: Build download URL using Hub's URL (not CAS internal URL)
    // Clients go through Hub, which proxies to CAS
    let hub_base_url = config.server.base_url();

    // Generate a short-lived proxy token for the download
    let xet_signer =
        req.app_data::<web::Data<std::sync::Arc<crate::auth::xet_signer::XetSigner>>>();
    let proxy_token_param = if let Some(signer) = xet_signer {
        // I2 fix: Handle signing errors gracefully - if we can't sign, omit the token
        // I6 fix: Use actual username instead of "anonymous" for audit trail
        match signer.sign_proxy(
            auth.username(),
            &file_entry.cas_hash,
            "download",
            &format!("{}/{}", namespace, repo_name),
            &repo_type.to_string(),
        ) {
            Ok((proxy_token, _)) => {
                // Proxy tokens use base64url encoding (A-Za-z0-9_-) plus '.' separator
                // and 'proxy_' prefix, all of which are URL-safe in query parameters.
                // No percent-encoding needed.
                format!("?token={}", proxy_token)
            }
            Err(e) => {
                // M3 fix: Return 500 instead of silently generating an invalid URL
                tracing::error!("Failed to sign proxy token for resolve: {}", e);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to generate download token",
                    "error_type": "InternalError"
                }));
            }
        }
    } else {
        String::new()
    };
    let download_url = format!(
        "{}/lfs/objects/{}{}",
        hub_base_url, file_entry.cas_hash, proxy_token_param
    );

    // Common HF Hub headers expected by huggingface_hub library
    // (commit_id is already an owned String from resolve_revision)

    // For small files, return content directly (HF Hub compatible)
    // For large files, return 302 redirect to LFS download URL
    if file_entry.size <= config.storage.inline_threshold_bytes {
        // Try to fetch content from CAS
        let xet_signer =
            req.app_data::<web::Data<std::sync::Arc<crate::auth::xet_signer::XetSigner>>>();
        let cas_client = req.app_data::<web::Data<std::sync::Arc<crate::cas_client::CasClient>>>();

        if let (Some(signer), Some(cas)) = (xet_signer, cas_client) {
            // CAS /lfs/objects/{oid} is a public object endpoint. Use a short-lived
            // user read token for inline fetches, not an internal service token.
            match signer.sign(
                auth.username(),
                "read",
                &format!("{}/{}", namespace, repo_name),
                &repo_type.to_string(),
                &revision,
            ) {
                Ok((cas_read_token, _)) => {
                    match cas
                        .proxy_lfs_download(&file_entry.cas_hash, &cas_read_token)
                        .await
                    {
                        Ok(data) => {
                            return HttpResponse::Ok()
                                .content_type("application/octet-stream")
                                .insert_header(("X-Repo-Commit", commit_id.as_str()))
                                .insert_header(("ETag", format!("\"{}\"", file_entry.cas_hash)))
                                .body(data);
                        }
                        Err(crate::error::HubError::NotFound(_)) => {
                            // I8 fix: If CAS explicitly returns 404, propagate it to client
                            // instead of redirecting to a URL that will also 404
                            return HttpResponse::NotFound().json(serde_json::json!({
                                "error": format!("File content not found in storage: {}", file_entry.cas_hash),
                                "error_type": "NotFoundError"
                            }));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "CAS inline fetch failed for {}: {}",
                                file_entry.cas_hash,
                                e
                            );
                            // Fall through to redirect for transient errors (network, timeout)
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to sign CAS read token for inline fetch: {}", e);
                    // Fall through to redirect
                }
            }
        }
    }

    // Large files or CAS fetch failed - redirect to LFS download URL
    HttpResponse::Found()
        .append_header(("Location", download_url))
        .insert_header(("X-Repo-Commit", commit_id.as_str()))
        .insert_header(("X-Linked-Size", file_entry.size.to_string()))
        .insert_header(("X-Linked-Etag", file_entry.cas_hash.as_str()))
        .finish()
}

// Model resolve handler
pub async fn resolve_model(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    handle_resolve(req, path, RepoType::Model, auth, metadata, config).await
}

// Dataset resolve handler
pub async fn resolve_dataset(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    handle_resolve(req, path, RepoType::Dataset, auth, metadata, config).await
}

// Space resolve handler
pub async fn resolve_space(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
    config: web::Data<HubConfig>,
) -> HttpResponse {
    handle_resolve(req, path, RepoType::Space, auth, metadata, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token_store::TokenStore;
    use crate::metadata::{FileEntry, Revision, SqliteMetadataStore};
    use actix_web::{App, test as actix_test};

    async fn setup_test_env_with_files() -> (
        std::sync::Arc<TokenStore>,
        std::sync::Arc<dyn MetadataStore>,
        HubConfig,
    ) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().await.unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> =
            std::sync::Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        let config = HubConfig::default();
        (token_store, metadata, config)
    }

    #[actix_web::test]
    async fn test_resolve_existing_file() {
        let (token_store, metadata, config) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();

        // M2: Create XetSigner for testing proxy token generation
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let xet_signer = std::sync::Arc::new(crate::auth::xet_signer::XetSigner::new(
            signing_key,
            "test-kid",
            3600,
            300,
        ));

        // Create repo and add files
        let repo = metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();
        let commit_id = "abc123";
        let revision = Revision {
            commit_id: commit_id.to_string(),
            repo_id: repo.id,
            parent: None,
            message: "Initial".to_string(),
            author: "testuser".to_string(),
            created_at: 1000,
        };
        metadata.add_revision(revision).await.unwrap();
        metadata.set_head(repo.id, commit_id).await.unwrap();

        // Add file entry
        let entries = vec![FileEntry {
            path: "model.bin".to_string(),
            repo_id: repo.id,
            commit_id: commit_id.to_string(),
            size: 1024,
            cas_hash: "hash123".to_string(),
            is_lfs: true,
        }];
        metadata.add_file_entries(entries).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                // M2: Register XetSigner to test proxy token generation
                .app_data(web::Data::new(xet_signer.clone()))
                .route(
                    "/{ns}/{repo}/resolve/{revision}/{path}",
                    web::get().to(resolve_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/testuser/my-model/resolve/main/model.bin")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        // No CAS client registered, so handler falls through to 302 redirect
        assert_eq!(resp.status().as_u16(), 302);
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(location.contains("hash123"));
        // I-3: Proxy tokens use base64url encoding (URL-safe), no percent-encoding needed
        assert!(
            location.contains("?token=proxy_"),
            "Redirect URL should contain proxy token: {}",
            location
        );
        // Verify HF Hub compatibility headers
        assert!(resp.headers().get("X-Repo-Commit").is_some());
        assert!(resp.headers().get("X-Linked-Size").is_some());
        assert_eq!(
            resp.headers()
                .get("X-Linked-Size")
                .unwrap()
                .to_str()
                .unwrap(),
            "1024"
        );
    }

    #[actix_web::test]
    async fn test_resolve_missing_file() {
        let (token_store, metadata, config) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();

        // Create repo with no files
        let repo = metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();
        let commit_id = "abc123";
        let revision = Revision {
            commit_id: commit_id.to_string(),
            repo_id: repo.id,
            parent: None,
            message: "Initial".to_string(),
            author: "testuser".to_string(),
            created_at: 1000,
        };
        metadata.add_revision(revision).await.unwrap();
        metadata.set_head(repo.id, commit_id).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/{ns}/{repo}/resolve/{revision}/{path}",
                    web::get().to(resolve_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/testuser/my-model/resolve/main/nonexistent.bin")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_resolve_private_repo_denies_non_owner() {
        let (token_store, metadata, config) = setup_test_env_with_files().await;
        // attacker 的 read token
        let token = token_store
            .create_token("attacker", "t", "read")
            .await
            .unwrap();
        // 私有 repo,owner 是别人
        let repo = metadata
            .create_repo("owner", "secret-model", RepoType::Model, true)
            .await
            .unwrap();
        let commit_id = "abc123";
        metadata
            .add_revision(Revision {
                commit_id: commit_id.to_string(),
                repo_id: repo.id,
                parent: None,
                message: "i".to_string(),
                author: "owner".to_string(),
                created_at: 1000,
            })
            .await
            .unwrap();
        metadata.set_head(repo.id, commit_id).await.unwrap();
        metadata
            .add_file_entries(vec![FileEntry {
                path: "model.bin".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 10,
                cas_hash: "h".to_string(),
                is_lfs: true,
            }])
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(config.clone()))
                .route(
                    "/{ns}/{repo}/resolve/{revision}/{path}",
                    web::get().to(resolve_model),
                ),
        )
        .await;
        let req = actix_test::TestRequest::get()
            .uri("/owner/secret-model/resolve/main/model.bin")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }
}
