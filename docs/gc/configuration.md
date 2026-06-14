# GC Configuration Reference

This document lists all environment variables that control the garbage
collection system. All GC configuration uses the `GC_*` prefix.

---

## Table of Contents

1. [Quick Start](#quick-start)
2. [Core Settings](#core-settings)
3. [Bloom Filter Settings](#bloom-filter-settings)
4. [Scanner Settings](#scanner-settings)
5. [Grace Period Settings](#grace-period-settings)
6. [Lease Settings](#lease-settings)
7. [Reference Tracker Settings](#reference-tracker-settings)
8. [Deletion Settings](#deletion-settings)
9. [Dry-Run Mode](#dry-run-mode)
10. [Legacy Settings](#legacy-settings)
11. [Tuning Recommendations](#tuning-recommendations)

---

## Quick Start

Minimal configuration to enable incremental GC:

```bash
# Enable GC with safe defaults
GC_ENABLED=true
GC_DRY_RUN=true              # Start with dry-run to verify behavior

# Hub connection (for reference hash queries)
GC_HUB_BASE_URL=https://hub.internal.example.com:8080
GC_HUB_INTERNAL_TOKEN=your-internal-token

# Optional: tune for your scale
GC_BLOOM_EXPECTED_ITEMS=10000000   # ~10M chunks
GC_GRACE_ABSOLUTE_SECONDS=3600     # 1 hour absolute protection
```

After verifying dry-run output is correct, set `GC_DRY_RUN=false` to enable
actual deletion.

---

## Core Settings

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_ENABLED` | bool | `false` | Master switch. When `false`, no GC background task starts. |
| `GC_INTERVAL_SECONDS` | u64 | `3600` | Seconds between GC cycle starts. The next cycle begins after this delay regardless of whether the previous cycle completed. Set to at least 2x the expected cycle duration. |
| `GC_DATA_DIR` | string | `/var/lib/cas/gc` | Local working directory for GC state files (Bloom filter, checkpoint, lease). Must be writable by the CAS process. On S3 backend, this is the local cache directory. |
| `GC_DRY_RUN` | bool | `true` | When `true`, GC runs the full cycle but skips the deletion phase. Logs what would be deleted. Use this to verify correctness before enabling actual deletion. |

### Validation

When `GC_ENABLED=true`:

- `GC_INTERVAL_SECONDS` must be > 0 (panic on startup).
- `GC_BLOOM_EXPECTED_ITEMS` must be > 0 (panic on startup).
- `GC_BLOOM_FALSE_POSITIVE_RATE` must be in (0.0, 1.0) exclusive (panic).
- `GC_DELETE_BATCH_SIZE` must be > 0 (panic).
- If both `GC_GRACE_ABSOLUTE_SECONDS` and `GC_GRACE_SOFT_CYCLES` are 0, a
  warning is logged (no grace period protection).

---

## Bloom Filter Settings

The Bloom filter provides O(1) probabilistic membership testing for chunk
hashes, avoiding the need to keep all referenced hashes in memory.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_BLOOM_EXPECTED_ITEMS` | u64 | `10000000` | Expected number of distinct chunk hashes. This sizes the underlying bit array. Set this to approximately the total number of unique chunk hashes across all shards in your deployment. |
| `GC_BLOOM_FALSE_POSITIVE_RATE` | f64 | `0.001` | Acceptable false positive rate (0.0-1.0). Lower values use more memory. 0.001 means ~0.1% of orphaned blobs will be falsely retained (wasted space, never data loss). |
| `GC_BLOOM_REBUILD_THRESHOLD` | f64 | `0.8` | When the Bloom filter's occupancy reaches this fraction of its capacity, a background rebuild is triggered. The rebuild creates a new filter with fresh sizing. |

### Memory Estimation

The memory used by the Bloom filter is determined by:

```
m = -n * ln(p) / (ln(2))^2

where:
  n = GC_BLOOM_EXPECTED_ITEMS
  p = GC_BLOOM_FALSE_POSITIVE_RATE
  m = number of bits

Example (defaults):
  n = 10,000,000
  p = 0.001
  m = 143,775,000 bits ≈ 17.1 MB
```

| Expected Items | FPR | Memory |
|---------------|-----|--------|
| 1,000,000 | 0.001 | ~1.7 MB |
| 10,000,000 | 0.001 | ~17 MB |
| 100,000,000 | 0.001 | ~170 MB |
| 10,000,000 | 0.0001 | ~23 MB |

### When to Adjust

- **Increase `GC_BLOOM_EXPECTED_ITEMS`** if your deployment has more than 10M
  unique chunk hashes. Under-sizing causes more frequent rebuilds.
- **Decrease `GC_BLOOM_FALSE_POSITIVE_RATE`** if you want fewer orphaned blobs
  retained. Going below 0.001 has diminishing returns on memory.
- **Increase `GC_BLOOM_REBUILD_THRESHOLD`** to rebuild less frequently (trade
  higher false-positive rate near end-of-life for fewer rebuilds).

---

## Scanner Settings

The incremental scanner walks storage in pages, checkpointing progress to
enable crash recovery.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_SCANNER_PAGE_SIZE` | usize | `1000` | Number of objects per listing page. Larger pages reduce S3 API calls but increase memory for page buffers. |
| `GC_SCANNER_CHECKPOINT_INTERVAL` | u64 | `10000` | Number of objects processed between checkpoint saves. More frequent checkpoints reduce re-scan work after a crash but increase S3 PUT calls. |
| `GC_SCANNER_MAX_DURATION_SECONDS` | u64 | `1800` | Maximum wall-clock seconds for a single scan pass (30 minutes). Prevents the scanner from running indefinitely on very large stores. When the limit is reached, the checkpoint is saved and the scan exits. The next cycle resumes from the saved position. |

### When to Adjust

- **Increase `GC_SCANNER_PAGE_SIZE`** for S3 backends with high latency per
  request. 1000 is a good balance; going above 5000 has diminishing returns.
- **Decrease `GC_SCANNER_CHECKPOINT_INTERVAL`** if your GC cycles are
  frequently interrupted and you want less re-scan work. Each checkpoint save
  is an S3 PUT (~$0.005 per 1000 PUTs).
- **Increase `GC_SCANNER_MAX_DURATION_SECONDS`** if your store is very large
  and a single scan pass needs more than 30 minutes to complete. The default
  of 30 minutes is sufficient for ~1M shards at typical S3 throughput.

---

## Grace Period Settings

The two-tier grace period prevents premature deletion of recently uploaded
blobs.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_GRACE_ABSOLUTE_SECONDS` | u64 | `3600` | Absolute minimum age (seconds) before a blob can be deleted. Any blob younger than this is unconditionally protected, regardless of reference status. |
| `GC_GRACE_SOFT_CYCLES` | u32 | `2` | Number of consecutive GC scans in which a blob must be observed as unreferenced before it becomes eligible for deletion. This protects against eventual consistency delays. |
| `GC_GRACE_PERIOD_SECONDS` | u64 | `600` | **Legacy** flat grace period. Used by the old GC implementation. New code uses `GC_GRACE_ABSOLUTE_SECONDS` instead. |

### Interaction

Both tiers must pass for a blob to be deleted:

```
blob.age >= GC_GRACE_ABSOLUTE_SECONDS
    AND blob.unreferenced_scan_count >= GC_GRACE_SOFT_CYCLES
```

### When to Adjust

- **Increase `GC_GRACE_ABSOLUTE_SECONDS`** if you observe race conditions where
  blobs are deleted before their shard references are fully written. The
  default of 1 hour is generous; most deployments can reduce to 10-15 minutes.
- **Increase `GC_GRACE_SOFT_CYCLES`** if your S3 backend has significant
  eventual consistency delays (rare with modern S3). The default of 2 means a
  blob must be unreferenced in 2 consecutive scans (i.e., at least 2 GC
  intervals old as seen by the scanner).
- **Set both to 0** only for testing. This disables all grace period protection
  and may cause data loss under concurrent upload/GC conditions.

---

## Lease Settings

Controls multi-node GC coordination via S3-based leasing.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_LEASE_TTL_SECONDS` | u64 | `3600` | Time-to-live for the GC lease. The lease must be renewed before it expires. Default 1 hour. |
| `GC_LEASE_RENEW_INTERVAL_SECONDS` | u64 | `600` | How often the lease holder renews the lease. Must be substantially less than `GC_LEASE_TTL_SECONDS` to survive transient pauses. Default 10 minutes. |

### When to Adjust

- **Increase `GC_LEASE_TTL_SECONDS`** if your GC cycles take longer than 1 hour.
  The TTL should be at least 2x the expected cycle duration.
- **Decrease `GC_LEASE_RENEW_INTERVAL_SECONDS`** if you want faster failover
  after a lease holder crash. However, more frequent renewals mean more S3 PUTs.
- Ensure `GC_LEASE_RENEW_INTERVAL_SECONDS` < `GC_LEASE_TTL_SECONDS` / 3 to
  tolerate at least 2 missed renewals before the lease expires.

---

## Reference Tracker Settings

Controls how shard-to-chunk reference mappings are stored and retrieved.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_REFERENCE_TRACKER_MODE` | string | `sidecar` | Reference tracking implementation. `sidecar` writes a `.refs.json` file alongside each shard in S3. `local_cache_db` uses a local SQLite database. |
| `GC_LOCAL_CACHE_DB_PATH` | string | `/var/lib/cas/gc/refs.db` | Path to the SQLite database when `GC_REFERENCE_TRACKER_MODE=local_cache_db`. Must be writable by the CAS process. |

### Sidecar Mode (Default, Recommended for S3)

Each shard upload atomically writes a sidecar file containing the shard's
reference set. GC reads these files during the scan phase. This mode works
with any S3-compatible backend and requires no local state.

### Local Cache DB Mode (Recommended for Local Storage)

A SQLite database maintains a cache of shard-to-hash mappings. This avoids
the overhead of reading sidecar files from local disk and is faster for
deployments using local storage.

---

## Deletion Settings

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_DELETE_BATCH_SIZE` | usize | `100` | Maximum number of blobs to delete in a single GC cycle. Limits per-cycle I/O impact. Remaining orphans are deleted in subsequent cycles. |
| `GC_DELETE_MAX_RETRIES` | u32 | `3` | Maximum retry attempts for a failed blob deletion before giving up. Uses exponential backoff between retries. |

### When to Adjust

- **Increase `GC_DELETE_BATCH_SIZE`** if you have a large backlog of orphans
  and want to clear them faster. Be aware that larger batches increase the
  duration of the deletion phase and the blast radius of any bugs.
- **Increase `GC_DELETE_MAX_RETRIES`** if your S3 backend has transient errors
  that cause deletion failures. The default of 3 with exponential backoff
  (1s, 2s, 4s) handles most transient failures.

---

## Dry-Run Mode

When `GC_DRY_RUN=true`, the GC system runs the complete cycle (lease
acquisition, scan, candidate computation) but skips the deletion phase.
Instead, it logs the candidates that would be deleted:

```
GC dry_run: would delete 42 LFS blobs, 17 xorbs
```

Dry-run mode is essential for:

1. **Initial deployment verification:** Confirm that the Bloom filter is
   correctly populated and the candidate set looks reasonable.
2. **Post-migration validation:** After running the sidecar migration script,
   verify that the candidate set is consistent with the old GC's output.
3. **Debugging:** When investigating unexpected deletions, enable dry-run
   to see what the current cycle would delete without actually deleting.

### Enabling Actual Deletion

To transition from dry-run to actual deletion:

1. Verify dry-run output for at least 2-3 GC cycles.
2. Check that `gc_sidecar_missing_total` is low (migration is complete).
3. Check that `gc_bloom_hits_total` / `gc_bloom_queries_total` is reasonable.
4. Set `GC_DRY_RUN=false`.
5. Monitor `gc_blobs_deleted_total` and `gc_bytes_freed_total` closely.

---

## Legacy Settings

These environment variables are used by the legacy GC implementation in
`src/gc/mod.rs`. They will be removed in a future release once the
incremental GC fully replaces the legacy path.

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `GC_HUB_BASE_URL` | string | `http://localhost:8080` | Hub API base URL for `/internal/referenced-hashes` endpoint. |
| `GC_HUB_INTERNAL_TOKEN` | string | (empty) | Authentication token for Hub's internal endpoints. |
| `GC_HTTP_TIMEOUT_SECONDS` | u64 | `300` | HTTP timeout for GC requests to Hub. |
| `GC_GRACE_PERIOD_SECONDS` | u64 | `600` | Legacy flat grace period (superseded by `GC_GRACE_ABSOLUTE_SECONDS`). |

### Deprecation Notice

The incremental GC eliminates the need for Hub API calls by using sidecar
files for reference tracking. Once the incremental GC is fully deployed:

- `GC_HUB_BASE_URL`, `GC_HUB_INTERNAL_TOKEN`, and `GC_HTTP_TIMEOUT_SECONDS`
  will be removed.
- `GC_GRACE_PERIOD_SECONDS` will be removed (use `GC_GRACE_ABSOLUTE_SECONDS`).

---

## Tuning Recommendations

### Small Deployment (< 1M chunks)

```bash
GC_ENABLED=true
GC_DRY_RUN=false
GC_BLOOM_EXPECTED_ITEMS=1000000
GC_BLOOM_FALSE_POSITIVE_RATE=0.001
GC_SCANNER_PAGE_SIZE=500
GC_SCANNER_CHECKPOINT_INTERVAL=5000
GC_INTERVAL_SECONDS=3600
GC_GRACE_ABSOLUTE_SECONDS=600
GC_GRACE_SOFT_CYCLES=2
GC_DELETE_BATCH_SIZE=50
```

Expected memory: ~2 MB for Bloom filter.

### Medium Deployment (1M - 10M chunks)

```bash
GC_ENABLED=true
GC_DRY_RUN=false
GC_BLOOM_EXPECTED_ITEMS=10000000
GC_BLOOM_FALSE_POSITIVE_RATE=0.001
GC_SCANNER_PAGE_SIZE=1000
GC_SCANNER_CHECKPOINT_INTERVAL=10000
GC_INTERVAL_SECONDS=3600
GC_GRACE_ABSOLUTE_SECONDS=3600
GC_GRACE_SOFT_CYCLES=2
GC_DELETE_BATCH_SIZE=100
```

Expected memory: ~17 MB for Bloom filter.

### Large Deployment (10M - 100M chunks)

```bash
GC_ENABLED=true
GC_DRY_RUN=false
GC_BLOOM_EXPECTED_ITEMS=100000000
GC_BLOOM_FALSE_POSITIVE_RATE=0.001
GC_SCANNER_PAGE_SIZE=2000
GC_SCANNER_CHECKPOINT_INTERVAL=20000
GC_SCANNER_MAX_DURATION_SECONDS=3600
GC_INTERVAL_SECONDS=7200
GC_GRACE_ABSOLUTE_SECONDS=3600
GC_GRACE_SOFT_CYCLES=3
GC_DELETE_BATCH_SIZE=500
GC_DELETE_MAX_RETRIES=5
```

Expected memory: ~170 MB for Bloom filter. Consider running GC less frequently
(every 2 hours) to reduce S3 API costs.

### Multi-Node Deployment

```bash
# Same as above, plus:
GC_LEASE_TTL_SECONDS=7200
GC_LEASE_RENEW_INTERVAL_SECONDS=600
```

Ensure each CAS node has a unique node ID (auto-generated from hostname or
configured explicitly).
