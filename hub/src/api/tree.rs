use super::error_json;
use crate::auth::extract::{AuthRead, AuthUser};
use crate::config::HubConfig;
use crate::error::internal_error_response;
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

fn tree_service_error_response(err: TreeServiceError) -> HttpResponse {
    match err {
        TreeServiceError::NotFound(msg) => {
            HttpResponse::NotFound().json(error_json(msg, "NotFoundError"))
        }
        TreeServiceError::Validation(msg) => {
            HttpResponse::BadRequest().json(error_json(msg, "ValidationError"))
        }
        TreeServiceError::Internal(msg) => internal_error_response("Tree request failed", msg),
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

fn parse_cursor_query(req: &HttpRequest) -> Result<Option<String>, String> {
    let mut cursor = None;
    for (key, value) in url::form_urlencoded::parse(req.query_string().as_bytes()) {
        if key != "cursor" {
            continue;
        }
        if cursor.replace(value.into_owned()).is_some() {
            return Err("Tree pagination cursor must be specified at most once".to_string());
        }
    }
    Ok(cursor)
}

fn replace_tree_revision(path: &str, commit_id: &str) -> String {
    // `splitn` keeps the suffix (including a trailing slash) intact while
    // locating the route's fixed `tree` segment. Searching for `/tree/` in
    // the whole string could replace a namespace or repository with that
    // name instead of the revision segment.
    let mut segments = path.splitn(7, '/');
    let prefix_segments: Vec<_> = (0..6).filter_map(|_| segments.next()).collect();
    if prefix_segments.len() != 6 || prefix_segments[5] != "tree" {
        return path.to_string();
    }
    let Some(revision_and_suffix) = segments.next() else {
        return path.to_string();
    };
    let suffix_start = revision_and_suffix
        .find('/')
        .unwrap_or(revision_and_suffix.len());
    format!(
        "{}/{}{}",
        prefix_segments.join("/"),
        commit_id,
        &revision_and_suffix[suffix_start..]
    )
}

fn build_next_link(req: &HttpRequest, config: &HubConfig, commit_id: &str, cursor: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in url::form_urlencoded::parse(req.query_string().as_bytes()) {
        if key != "cursor" {
            query.append_pair(&key, &value);
        }
    }
    query.append_pair("cursor", cursor);
    let query = query.finish();
    format!(
        "{}{}?{}",
        config.server.base_url().trim_end_matches('/'),
        replace_tree_revision(req.path(), commit_id),
        query
    )
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
    repo_type: RepoType,
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let (namespace, repo_name, revision, tree_path) = path.into_inner();
    let recursive = parse_recursive_query(&req);
    let cursor = match parse_cursor_query(&req) {
        Ok(cursor) => cursor,
        Err(error) => {
            return HttpResponse::BadRequest().json(error_json(error, "ValidationError"));
        }
    };
    let service = tree_service(&metadata);
    let page = match service
        .list_tree(TreeListRequest {
            username: &auth.info.username,
            namespace: &namespace,
            repo_name: &repo_name,
            repo_type,
            revision: &revision,
            tree_path: &tree_path,
            recursive,
            cursor: cursor.as_deref(),
        })
        .await
    {
        Ok(page) => page,
        Err(err) => return tree_service_error_response(err),
    };

    let tree_entries: Vec<TreeEntry> = page.entries.into_iter().map(service_entry_to_api).collect();
    let mut response = HttpResponse::Ok();
    if let Some(next_cursor) = page.next_cursor {
        let Some(config) = req.app_data::<web::Data<HubConfig>>() else {
            return internal_error_response(
                "Tree pagination failed",
                "Hub configuration is unavailable",
            );
        };
        let next_link = build_next_link(&req, config, &page.commit_id, &next_cursor);
        response.insert_header(("Link", format!("<{next_link}>; rel=\"next\"")));
    }
    response.json(tree_entries)
}

/// Tree handler for routes without a path segment: forwards to
/// [`handle_tree`] with an empty path.
async fn handle_tree_no_path(
    repo_type: RepoType,
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    auth: AuthUser<AuthRead>,
    metadata: web::Data<std::sync::Arc<dyn MetadataStore>>,
) -> HttpResponse {
    let (ns, repo, rev) = path.into_inner();
    let full_path = web::Path::from((ns, repo, rev, "".to_string()));
    handle_tree(repo_type, req, full_path, auth, metadata).await
}

repo_type_handlers! {
    /// GET /api/{models,datasets,spaces}/{ns}/{repo}/tree/{rev}/{path:.*}
    [tree_model, tree_dataset, tree_space]
    (
        req: HttpRequest,
        path: web::Path<(String, String, String, String)>,
        auth: AuthUser<AuthRead>,
        metadata: web::Data<std::sync::Arc<dyn MetadataStore>>
    ) -> HttpResponse
    = handle_tree(req, path, auth, metadata)
}

repo_type_handlers! {
    /// GET /api/{models,datasets,spaces}/{ns}/{repo}/tree/{rev}
    [tree_model_no_path, tree_dataset_no_path, tree_space_no_path]
    (
        req: HttpRequest,
        path: web::Path<(String, String, String)>,
        auth: AuthUser<AuthRead>,
        metadata: web::Data<std::sync::Arc<dyn MetadataStore>>
    ) -> HttpResponse
    = handle_tree_no_path(req, path, auth, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token_store::TokenStore;
    use crate::metadata::{FileEntry, FileTreeChange, Revision, SqliteMetadataStore};
    use actix_web::{App, test as actix_test};

    #[test]
    fn next_link_replaces_only_the_tree_revision_segment() {
        assert_eq!(
            replace_tree_revision(
                "/api/models/tree/tree/tree/main/models/tree/",
                "0123456789abcdef"
            ),
            "/api/models/tree/tree/tree/0123456789abcdef/models/tree/"
        );
        assert_eq!(
            replace_tree_revision("/api/models/ns/repo/tree/main/", "commit"),
            "/api/models/ns/repo/tree/commit/"
        );
    }

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
        let commit_id = "abcdef1234567890";
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
        let commit_id = "abcdef1234567890";
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
        let commit_id = "abcdef1234567890";
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
        let commit_id = "abcdef1234567890";
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
        let commit_id = "abcdef1234567890";
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

    #[actix_web::test]
    async fn test_recursive_tree_listing_uses_hf_link_pagination() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();
        let repo = metadata
            .create_repo("testuser", "large-tree", RepoType::Model, false)
            .await
            .unwrap();
        let commit_id = "abcdef1234567890";
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
            .add_file_entries(
                (0..1001)
                    .map(|index| FileEntry {
                        path: format!("files/{index:04}.bin"),
                        repo_id: repo.id,
                        commit_id: commit_id.to_string(),
                        size: 1,
                        cas_hash: format!("hash-{index}"),
                        is_lfs: true,
                    })
                    .collect(),
            )
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store))
                .app_data(web::Data::new(metadata.clone()))
                .app_data(web::Data::new(HubConfig::default()))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/large-tree/tree/main/?recursive=true&expand=false")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let link = resp
            .headers()
            .get("Link")
            .expect("first tree page should advertise the next page")
            .to_str()
            .unwrap()
            .to_string();
        let first_page: Vec<TreeEntry> = actix_test::read_body_json(resp).await;
        assert_eq!(first_page.len(), 1000);

        let next_url = link
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix(">; rel=\"next\""))
            .expect("Link header should contain one next relation");
        let next_url = url::Url::parse(next_url).expect("next link should be absolute");
        assert!(next_url.path().contains("/tree/abcdef1234567890/"));
        let next_query: std::collections::HashMap<_, _> =
            next_url.query_pairs().into_owned().collect();
        assert_eq!(
            next_query.get("recursive").map(String::as_str),
            Some("true")
        );
        assert_eq!(next_query.get("expand").map(String::as_str), Some("false"));
        assert!(next_query.contains_key("cursor"));

        // Move HEAD after the first page. The generated link must continue to
        // read the commit selected by the first request.
        let next_commit = "1234567890abcdef";
        metadata
            .commit_changes_atomic(
                &Revision {
                    commit_id: next_commit.to_string(),
                    repo_id: repo.id,
                    parent: Some(commit_id.to_string()),
                    message: "remove tail".to_string(),
                    author: "testuser".to_string(),
                    created_at: 1001,
                },
                &[
                    FileTreeChange::Delete("files/1000.bin".to_string()),
                    FileTreeChange::Upsert(FileEntry {
                        path: "files/2000.bin".to_string(),
                        repo_id: repo.id,
                        commit_id: next_commit.to_string(),
                        size: 1,
                        cas_hash: "hash-2000".to_string(),
                        is_lfs: true,
                    }),
                ],
                Some(commit_id),
            )
            .await
            .unwrap();
        let next_uri = match next_url.query() {
            Some(query) => format!("{}?{}", next_url.path(), query),
            None => next_url.path().to_string(),
        };
        let req = actix_test::TestRequest::get()
            .uri(&next_uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        assert!(resp.headers().get("Link").is_none());
        let second_page: Vec<TreeEntry> = actix_test::read_body_json(resp).await;
        assert_eq!(second_page.len(), 1);
        assert_eq!(second_page[0].path, "files/1000.bin");
    }

    #[actix_web::test]
    async fn test_non_recursive_tree_pagination_does_not_repeat_directory() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();
        let repo = metadata
            .create_repo("testuser", "large-directory", RepoType::Model, false)
            .await
            .unwrap();
        let commit_id = "abcdef1234567890";
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
            .add_file_entries(
                (0..1001)
                    .map(|index| FileEntry {
                        path: format!("one-directory/{index:04}.bin"),
                        repo_id: repo.id,
                        commit_id: commit_id.to_string(),
                        size: 1,
                        cas_hash: format!("hash-{index}"),
                        is_lfs: true,
                    })
                    .collect(),
            )
            .await
            .unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store))
                .app_data(web::Data::new(metadata))
                .app_data(web::Data::new(HubConfig::default()))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/models/testuser/large-directory/tree/main/")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        let link = resp.headers().get("Link").unwrap().to_str().unwrap();
        let next_url = link
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix(">; rel=\"next\""))
            .and_then(|value| url::Url::parse(value).ok())
            .unwrap();
        let first_page: Vec<TreeEntry> = actix_test::read_body_json(resp).await;
        assert_eq!(first_page.len(), 1);
        assert_eq!(first_page[0].entry_type, "directory");
        assert_eq!(first_page[0].path, "one-directory");

        let next_uri = format!("{}?{}", next_url.path(), next_url.query().unwrap());
        let req = actix_test::TestRequest::get()
            .uri(&next_uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.headers().get("Link").is_none());
        let second_page: Vec<TreeEntry> = actix_test::read_body_json(resp).await;
        assert!(second_page.is_empty());
    }

    #[actix_web::test]
    async fn test_tree_rejects_malformed_or_repeated_cursor() {
        let (token_store, metadata) = setup_test_env_with_files().await;
        let token = token_store
            .create_token("testuser", "test-token", "read")
            .await
            .unwrap();
        let repo = metadata
            .create_repo("testuser", "cursor-test", RepoType::Model, false)
            .await
            .unwrap();
        let commit_id = "abcdef1234567890";
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

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(token_store))
                .app_data(web::Data::new(metadata))
                .route(
                    "/api/models/{ns}/{repo}/tree/{revision}/{path:.*}",
                    web::get().to(tree_model),
                ),
        )
        .await;

        for query in ["cursor=%2A%2A%2A", "cursor=YQ&cursor=Yg"] {
            let req = actix_test::TestRequest::get()
                .uri(&format!(
                    "/api/models/testuser/cursor-test/tree/main/?{query}"
                ))
                .insert_header(("Authorization", format!("Bearer {token}")))
                .to_request();
            let resp = actix_test::call_service(&app, req).await;
            assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
            let body: serde_json::Value = actix_test::read_body_json(resp).await;
            assert_eq!(body["error_type"], "ValidationError");
        }
    }
}
