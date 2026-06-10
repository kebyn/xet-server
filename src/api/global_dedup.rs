//! Global Deduplication API
//!
//! GET /v1/chunks/{prefix}/{hash} - Query chunk deduplication information

use actix_web::{web, HttpResponse};
use serde::{Serialize, Deserialize};

use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::StorageBackend;

#[derive(Serialize, Deserialize)]
struct ChunkDedupResponse {
    hash: String,
    found: bool,
    xorb_hash: Option<String>,
    chunk_index: Option<u32>,
}

/// Query chunk deduplication information
pub async fn query_chunk_dedup(
    path: web::Path<(String, String)>,
    index: web::Data<MetadataIndex>,
    _storage: web::Data<Box<dyn StorageBackend>>,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let (prefix, hash) = path.into_inner();

    // Validate prefix
    if prefix != "default" {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid prefix, expected 'default'"
        }));
    }

    // Validate hash format (should be a hex hash)
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid hash format, expected 64-character hex string"
        }));
    }

    // Look up chunk in metadata index
    let response = match index.get_xorb_for_chunk(&hash) {
        Some((xorb_hash, chunk_index)) => {
            ChunkDedupResponse {
                hash,
                found: true,
                xorb_hash: Some(xorb_hash),
                chunk_index: Some(chunk_index),
            }
        }
        None => {
            ChunkDedupResponse {
                hash,
                found: false,
                xorb_hash: None,
                chunk_index: None,
            }
        }
    };

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};
    use crate::storage::local::LocalStorage;
    use tempfile::tempdir;

    #[actix_web::test]
    async fn test_chunk_dedup_not_found() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .route("/v1/chunks/{prefix}/{hash}", web::get().to(query_chunk_dedup))
        ).await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/chunks/default/{}", hash))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body: ChunkDedupResponse = test::read_body_json(resp).await;
        assert_eq!(body.found, false);
        assert_eq!(body.hash, hash);
    }

    #[actix_web::test]
    async fn test_chunk_dedup_invalid_prefix() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .route("/v1/chunks/{prefix}/{hash}", web::get().to(query_chunk_dedup))
        ).await;

        let hash = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/chunks/invalid/{}", hash))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn test_chunk_dedup_invalid_hash() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let index = MetadataIndex::new();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .route("/v1/chunks/{prefix}/{hash}", web::get().to(query_chunk_dedup))
        ).await;

        let req = test::TestRequest::get()
            .uri("/v1/chunks/default/invalid_hash")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }
}
