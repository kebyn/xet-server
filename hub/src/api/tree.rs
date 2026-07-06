use crate::auth::extract::{AuthRead, AuthUser};
use crate::metadata::{MetadataStore, RepoType};
use crate::services::tree::{
    TreeListRequest, TreeListingEntry, TreeListingEntryType, TreeService, TreeServiceError,
};
use actix_web::{HttpRequest, HttpResponse, web};
use serde::Serialize;
use std::sync::Arc;

/// Tree entry response
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct TreeEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub oid: Option<String>,
    pub size: u64,
    pub path: String,
}

fn tree_service(metadata: &web::Data<Arc<dyn MetadataStore>>) -> TreeService {
    TreeService::new(metadata.get_ref().clone())
}

fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}

fn tree_service_error_response(err: TreeServiceError) -> HttpResponse {
    match err {
        TreeServiceError::NotFound(msg) => {
            HttpResponse::NotFound().json(error_json(msg, "NotFoundError"))
        }
        TreeServiceError::Internal(msg) => {
            HttpResponse::InternalServerError().json(error_json(msg, "InternalError"))
        }
    }
}

fn parse_recursive_query(req: &HttpRequest) -> bool {
    req.uri()
        .query()
        .map(|q| {
            q.split('&').any(|pair| {
                pair.split_once('=')
                    .map(|(k, v)| k == "recursive" && v == "true")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn service_entry_to_api(entry: TreeListingEntry) -> TreeEntry {
    let entry_type = match entry.entry_type {
        TreeListingEntryType::File => "file",
        TreeListingEntryType::Directory => "directory",
    };

    TreeEntry {
        entry_type: entry_type.to_string(),
        oid: entry.oid,
        size: entry.size,
        path: entry.path,
    }
}

/// Internal helper for tree listing
async fn handle_tree(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    repo_type: RepoType,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let (namespace, repo_name, revision, tree_path) = path.into_inner();
    let recursive = parse_recursive_query(&req);
    let service = tree_service(&metadata);
    let entries = match service
        .list_tree(TreeListRequest {
            username: &auth.info.username,
            namespace: &namespace,
            repo_name: &repo_name,
            repo_type,
            revision: &revision,
            tree_path: &tree_path,
            recursive,
        })
        .await
    {
        Ok(entries) => entries,
        Err(err) => return tree_service_error_response(err),
    };

    let tree_entries: Vec<TreeEntry> = entries.into_iter().map(service_entry_to_api).collect();
    HttpResponse::Ok().json(tree_entries)
}

// Model tree handler
pub async fn tree_model(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_tree(req, path, RepoType::Model, auth, metadata).await
}

// Dataset tree handler
pub async fn tree_dataset(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_tree(req, path, RepoType::Dataset, auth, metadata).await
}

// Space tree handler
pub async fn tree_space(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_tree(req, path, RepoType::Space, auth, metadata).await
}

// Generic no-path tree handler
async fn handle_tree_no_path(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    repo_type: RepoType,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let (ns, repo, rev) = path.into_inner();
    let full_path = web::Path::from((ns, repo, rev, "".to_string()));
    handle_tree(req, full_path, repo_type, auth, metadata).await
}

pub async fn tree_model_no_path(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_tree_no_path(req, path, RepoType::Model, auth, metadata).await
}

pub async fn tree_dataset_no_path(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_tree_no_path(req, path, RepoType::Dataset, auth, metadata).await
}

pub async fn tree_space_no_path(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    handle_tree_no_path(req, path, RepoType::Space, auth, metadata).await
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
    ) {
        let token_store = std::sync::Arc::new(TokenStore::in_memory().await.unwrap());
        let metadata: std::sync::Arc<dyn MetadataStore> =
            std::sync::Arc::new(SqliteMetadataStore::in_memory().await.unwrap());
        (token_store, metadata)
    }

    #[actix_web::test]
    async fn test_tree_listing() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();

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

        // Add file entries
        let entries = vec![
            FileEntry {
                path: "model.bin".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 1024,
                cas_hash: "hash1".to_string(),
                is_lfs: true,
            },
            FileEntry {
                path: "config.json".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 256,
                cas_hash: "hash2".to_string(),
                is_lfs: false,
            },
        ];
        metadata.add_file_entries(entries).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/my-model/tree/main/")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: Vec<TreeEntry> = actix_test::read_body_json(resp).await;
        assert_eq!(body.len(), 2);
    }

    #[actix_web::test]
    async fn test_tree_private_repo_denies_non_owner() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("attacker", "t", "read")
            .await
            .unwrap();
        // 私有 repo,owner 是别人
        let repo = metadata
            .create_repo("owner", "secret", RepoType::Model, true)
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
                size: 1024,
                cas_hash: "secret_hash".to_string(),
                is_lfs: true,
            }])
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;
        let req = actix_test::TestRequest::get()
            .uri("/api/models/owner/secret/tree/main/")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_tree_private_repo_allows_owner() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("owner", "t", "read")
            .await
            .unwrap();
        let repo = metadata
            .create_repo("owner", "secret", RepoType::Model, true)
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
                size: 1024,
                cas_hash: "h".to_string(),
                is_lfs: true,
            }])
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;
        let req = actix_test::TestRequest::get()
            .uri("/api/models/owner/secret/tree/main/")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_tree_non_recursive_joins_nested_directory_with_slash() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();
        let repo = metadata
            .create_repo("testuser", "my-model", RepoType::Model, false)
            .await
            .unwrap();
        let commit_id = "abc123";
        metadata
            .add_revision(Revision {
                commit_id: commit_id.to_string(),
                repo_id: repo.id,
                parent: None,
                message: "Initial".to_string(),
                author: "testuser".to_string(),
                created_at: 1000,
            })
            .await
            .unwrap();
        metadata.set_head(repo.id, commit_id).await.unwrap();
        metadata
            .add_file_entries(vec![FileEntry {
                path: "models/sub/a.bin".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 1,
                cas_hash: "hash".to_string(),
                is_lfs: true,
            }])
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/my-model/tree/main/models")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let body: Vec<TreeEntry> = actix_test::read_body_json(resp).await;
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].entry_type, "directory");
        assert_eq!(body[0].path, "models/sub");
    }

    #[actix_web::test]
    async fn test_tree_with_subdirectories() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();

        // Create repo and add files with nested paths
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

        // Add file entries with nested paths
        let entries = vec![
            FileEntry {
                path: "models/model.bin".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 1024,
                cas_hash: "hash1".to_string(),
                is_lfs: true,
            },
            FileEntry {
                path: "models/config.json".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 256,
                cas_hash: "hash2".to_string(),
                is_lfs: false,
            },
            FileEntry {
                path: "README.md".to_string(),
                repo_id: repo.id,
                commit_id: commit_id.to_string(),
                size: 128,
                cas_hash: "hash3".to_string(),
                is_lfs: false,
            },
        ];
        metadata.add_file_entries(entries).await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store.clone()))
                .app_data(web::Data::new(metadata.clone()))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/my-model/tree/main/")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();

        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: Vec<TreeEntry> = actix_test::read_body_json(resp).await;
        // Should have README.md file and "models" directory
        assert_eq!(body.len(), 2);

        // Check for directory
        let dir_entry = body.iter().find(|e| e.entry_type == "directory");
        assert!(dir_entry.is_some());
        assert_eq!(dir_entry.unwrap().path, "models");

        // Check for file
        let file_entry = body.iter().find(|e| e.path == "README.md");
        assert!(file_entry.is_some());
        assert_eq!(file_entry.unwrap().entry_type, "file");
    }
}
