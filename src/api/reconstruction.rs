//! Reconstruction API
//!
//! GET /v1/reconstructions/{file_id} - Get file reconstruction information (V1 format)
//! GET /v2/reconstructions/{file_id} - Get file reconstruction information (V2 format)

use actix_web::{web, HttpResponse};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use crate::config::ServerConfig;
use crate::index::MetadataIndex;
use crate::metrics::GLOBAL_METRICS;
use crate::storage::StorageBackend;
use crate::format::shard::MDBShardFile;

// V1 Response structures
#[derive(Serialize)]
struct ReconstructionResponseV1 {
    file_id: String,
    xorbs: Vec<XorbInfoV1>,
}

#[derive(Serialize)]
struct XorbInfoV1 {
    xorb_hash: String,
    size: u64,
    chunks: Vec<ChunkInfoV1>,
}

#[derive(Serialize)]
struct ChunkInfoV1 {
    chunk_hash: String,
    offset: u64,
    length: u64,
}

// V2 Response structures (with fetch_info)
#[derive(Serialize)]
struct ReconstructionResponseV2 {
    file_id: String,
    xorbs: Vec<XorbInfoV2>,
    fetch_info: HashMap<String, XorbFetchInfo>,
}

#[derive(Serialize)]
struct XorbInfoV2 {
    xorb_hash: String,
    size: u64,
}

#[derive(Serialize)]
struct XorbFetchInfo {
    storage_path: String,
    size: u64,
}

/// Get file reconstruction information (V1 format)
/// Returns detailed chunk-level information for backward compatibility
pub async fn get_reconstruction_v1(
    path: web::Path<String>,
    index: web::Data<MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
    _config: web::Data<ServerConfig>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let file_id = path.into_inner();

    // Validate file_id format (should be a hex hash)
    if file_id.len() != 64 || !file_id.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid file_id format, expected 64-character hex string"
        }));
    }

    // Look up shards for this file
    let shard_ids = match index.get_shards_for_file(&file_id) {
        Some(ids) => ids,
        None => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "File not found"
            }));
        }
    };

    // Collect xorb information from all shards
    let mut xorbs = Vec::new();
    let mut seen_xorbs = HashSet::new();

    for shard_id in shard_ids {
        let shard_key = format!("shards/{}", shard_id);

        // Fetch shard from storage
        let shard_data = match storage.get(&shard_key).await {
            Ok(data) => {
                GLOBAL_METRICS.record_storage_operation();
                data
            }
            Err(e) => {
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("Failed to fetch shard: {}", e)
                }));
            }
        };

        // Parse shard
        let shard = match MDBShardFile::parse(&shard_data) {
            Ok(s) => s,
            Err(e) => {
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("Failed to parse shard: {}", e)
                }));
            }
        };

        // Extract xorb information (deduplicated)
        let mut chunk_index_offset = 0;
        for xorb_entry in &shard.xorb_entries {
            let xorb_hash = xorb_entry.xorb_hash.to_hex();
            let xorb_size = xorb_entry.num_bytes_in_xorb as u64;
            if seen_xorbs.insert(xorb_hash.clone()) {
                // Collect chunks for this xorb
                let mut chunks = Vec::new();
                for i in 0..xorb_entry.num_entries as usize {
                    if chunk_index_offset + i < shard.xorb_chunk_entries.len() {
                        let chunk_entry = &shard.xorb_chunk_entries[chunk_index_offset + i];
                        chunks.push(ChunkInfoV1 {
                            chunk_hash: chunk_entry.chunk_hash.to_hex(),
                            offset: chunk_entry.chunk_byte_range_start as u64,
                            length: chunk_entry.unpacked_segment_bytes as u64,
                        });
                    }
                }
                chunk_index_offset += xorb_entry.num_entries as usize;

                let xorb_info = XorbInfoV1 {
                    xorb_hash,
                    size: xorb_size,
                    chunks,
                };
                xorbs.push(xorb_info);
            } else {
                // Skip chunks for duplicate xorbs
                chunk_index_offset += xorb_entry.num_entries as usize;
            }
        }
    }

    // Calculate total download bytes (sum of all xorb sizes)
    let total_download_bytes: u64 = xorbs.iter()
        .map(|x| x.size)
        .sum();

    let response = ReconstructionResponseV1 {
        file_id,
        xorbs,
    };

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(total_download_bytes);
    GLOBAL_METRICS.record_latency(start);

    HttpResponse::Ok().json(response)
}

/// Get file reconstruction information (V2 format)
/// Returns xorb-level information with fetch_info for efficient retrieval
pub async fn get_reconstruction(
    path: web::Path<String>,
    index: web::Data<MetadataIndex>,
    storage: web::Data<Box<dyn StorageBackend>>,
    _config: web::Data<ServerConfig>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    let start = std::time::Instant::now();
    let file_id = path.into_inner();

    // Validate file_id format (should be a hex hash)
    if file_id.len() != 64 || !file_id.chars().all(|c| c.is_ascii_hexdigit()) {
        GLOBAL_METRICS.record_request(400);
        GLOBAL_METRICS.record_latency(start);
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Invalid file_id format, expected 64-character hex string"
        }));
    }

    // Look up shards for this file
    let shard_ids = match index.get_shards_for_file(&file_id) {
        Some(ids) => ids,
        None => {
            GLOBAL_METRICS.record_request(404);
            GLOBAL_METRICS.record_latency(start);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "File not found"
            }));
        }
    };

    // Collect xorb information from all shards (deduplicated)
    let mut xorbs = Vec::new();
    let mut fetch_info = HashMap::new();
    let mut seen_xorbs = HashSet::new();

    for shard_id in shard_ids {
        let shard_key = format!("shards/{}", shard_id);

        // Fetch shard from storage
        let shard_data = match storage.get(&shard_key).await {
            Ok(data) => {
                GLOBAL_METRICS.record_storage_operation();
                data
            }
            Err(e) => {
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("Failed to fetch shard: {}", e)
                }));
            }
        };

        // Parse shard
        let shard = match MDBShardFile::parse(&shard_data) {
            Ok(s) => s,
            Err(e) => {
                GLOBAL_METRICS.record_request(500);
                GLOBAL_METRICS.record_error();
                GLOBAL_METRICS.record_latency(start);
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": format!("Failed to parse shard: {}", e)
                }));
            }
        };

        // Extract xorb information (deduplicated)
        for xorb_entry in &shard.xorb_entries {
            let xorb_hash = xorb_entry.xorb_hash.to_hex();
            let xorb_size = xorb_entry.num_bytes_in_xorb as u64;
            let storage_path = format!("xorbs/default/{}", xorb_hash);

            // Only add to xorbs vec if not seen before
            if seen_xorbs.insert(xorb_hash.clone()) {
                xorbs.push(XorbInfoV2 {
                    xorb_hash: xorb_hash.clone(),
                    size: xorb_size,
                });

                fetch_info.insert(xorb_hash, XorbFetchInfo {
                    storage_path,
                    size: xorb_size,
                });
            }
        }
    }

    // Calculate total download bytes (sum of all xorb sizes)
    let total_download_bytes: u64 = xorbs.iter()
        .map(|x| x.size)
        .sum();

    let response = ReconstructionResponseV2 {
        file_id,
        xorbs,
        fetch_info,
    };

    GLOBAL_METRICS.record_request(200);
    GLOBAL_METRICS.record_download_bytes(total_download_bytes);
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

    #[actix_web::test]
    async fn test_reconstruction_v1_not_found() {
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
                .route("/v1/reconstructions/{file_id}", web::get().to(get_reconstruction_v1))
        ).await;

        let file_id = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/reconstructions/{}", file_id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn test_reconstruction_v1_invalid_file_id() {
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
                .route("/v1/reconstructions/{file_id}", web::get().to(get_reconstruction_v1))
        ).await;

        let req = test::TestRequest::get()
            .uri("/v1/reconstructions/invalid")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }
}
