//! Integration tests for the S3 storage backend.
//!
//! These run against an in-process mock S3 HTTP server (the same real-socket
//! pattern the hub crate uses for its mock CAS): the aws-sdk client is
//! pointed at it through `S3Storage::new(bucket, region, Some(endpoint))` —
//! the endpoint parameter enables path-style addressing — with dummy static
//! credentials, which the mock ignores. Failure modes are injected with 4xx
//! responses because the SDK retries 5xx/429.
//!
//! Every test is `#[serial]`: they all touch the AWS_ACCESS_KEY_ID /
//! AWS_SECRET_ACCESS_KEY environment variables.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use actix_web::{App, HttpResponse, HttpServer, web};
use bytes::Bytes;
use serial_test::serial;
use xet_server::storage::s3::S3Storage;
use xet_server::storage::{StorageBackend, StorageError};

const BUCKET: &str = "test-bucket";
/// Above the 5 MiB multipart threshold; with 8 MiB parts this is a 3-part upload.
const MULTIPART_FILE_SIZE: usize = 17 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Scoped environment variables (edition 2024: set_var/remove_var are unsafe)
// ---------------------------------------------------------------------------

struct ScopedEnv {
    key: &'static str,
    previous: Option<String>,
}

impl ScopedEnv {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        unsafe {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

fn dummy_aws_credentials() -> (ScopedEnv, ScopedEnv) {
    (
        ScopedEnv::set("AWS_ACCESS_KEY_ID", "test-access-key"),
        ScopedEnv::set("AWS_SECRET_ACCESS_KEY", "test-secret-key"),
    )
}

// ---------------------------------------------------------------------------
// Mock S3
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockS3 {
    /// key → bytes
    objects: HashMap<String, Vec<u8>>,
    /// upload_id → (key, part_number → bytes)
    multipart: HashMap<String, (String, BTreeMap<u32, Vec<u8>>)>,
    failures: MockFailures,
    list_injection: ListInjection,
    /// Part uploads park until `unblock` (Drop-cleanup test).
    block_parts: bool,
    unblock: bool,
    aborts: Vec<String>,
    part_uploads: u32,
    initiates: u32,
}

#[derive(Default)]
struct MockFailures {
    /// 1-based part number whose upload fails with 400.
    fail_part_number: Option<u32>,
    fail_complete: bool,
    fail_initiate: bool,
    /// HEAD object responds 403 (must surface as Internal, not NotFound).
    head_forbidden: bool,
}

#[derive(Default)]
struct ListInjection {
    /// Serve list results in pages of this many keys (0 = single page).
    page_size: usize,
    /// A continuation page repeats the token the client already used.
    repeat_token: bool,
    /// Claim IsTruncated without a NextContinuationToken.
    truncated_without_token: bool,
    /// Return more Contents entries than the requested page size.
    oversized_page: bool,
}

type Shared = Arc<Mutex<MockS3>>;

fn error_xml(code: &str, message: &str) -> String {
    format!("<Error><Code>{code}</Code><Message>{message}</Message></Error>")
}

fn bad_request(code: &str, message: &str) -> HttpResponse {
    HttpResponse::BadRequest()
        .content_type("application/xml")
        .body(error_xml(code, message))
}

async fn start_mock_s3() -> (String, Shared) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state: Shared = Arc::new(Mutex::new(MockS3::default()));

    let state_for_app = state.clone();
    let server = HttpServer::new(move || {
        let state = state_for_app.clone();
        App::new()
            // Multipart parts are 8 MiB each.
            .app_data(web::PayloadConfig::default().limit(64 * 1024 * 1024))
            .app_data(web::Data::new(state))
            // The SDK sends bucket-level operations (head_bucket,
            // list_objects_v2) to "/{bucket}/" with a trailing slash; object
            // routes are registered last so they cannot shadow these.
            .route("/{bucket}", web::get().to(list_objects_v2))
            .route("/{bucket}/", web::get().to(list_objects_v2))
            .route("/{bucket}", web::head().to(head_bucket))
            .route("/{bucket}/", web::head().to(head_bucket))
            .route("/{bucket}/{key:.*}", web::get().to(get_object))
            .route("/{bucket}/{key:.*}", web::head().to(head_object))
            .route("/{bucket}/{key:.*}", web::put().to(put_object_or_part))
            .route("/{bucket}/{key:.*}", web::post().to(post_object))
            .route(
                "/{bucket}/{key:.*}",
                web::delete().to(delete_object_or_abort),
            )
    })
    .listen(listener)
    .unwrap()
    .run();
    tokio::spawn(server);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mock S3 did not start listening on {addr}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    (format!("http://{addr}"), state)
}

async fn make_storage(endpoint: &str) -> S3Storage {
    S3Storage::new(BUCKET, Some("us-east-1"), Some(endpoint))
        .await
        .expect("S3Storage construction with dummy credentials must succeed")
}

// --- object routes ---

async fn get_object(state: web::Data<Shared>, path: web::Path<(String, String)>) -> HttpResponse {
    let (_bucket, key) = path.into_inner();
    let state = state.lock().unwrap();
    match state.objects.get(&key) {
        Some(data) => HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(data.clone()),
        None => HttpResponse::NotFound()
            .content_type("application/xml")
            .body(error_xml("NoSuchKey", "The specified key does not exist.")),
    }
}

/// HEAD returns the full body: actix-web drops the body bytes for HEAD
/// requests but keeps the computed Content-Length, which is what the
/// exists/get_size code reads.
async fn head_object(state: web::Data<Shared>, path: web::Path<(String, String)>) -> HttpResponse {
    let (_bucket, key) = path.into_inner();
    let state = state.lock().unwrap();
    if state.failures.head_forbidden {
        return HttpResponse::Forbidden().finish();
    }
    match state.objects.get(&key) {
        Some(data) => HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(data.clone()),
        None => HttpResponse::NotFound().finish(),
    }
}

async fn head_bucket() -> HttpResponse {
    HttpResponse::Ok().finish()
}

async fn put_object_or_part(
    state: web::Data<Shared>,
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
    body: web::Bytes,
) -> HttpResponse {
    let (_bucket, key) = path.into_inner();

    if query.contains_key("uploadId") && query.contains_key("partNumber") {
        let part_number: u32 = query["partNumber"].parse().unwrap();
        let upload_id = query["uploadId"].clone();

        // Park the part upload until the Drop-cleanup test unblocks it.
        loop {
            let (blocked, unblocked) = {
                let state = state.lock().unwrap();
                (state.block_parts, state.unblock)
            };
            if !blocked || unblocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let mut state = state.lock().unwrap();
        state.part_uploads += 1;
        if state.failures.fail_part_number == Some(part_number) {
            return bad_request("InternalError", "injected part-upload failure");
        }
        let (_key, parts) = state
            .multipart
            .entry(upload_id)
            .or_insert_with(|| (key.clone(), BTreeMap::new()));
        parts.insert(part_number, body.to_vec());
        return HttpResponse::Ok()
            .insert_header(("ETag", format!("mock-etag-{part_number}")))
            .finish();
    }

    let mut state = state.lock().unwrap();
    state.objects.insert(key, body.to_vec());
    HttpResponse::Ok().finish()
}

async fn post_object(
    state: web::Data<Shared>,
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
) -> HttpResponse {
    let (_bucket, key) = path.into_inner();

    // POST /{key}?uploads — initiate multipart upload.
    if query.contains_key("uploads") {
        let mut state = state.lock().unwrap();
        if state.failures.fail_initiate {
            return bad_request("InvalidRequest", "injected initiate failure");
        }
        state.initiates += 1;
        let upload_id = format!("mock-upload-{}", state.initiates);
        state
            .multipart
            .insert(upload_id.clone(), (key.clone(), BTreeMap::new()));
        let xml = format!(
            "<InitiateMultipartUploadResult>\
             <Bucket>{BUCKET}</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId>\
             </InitiateMultipartUploadResult>"
        );
        return HttpResponse::Ok().content_type("application/xml").body(xml);
    }

    // POST /{key}?uploadId=... — complete multipart upload.
    if let Some(upload_id) = query.get("uploadId") {
        let upload_id = upload_id.clone();
        let mut state = state.lock().unwrap();
        if state.failures.fail_complete {
            return bad_request("InvalidPart", "injected complete failure");
        }
        if let Some((key, parts)) = state.multipart.remove(&upload_id) {
            let mut assembled = Vec::new();
            for (_number, bytes) in parts {
                assembled.extend_from_slice(&bytes);
            }
            state.objects.insert(key, assembled);
        }
        return HttpResponse::Ok().content_type("application/xml").body(
            "<CompleteMultipartUploadResult><ETag>mock-etag</ETag></CompleteMultipartUploadResult>",
        );
    }

    HttpResponse::BadRequest().finish()
}

async fn delete_object_or_abort(
    state: web::Data<Shared>,
    path: web::Path<(String, String)>,
    query: web::Query<HashMap<String, String>>,
) -> HttpResponse {
    let (_bucket, key) = path.into_inner();
    let mut state = state.lock().unwrap();
    if let Some(upload_id) = query.get("uploadId") {
        state.aborts.push(upload_id.clone());
        state.multipart.remove(upload_id);
        return HttpResponse::NoContent().finish();
    }
    state.objects.remove(&key);
    HttpResponse::NoContent().finish()
}

// --- list route ---

fn list_xml(keys: &[String], truncated: bool, next_token: Option<&str>) -> String {
    let mut xml =
        String::from("<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for key in keys {
        xml.push_str(&format!("<Contents><Key>{key}</Key></Contents>"));
    }
    xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
    if let Some(token) = next_token {
        xml.push_str(&format!(
            "<NextContinuationToken>{token}</NextContinuationToken>"
        ));
    }
    xml.push_str("</ListBucketResult>");
    xml
}

async fn list_objects_v2(
    state: web::Data<Shared>,
    query: web::Query<HashMap<String, String>>,
    path: web::Path<String>,
) -> HttpResponse {
    let _bucket = path.into_inner();
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let continuation_token = query.get("continuation-token").cloned();

    let state = state.lock().unwrap();

    if state.list_injection.oversized_page {
        let key = state
            .objects
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "oversized-key".to_string());
        let keys = vec![key; 1001];
        return HttpResponse::Ok()
            .content_type("application/xml")
            .body(list_xml(&keys, false, None));
    }

    let mut keys: Vec<String> = state
        .objects
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .cloned()
        .collect();
    keys.sort();

    let page_size = state.list_injection.page_size;
    if page_size == 0 {
        return HttpResponse::Ok()
            .content_type("application/xml")
            .body(list_xml(&keys, false, None));
    }

    let page_index: usize = continuation_token
        .as_deref()
        .and_then(|token| token.parse().ok())
        .unwrap_or(0);
    let start = (page_index * page_size).min(keys.len());
    let end = (start + page_size).min(keys.len());
    let page: Vec<String> = keys[start..end].to_vec();

    if state.list_injection.truncated_without_token && page_index == 0 {
        return HttpResponse::Ok()
            .content_type("application/xml")
            .body(list_xml(&page, true, None));
    }

    let has_more = end < keys.len();
    let next_token = if has_more {
        Some((page_index + 1).to_string())
    } else {
        None
    };
    // Repeating a token the client already consumed must be rejected client-side.
    let next_token = if state.list_injection.repeat_token && page_index >= 1 {
        continuation_token
    } else {
        next_token
    };

    HttpResponse::Ok()
        .content_type("application/xml")
        .body(list_xml(&page, next_token.is_some(), next_token.as_deref()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a deterministic patterned file of `size` bytes.
fn write_patterned_file(dir: &tempfile::TempDir, name: &str, size: usize) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).unwrap();
    let mut written = 0usize;
    while written < size {
        let take = (256 * 1024).min(size - written);
        let block: Vec<u8> = (0..take).map(|i| ((written + i) % 251) as u8).collect();
        file.write_all(&block).unwrap();
        written += take;
    }
    file.flush().unwrap();
    path
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn constructor_requires_aws_credentials() {
    let _access_key = ScopedEnv::remove("AWS_ACCESS_KEY_ID");
    let _secret_key = ScopedEnv::remove("AWS_SECRET_ACCESS_KEY");

    let error = S3Storage::new(BUCKET, None, None)
        .await
        .err()
        .expect("missing credentials must fail construction");
    assert!(
        error.to_string().contains("AWS_ACCESS_KEY_ID"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
#[serial]
async fn crud_roundtrip() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    assert!(!storage.exists("obj").await.unwrap());

    let data = Bytes::from_static(b"hello s3");
    storage.put("obj", data.clone()).await.unwrap();
    assert_eq!(storage.get("obj").await.unwrap(), data);
    assert!(storage.exists("obj").await.unwrap());
    assert_eq!(storage.get_size("obj").await.unwrap(), 8);

    storage.delete("obj").await.unwrap();
    assert!(!storage.exists("obj").await.unwrap());
    assert!(matches!(
        storage.get("obj").await,
        Err(StorageError::NotFound(_))
    ));
    assert!(matches!(
        storage.get_size("obj").await,
        Err(StorageError::NotFound(_))
    ));
    // Deleting a missing key stays idempotent (S3 answers 204 either way).
    storage.delete("obj").await.unwrap();

    assert!(state.lock().unwrap().objects.is_empty());
}

#[tokio::test]
#[serial]
async fn health_check_hits_head_bucket() {
    let _creds = dummy_aws_credentials();
    let (endpoint, _state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    storage.health_check().await.unwrap();
}

#[tokio::test]
#[serial]
async fn list_objects_filters_by_prefix_in_sorted_order() {
    let _creds = dummy_aws_credentials();
    let (endpoint, _state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    for key in ["shards/b", "shards/a", "xorbs/z", "lfs/obj"] {
        storage.put(key, Bytes::from_static(b"x")).await.unwrap();
    }

    let keys = storage.list_objects("shards/").await.unwrap();
    assert_eq!(keys, vec!["shards/a".to_string(), "shards/b".to_string()]);
}

#[tokio::test]
#[serial]
async fn list_objects_walks_all_pages() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    for key in ["k1", "k2", "k3", "k4"] {
        storage.put(key, Bytes::from_static(b"x")).await.unwrap();
    }
    state.lock().unwrap().list_injection.page_size = 2;

    let keys = storage.list_objects("").await.unwrap();
    assert_eq!(
        keys,
        vec!["k1", "k2", "k3", "k4"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[serial]
async fn list_objects_rejects_a_repeated_continuation_token() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    for key in ["k1", "k2", "k3", "k4", "k5"] {
        storage.put(key, Bytes::from_static(b"x")).await.unwrap();
    }
    {
        let mut state = state.lock().unwrap();
        state.list_injection.page_size = 2;
        state.list_injection.repeat_token = true;
    }

    // The second page repeats the first token; only a controllable fake can
    // produce this and the defensive validation must reject it.
    let result = storage.list_objects("").await;
    assert!(
        result.is_err(),
        "repeated continuation token must be rejected"
    );
}

#[tokio::test]
#[serial]
async fn list_objects_rejects_truncation_without_a_next_token() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    storage.put("k1", Bytes::from_static(b"x")).await.unwrap();
    {
        let mut state = state.lock().unwrap();
        state.list_injection.page_size = 2;
        state.list_injection.truncated_without_token = true;
    }

    let result = storage.list_objects("").await;
    assert!(
        result.is_err(),
        "truncation without a token must be rejected"
    );
}

#[tokio::test]
#[serial]
async fn list_objects_rejects_a_page_larger_than_the_requested_size() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    storage.put("k1", Bytes::from_static(b"x")).await.unwrap();
    state.lock().unwrap().list_injection.oversized_page = true;

    let error = storage.list_objects("").await.unwrap_err();
    assert!(
        error.to_string().contains("exceeding requested page size"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
#[serial]
async fn put_from_path_small_file_uses_a_single_put() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    let dir = tempfile::tempdir().unwrap();
    let path = write_patterned_file(&dir, "small.bin", 4 * 1024 * 1024);
    storage.put_from_path("small", &path).await.unwrap();

    let state = state.lock().unwrap();
    assert_eq!(state.part_uploads, 0, "small files must not use multipart");
    assert_eq!(
        state.objects.get("small").map(Vec::len),
        Some(4 * 1024 * 1024)
    );
}

#[tokio::test]
#[serial]
async fn put_from_path_large_file_roundtrips_via_multipart() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    let dir = tempfile::tempdir().unwrap();
    let path = write_patterned_file(&dir, "large.bin", MULTIPART_FILE_SIZE);
    storage.put_from_path("large", &path).await.unwrap();

    {
        let state = state.lock().unwrap();
        assert_eq!(
            state.part_uploads, 3,
            "17 MiB with 8 MiB parts must upload in 3 parts"
        );
        assert!(
            state.multipart.is_empty(),
            "completed upload must be finalized"
        );
    }

    let downloaded = storage.get("large").await.unwrap();
    let expected = std::fs::read(&path).unwrap();
    assert_eq!(downloaded.len(), MULTIPART_FILE_SIZE);
    assert_eq!(downloaded, Bytes::from(expected));
}

#[tokio::test]
#[serial]
async fn multipart_part_failure_aborts_the_upload() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    let dir = tempfile::tempdir().unwrap();
    let path = write_patterned_file(&dir, "large.bin", MULTIPART_FILE_SIZE);
    state.lock().unwrap().failures.fail_part_number = Some(2);

    let result = storage.put_from_path("large", &path).await;
    assert!(result.is_err(), "a failed part must fail the upload");

    let state = state.lock().unwrap();
    assert_eq!(state.aborts.len(), 1, "the failed upload must be aborted");
    assert!(!state.objects.contains_key("large"));
    assert!(state.multipart.is_empty());
}

#[tokio::test]
#[serial]
async fn multipart_complete_failure_aborts_the_upload() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    let dir = tempfile::tempdir().unwrap();
    let path = write_patterned_file(&dir, "large.bin", MULTIPART_FILE_SIZE);
    state.lock().unwrap().failures.fail_complete = true;

    let result = storage.put_from_path("large", &path).await;
    assert!(result.is_err(), "a failed complete must fail the upload");

    let state = state.lock().unwrap();
    assert_eq!(state.aborts.len(), 1, "the failed upload must be aborted");
    assert!(!state.objects.contains_key("large"));
}

#[tokio::test]
#[serial]
async fn multipart_initiate_failure_surfaces_as_error() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    let dir = tempfile::tempdir().unwrap();
    let path = write_patterned_file(&dir, "large.bin", MULTIPART_FILE_SIZE);
    state.lock().unwrap().failures.fail_initiate = true;

    let result = storage.put_from_path("large", &path).await;
    assert!(result.is_err(), "a failed initiate must fail the upload");

    let state = state.lock().unwrap();
    assert!(
        state.aborts.is_empty(),
        "nothing was initiated, nothing to abort"
    );
    assert!(state.multipart.is_empty());
}

#[tokio::test]
#[serial]
async fn download_to_path_streams_to_a_nested_destination() {
    let _creds = dummy_aws_credentials();
    let (endpoint, _state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    storage
        .put("obj", Bytes::from_static(b"streamed payload"))
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("nested/sub/obj.bin");
    storage.download_to_path("obj", &dest).await.unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"streamed payload");
    // Only the final file remains — no temp .part leftovers.
    assert_eq!(
        std::fs::read_dir(dest.parent().unwrap()).unwrap().count(),
        1
    );
}

#[tokio::test]
#[serial]
async fn download_to_path_missing_key_leaves_no_file_behind() {
    let _creds = dummy_aws_credentials();
    let (endpoint, _state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("missing.bin");
    assert!(matches!(
        storage.download_to_path("missing", &dest).await,
        Err(StorageError::NotFound(_))
    ));
    assert!(!dest.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
#[serial]
async fn head_forbidden_surfaces_as_internal_not_not_found() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = make_storage(&endpoint).await;

    state.lock().unwrap().failures.head_forbidden = true;

    let error = storage.exists("obj").await.unwrap_err();
    assert!(
        matches!(error, StorageError::Internal { .. }),
        "unexpected error: {error}"
    );
    assert!(
        std::error::Error::source(&error).is_some(),
        "S3 SDK error source should be preserved"
    );
}

#[tokio::test]
#[serial]
async fn dropping_storage_aborts_inflight_multipart_uploads() {
    let _creds = dummy_aws_credentials();
    let (endpoint, state) = start_mock_s3().await;
    let storage = Arc::new(make_storage(&endpoint).await);

    // Park part uploads so the multipart transfer stays in flight.
    state.lock().unwrap().block_parts = true;

    let dir = tempfile::tempdir().unwrap();
    let path = write_patterned_file(&dir, "large.bin", MULTIPART_FILE_SIZE);

    let task = {
        let storage = storage.clone();
        tokio::spawn(async move { storage.put_from_path("inflight", &path).await })
    };

    // Wait for the upload to be initiated.
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.lock().unwrap().initiates == 0 {
        assert!(
            Instant::now() < deadline,
            "multipart upload never initiated"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Release the last references: dropping S3Storage must abort the
    // in-flight upload even though the transfer is still parked.
    drop(storage);
    task.abort();

    let deadline = Instant::now() + Duration::from_secs(10);
    while state.lock().unwrap().aborts.is_empty() {
        assert!(Instant::now() < deadline, "drop never aborted the upload");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Let the parked part handler finish so the mock server can shut down.
    state.lock().unwrap().unblock = true;
}
