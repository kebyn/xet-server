//! Reconstruction API
//!
//! GET /v2/reconstructions/{file_id} - Get file reconstruction information

use actix_web::{web, HttpResponse};
use serde::Serialize;

use crate::config::ServerConfig;
use crate::index::MetadataIndex;
use crate::storage::StorageBackend;
use crate::format::shard::MDBShardFile;

#[derive(Serialize)]
struct ReconstructionResponse {
    file_id: String,
    xorbs: Vec<XorbInfo>,
}

#[derive(Serialize)]
struct XorbInfo {
    xorb_hash: String,
    chunks: Vec<ChunkInfo>,
}

#[derive(Serialize)]
struct ChunkInfo {
    chunk_hash: String,
    offset: u64,
    length: u64,
}

/// Get file reconstruction information
pub async fn get_reconstruction(
    path: web::Path<String>,
    index: web::Data<MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
    config: web::Data<ServerConfig>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let file_id = path.into_inner();

    // Validate file_id format (should be a hex hash)
    if file_id.len() != 64 || !file_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid file_id format, expected 64-character hex string"
        }));
    }

    // Look up shards for this file
    let shard_ids = match index.get_shards_for_file(&file_id) {
        Some(ids) => ids,
        None => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "File not found"
            }));
        }
    };

    // Collect xorb information from all shards
    let mut xorbs = Vec::new();

    for shard_id in shard_ids {
        let shard_key = format!("shards/{}", shard_id);

        // Fetch shard from storage
        let shard_data = match storage.get(&shard_key).await {
            Ok(data) => data,
            Err(e) => {
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("Failed to fetch shard: {}", e)
                }));
            }
        };

        // Parse shard
        let shard = match MDBShardFile::parse(&shard_data) {
            Ok(s) => s,
            Err(e) => {
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("Failed to parse shard: {}", e)
                }));
            }
        };

        // Extract xorb information
        // TODO: This is a simplified implementation. In production, we would:
        // 1. Find the specific file entry in the shard
        // 2. Extract the exact xorb references for that file
        // 3. Build the complete reconstruction response

        for xorb_entry in &shard.xorb_entries {
            let xorb_info = XorbInfo {
                xorb_hash: xorb_entry.xorb_hash.to_hex(),
                chunks: Vec::new(), // TODO: Populate with actual chunk info
            };
            xorbs.push(xorb_info);
        }
    }

    let response = ReconstructionResponse {
        file_id,
        xorbs,
    };

    HttpResponse::Ok().json(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};
    use crate::storage::local::LocalStorage;
    use tempfile::tempdir;

    #[actix_web::test]
    async fn test_reconstruction_not_found() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let index = MetadataIndex::new();
        let config = ServerConfig::default();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .route("/v2/reconstructions/{file_id}", web::get().to(get_reconstruction))
        ).await;

        let file_id = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v2/reconstructions/{}", file_id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn test_reconstruction_invalid_file_id() {
        let dir = tempdir().unwrap();
        let storage: Box<dyn StorageBackend> = Box::new(
            LocalStorage::new(dir.path().to_str().unwrap()).unwrap()
        );

        let index = MetadataIndex::new();
        let config = ServerConfig::default();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(index))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(config))
                .route("/v2/reconstructions/{file_id}", web::get().to(get_reconstruction))
        ).await;

        let req = test::TestRequest::get()
            .uri("/v2/reconstructions/invalid")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }
}
