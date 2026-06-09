# Metrics Dead Code Fix Design

## Problem Statement

Code review identified that two metrics API methods exist but are never called:
- `connection_opened()` and `connection_closed()` — `active_connections` metric always 0
- `record_download_bytes()` — `download_bytes_total` metric always 0

These methods are tested in unit tests but have no production usage.

## Solution Overview

### 1. Connection Tracking via Middleware

**Architecture:**
```
Client Request → Metrics Middleware → Route Handler → Response
                     ↓
              GLOBAL_METRICS.connection_opened()
                     ↓
              [handler executes]
                     ↓
              GLOBAL_METRICS.connection_closed()
```

**Implementation:**
- New module: `src/middleware.rs`
- Function signature:
  ```rust
  pub async fn metrics_middleware(
      req: ServiceRequest,
      next: Next<impl MessageBody>,
  ) -> Result<HttpResponse, Error>
  ```

**Behavior:**
- Call `connection_opened()` when request arrives
- Execute handler via `next.call(req).await`
- Call `connection_closed()` after handler completes (success or error)
- Use RAII guard pattern to ensure `connection_closed()` is always called

**Integration:**
- Register in `server.rs` using `.wrap(middleware::from_fn(metrics_middleware))`
- Applies to all routes automatically
- Order: Add after `Logger::default()` wrapper

### 2. Download Byte Tracking in Reconstruction

**Scope:**
- `get_reconstruction_v1()` in `src/api/reconstruction.rs`
- `get_reconstruction()` in `src/api/reconstruction.rs`

**V2 Implementation:**
```rust
// After building response, before recording metrics
let total_download_bytes: u64 = xorbs.iter()
    .map(|x| x.size)
    .sum();

GLOBAL_METRICS.record_download_bytes(total_download_bytes);
```

**V1 Implementation:**
Add `size: u64` field to `XorbInfoV1` struct, populated from `xorb_entry.num_bytes_in_xorb`. Sum the sizes for `record_download_bytes()`. This is a non-breaking additive change to the response structure and resolves the existing `// TODO: Populate with actual chunk info` gap at the xorb level.

**Error Handling:**
- Only record on successful responses (200 OK)
- Zero-byte tracking is valid (empty file reconstruction)
- Don't record on 400/404/500 errors

## File Changes

### New Files
1. `src/middleware.rs` — metrics middleware implementation

### Modified Files
1. `src/server.rs` — register middleware
2. `src/api/reconstruction.rs` — add download byte tracking to both endpoints
3. `src/api/reconstruction.rs` — add xorb size field to V1 response for tracking
4. `src/lib.rs` — export middleware module

### Test Files
1. `tests/test_middleware.rs` — integration test for connection tracking
2. Update `tests/test_metrics.rs` — add download byte tracking test

## Testing Strategy

### Unit Tests
- Middleware: Verify connection count increments/decrements
- Download tracking: Verify byte sum calculation

### Integration Tests
1. **Connection tracking test:**
   - Make multiple concurrent requests
   - Verify `active_connections` gauge reflects concurrent count
   - Verify returns to 0 after requests complete

2. **Download byte tracking test:**
   - Call reconstruction endpoint with known xorb sizes
   - Verify `download_bytes_total` increments by expected sum
   - Verify no increment on 404 error

### Test Considerations
- Use `#[serial]` attribute for tests using global metrics
- Reset metrics between tests if possible (or use delta checking)

## Success Criteria

1. `active_connections` metric shows non-zero values during concurrent requests
2. `download_bytes_total` metric increments by xorb sizes on successful reconstruction
3. All existing tests continue to pass
4. New tests verify both features work correctly
5. No performance degradation from middleware overhead

## Non-Goals

- Rate limiting on /metrics endpoint (separate concern)
- Per-route metrics breakdown (future enhancement)
- Connection duration tracking (only count, not time)
