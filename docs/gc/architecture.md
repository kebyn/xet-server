# Incremental Garbage Collection Architecture

This document describes the architecture of the incremental GC system in Xet CAS,
designed to replace the legacy Hub-dependent GC with a fully self-contained,
incremental, multi-node-safe garbage collector.

---

## Table of Contents

1. [Design Goals](#design-goals)
2. [System Overview](#system-overview)
3. [Five-Phase GC Cycle](#five-phase-gc-cycle)
4. [Component Reference](#component-reference)
5. [Component Interactions](#component-interactions)
6. [Multi-Node Coordination](#multi-node-coordination)
7. [Sidecar File Pattern](#sidecar-file-pattern)
8. [Two-Tier Grace Period](#two-tier-grace-period)
9. [Crash Recovery via Checkpoint](#crash-recovery-via-checkpoint)
10. [Safety Invariants](#safety-invariants)
11. [Observability](#observability)

---

## Design Goals

| Goal | Legacy GC | Incremental GC |
|------|-----------|----------------|
| Hub dependency | Requires `/internal/referenced-hashes` | Fully self-contained |
| Multi-node safety | Race conditions on concurrent GC | S3-based lease coordination |
| Performance | Full scan every cycle (O(N)) | Incremental scan (O(delta)) |
| Memory footprint | All hashes in RAM | Bloom filter (~17 MB for 10M items) |
| Crash recovery | Restart from scratch | Resume from checkpoint |
| Reference tracking | Hub queries on every run | Sidecar files on every upload |

> **Note:** The legacy Hub `/internal/referenced-hashes` endpoint has been
> removed. The incremental GC is fully self-contained with no Hub dependency.

---

## System Overview

```
┌─────────────────────────────────────────────────────────────────────────┐
│                      Incremental GC System                              │
│                                                                         │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐     │
│  │  Bloom Filter    │  │  Incremental     │  │  Reference       │     │
│  │  Protected Set   │◄─┤  Scanner +       │◄─┤  Tracker         │     │
│  │  (src/gc/bloom)  │  │  Checkpoint      │  │  (S3 / Local)    │     │
│  └──────────────────┘  │  (src/gc/scanner)│  └──────────────────┘     │
│           ↑             └──────────────────┘            ↑              │
│           │                     ↑                       │              │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐     │
│  │  Grace Period    │  │  GcCoordinator   │  │  GcRunner        │     │
│  │  (2-tier)        │  │  (Lease Mgmt)    │  │  (5-phase cycle) │     │
│  │  (src/gc/grace)  │  │  (src/gc/coord.) │  │  (src/gc/mod)    │     │
│  └──────────────────┘  └──────────────────┘  └──────────────────┘     │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### Storage Layout

```
s3://bucket/  (or local storage root/)
├── shards/                       Shard files (xorb+LFS references)
├── shard_refs/                   Sidecar reference files (*.refs.json)
├── xorbs/                        Deduplicated xorb blobs
├── lfs/objects/                  Raw LFS blobs (pre-conversion)
└── .gc/                          GC internal state
    ├── checkpoint.json           Scanner position + progress counters
    ├── bloom.bin                 Persisted Bloom filter (CRC32-prefixed)
    └── lease.json                Multi-node lease lock file
```

---

## Five-Phase GC Cycle

The `GcRunner` (in `src/gc/mod.rs`) executes each GC cycle in five sequential
phases. Each phase is independent and can be retried or skipped based on the
state of the previous phase.

### Phase 1: Acquire Lease

```
Purpose:  Ensure only one node runs GC at a time.
Input:    S3 storage, node ID, lease TTL.
Output:   Lease guard (auto-renews in background).
Failure:  Skip this GC cycle (return immediately).
```

The `GcCoordinator` attempts a conditional PUT on `.gc/lease.json`. If the
existing lease is held by another node and has not expired, the current node
yields. On success, a background task starts renewing the lease every
`GC_LEASE_RENEW_INTERVAL_SECONDS` (default 600 s) until the GC cycle completes.

### Phase 2: Load Checkpoint + Bloom Filter

```
Purpose:  Resume scanning from where the previous cycle left off.
Input:    .gc/checkpoint.json, .gc/bloom.bin
Output:   Hydrated BloomFilterProtectedSet, GcCheckpoint with cursor.
Failure:  If Bloom filter CRC32 check fails, create a fresh filter and
          reset the checkpoint to scan from the beginning.
```

Both files are CRC32-integrity-checked on load. If either is corrupted, the
system degrades gracefully: it creates a fresh Bloom filter and restarts the
scan from the beginning. This is safe because a fresh Bloom filter means no
items are protected, which means nothing will be deleted (the Bloom filter
starts empty and items are added as they are scanned).

### Phase 3: Incremental Scan

```
Purpose:  Walk new/updated shards and populate the Bloom filter with
          all referenced chunk hashes.
Input:    Storage listing, checkpoint cursor, sidecar files.
Output:   Updated Bloom filter, advanced checkpoint cursor.
Failure:  If scan times out (GC_SCANNER_MAX_DURATION_SECONDS), save the
          checkpoint and exit. Next cycle resumes from this position.
```

The `IncrementalScanner` lists shards in pages of `GC_SCANNER_PAGE_SIZE`
(default 1000). For each shard:

1. Read the sidecar file `shard_refs/{hash}.refs.json`.
2. If the sidecar is missing, parse the shard directly (fallback).
3. Insert all referenced xorb and LFS hashes into the Bloom filter.
4. Every `GC_SCANNER_CHECKPOINT_INTERVAL` (default 10000) objects, persist
   the checkpoint with the current S3 cursor position.

**Three-layer sidecar defense:**

| Layer | Strategy | Trigger |
|-------|----------|---------|
| 1 | Read sidecar file | Normal path |
| 2 | Parse shard directly | Sidecar missing |
| 3 | Conservative: treat references as unknown | Parse fails |

When layer 3 fires, the affected hashes are NOT inserted into the Bloom filter.
Because the Bloom filter only protects items that ARE present, a missing entry
means the item might be deleted. To prevent false deletion, layer 3 causes the
scanner to skip deletion of related xorbs in this cycle (the safety invariant).

### Phase 4: Compute Candidates

```
Purpose:  Identify blobs that are not protected by the Bloom filter and
          pass both grace period checks.
Input:    Full storage listing, populated Bloom filter, grace period state.
Output:   Candidate deletion set (LFS blobs + xorbs).
Failure:  If degraded scan occurred (any layer-3 fallback), abort
          deletion for safety.
```

For each blob in storage:

1. Query the Bloom filter. If `contains(hash)` returns `true`, the blob is
   protected; skip it. (With false-positive rate 0.001, ~0.1% of orphaned
   blobs will be falsely retained.)
2. If `false` (definitely not referenced), check the grace period:
   a. **Absolute grace**: blob age < `GC_GRACE_ABSOLUTE_SECONDS` (default 3600)?
      If yes, skip (never delete).
   b. **Soft grace**: blob has been observed as unreferenced in fewer than
      `GC_GRACE_SOFT_CYCLES` (default 2) consecutive scans? If yes, skip.
3. Blob passes both checks; add to candidate set.

### Phase 5: Delete + Cleanup

```
Purpose:  Remove candidate blobs and persist state for the next cycle.
Input:    Candidate set, lease guard, Bloom filter, checkpoint.
Output:   Deletion complete, state persisted, lease released.
Failure:  Individual deletion errors are logged and retried up to
          GC_DELETE_MAX_RETRIES times. Lease is always released.
```

Deletion proceeds in batches of `GC_DELETE_BATCH_SIZE` (default 100) to limit
per-cycle I/O impact. Before each deletion, the grace period is re-checked
(fresh `mtime` lookup) to prevent race conditions where a blob was uploaded
between the scan and delete phases.

After all candidates are processed (or the batch limit is reached):

1. Persist the updated Bloom filter to `.gc/bloom.bin` (atomic: write to
   temp file, then rename).
2. Persist the final checkpoint to `.gc/checkpoint.json`.
3. Release the lease.

---

## Component Reference

### BloomFilterProtectedSet (`src/gc/bloom.rs`)

A probabilistic set that answers "have we seen this hash?" with configurable
false-positive rate. Uses double-buffering: when the active filter reaches
`GC_BLOOM_REBUILD_THRESHOLD` (default 80%) of capacity, a new filter is built
in the background while the active one continues serving queries.

| Property | Default | Description |
|----------|---------|-------------|
| `expected_items` | 10,000,000 | Sizing parameter for the filter |
| `false_positive_rate` | 0.001 (0.1%) | Probability of false positive |
| `rebuild_threshold` | 0.8 | Occupancy fraction that triggers rebuild |
| Memory usage | ~17-20 MB | For default configuration |
| Query latency | < 1 microsecond | Constant-time regardless of size |

**Persistence format:** `[CRC32: 4 bytes][bincode-serialized Bloom filter]`

**Key invariant:** A hash NOT in the Bloom filter is definitely not referenced.
A hash IN the Bloom filter MIGHT be referenced (with probability 1 - FPR).
The GC never deletes a blob that is in the Bloom filter.

### IncrementalScanner (`src/gc/scanner.rs`)

Walks storage in pages, reading sidecar files to populate the Bloom filter.
Maintains a cursor (S3 continuation token or local offset) in the checkpoint
so that successive cycles only scan new/updated objects.

| Property | Default | Description |
|----------|---------|-------------|
| `page_size` | 1000 | Objects per listing page |
| `checkpoint_interval` | 10,000 | Objects between checkpoint saves |
| `max_duration_seconds` | 1800 (30 min) | Maximum time for one scan pass |

### GcCheckpoint (`src/gc/checkpoint.rs`)

Tracks the scanner's position within the storage listing. On crash or restart,
the scanner resumes from the saved cursor instead of starting over.

Fields:

- `version` - Schema version for forward compatibility.
- `last_scan_at` - Timestamp of last successful scan.
- `s3_cursor` - Continuation token for paginated listing.
- `shards_scanned`, `xorbs_scanned`, `lfs_blobs_scanned` - Progress counters.
- `cycle_started_at` - When the current GC cycle began.
- `status` - `InProgress | Completed | Failed(reason)`.
- `crc32` - Integrity checksum.

### GcCoordinator (`src/gc/coordinator.rs`)

Manages the multi-node lease using S3 conditional PUTs. Only one node holds the
lease at a time; other nodes skip their GC cycle and retry at the next interval.

| Property | Default | Description |
|----------|---------|-------------|
| `ttl_seconds` | 3600 (1 hour) | Lease lifetime |
| `renew_interval_seconds` | 600 (10 min) | Background renewal period |

### ReferenceTracker (`src/gc/reference_tracker/`)

Records which chunk hashes are referenced by each shard. Two implementations:

**S3 (sidecar):** Each shard upload atomically writes a sidecar file
`shard_refs/{shard_hash}.refs.json` containing the referenced xorb and LFS
hashes. GC reads these files during scan.

**Local (SQLite):** A local database at `GC_LOCAL_CACHE_DB_PATH`
(default `/var/lib/cas/gc/refs.db`) caches shard-to-hash mappings. Updated
incrementally on each shard upload. Used when storage backend is local.

### GracePeriod (`src/gc/grace.rs`)

Two-tier protection against premature deletion:

| Tier | Env Var | Default | Semantics |
|------|---------|---------|-----------|
| Absolute | `GC_GRACE_ABSOLUTE_SECONDS` | 3600 (1 h) | Blobs younger than this are never deleted |
| Soft | `GC_GRACE_SOFT_CYCLES` | 2 | Blobs must survive N consecutive scans as unreferenced |

The absolute tier uses wall-clock age (`now - mtime`). The soft tier tracks
unreferenced observation counts across GC cycles. A blob must pass BOTH tiers
before deletion.

---

## Component Interactions

The following sequence shows a complete GC cycle:

```
GcRunner                     GcCoordinator
   │                              │
   ├──── try_acquire_lease() ────►│──► S3 PUT .gc/lease.json (conditional)
   │◄─── Ok(LeaseGuard) ─────────┤
   │                              │──► start background lease renewal
   │
   ├──── load_checkpoint() ───────► Checkpoint file
   ├──── load_bloom() ────────────► Bloom filter file
   │
   │                          IncrementalScanner
   ├──── scan() ──────────────────►│
   │                              │──► list shards (paged, from cursor)
   │                              │──► for each shard:
   │                              │     ├─ read sidecar (ReferenceTracker)
   │                              │     ├─ insert refs into BloomFilterProtectedSet
   │                              │     └─ checkpoint every N objects
   │◄─── ScanResult ──────────────┤
   │
   │                          BloomFilterProtectedSet
   ├──── compute_candidates() ────►│
   │  for each blob in storage:   │
   │     if !bloom.contains(hash) │──► O(1) lookup
   │        && grace_period_ok()  │
   │     → candidate              │
   │◄─── Vec<candidate> ──────────┤
   │
   ├──── delete_candidates() ─────► S3 DELETE / local unlink
   │     (batched, retried)       │
   │     (re-check grace before   │
   │      each deletion)          │
   │
   ├──── save_bloom() ────────────► .gc/bloom.bin (atomic write)
   ├──── save_checkpoint() ───────► .gc/checkpoint.json (atomic write)
   │
   ├──── release_lease() ─────────►│──► S3 DELETE .gc/lease.json
   │                              │──► stop background renewal
   │
   └──── done
```

---

## Multi-Node Coordination

### Problem

When multiple CAS nodes share the same storage backend, concurrent GC runs can
cause race conditions: two nodes might decide the same blob is orphaned and
attempt to delete it simultaneously, or one node might delete a blob that
another node's scan has not yet evaluated.

### Solution: S3-based Lease

The `GcCoordinator` uses a lease file (`.gc/lease.json`) in the storage backend
to ensure mutual exclusion:

```json
{
  "holder_node_id": "node-abc-123",
  "expires_at": "2026-06-14T11:00:00Z",
  "acquired_at": "2026-06-14T10:00:00Z"
}
```

**Acquisition protocol:**

1. Read existing lease (if any) from `.gc/lease.json`.
2. If the lease exists and is held by another node AND has not expired,
   return `None` (yield to the holder).
3. Attempt a conditional PUT: write the new lease only if the existing
   lease matches what was read in step 1 (optimistic concurrency control
   via `put_if_lease_matches()`).
4. On success, return a `LeaseGuard` that auto-renews in the background.
5. On `ConditionFailed` (another node wrote first), return `None`.

**Lease renewal:** A background Tokio task wakes every
`GC_LEASE_RENEW_INTERVAL_SECONDS` (default 10 min) and extends the lease
expiration by `GC_LEASE_TTL_SECONDS` (default 1 h). This ensures the lease
survives transient network issues.

**Lease release:** When the GC cycle completes, the `LeaseGuard` is dropped,
which deletes `.gc/lease.json`. If the node crashes, the lease expires
automatically after `GC_LEASE_TTL_SECONDS`.

**Crash recovery:** When a new leader acquires an expired lease, it loads the
previous checkpoint (which may have been written by the crashed node) and
continues scanning from the saved cursor. This ensures no work is lost.

### No External Dependencies

Unlike solutions using Redis, etcd, or DynamoDB, the S3-based lease requires
no additional infrastructure. The conditional PUT is implemented via S3's
`If-Match` / `If-None-Match` headers (ETag-based optimistic concurrency).

---

## Sidecar File Pattern

### Problem

S3 objects have a 2 KB metadata limit, which is insufficient to store the
list of chunk hashes referenced by a shard (a shard can reference thousands
of chunks). An external reference tracking mechanism is needed.

### Solution: Sidecar Files

When a shard is uploaded, the upload hook (`src/conversion/mod.rs`) atomically
writes a sidecar file containing the shard's reference set:

```
shard_refs/
└── {shard_hash}.refs.json
```

**Sidecar file format:**

```json
{
  "version": 1,
  "shard_hash": "abc123def456...",
  "lfs_refs": ["oid1", "oid2", "oid3"],
  "xorb_refs": ["xorb_hash_1", "xorb_hash_2"],
  "created_at": "2026-06-14T10:00:00Z"
}
```

**Upload flow (atomicity):**

1. Parse the shard to extract all xorb and LFS references.
2. Write the sidecar file to `shard_refs/{hash}.refs.json`.
3. Upload the shard to `shards/{hash}`.
4. If step 3 fails, the sidecar is orphaned but harmless (GC ignores
   sidecars without corresponding shards).

This ordering ensures that when a shard exists in storage, its sidecar always
exists too. The reverse is not required; orphaned sidecars are harmless.

### Migration for Historical Shards

Shards uploaded before the incremental GC was deployed will not have sidecar
files. The migration script (`tools/migrate_gc_sidecars`) backfills sidecars
by scanning all existing shards and parsing their reference sets. See
[migration.md](./migration.md) for details.

---

## Two-Tier Grace Period

The grace period protects against two classes of premature deletion:

### Tier 1: Absolute Grace Period

**Config:** `GC_GRACE_ABSOLUTE_SECONDS` (default 3600 = 1 hour)

Any blob with `age < absolute_seconds` is unconditionally protected. This
prevents deletion of blobs that were just uploaded but whose shard references
have not yet been written to storage.

```rust
if now - blob.mtime < Duration::from_secs(grace.absolute_seconds) {
    return false; // never delete
}
```

### Tier 2: Soft Grace Period

**Config:** `GC_GRACE_SOFT_CYCLES` (default 2)

A blob must be observed as unreferenced in at least N consecutive GC scans
before it becomes eligible for deletion. This protects against eventual
consistency delays: a shard upload might be visible in the storage listing
but its sidecar might not yet be readable.

The scanner tracks per-blob unreferenced counts in memory. On each scan:

- If a blob is in the Bloom filter (referenced), reset its count to 0.
- If a blob is NOT in the Bloom filter (unreferenced), increment its count.
- If count < `soft_cycles`, skip deletion.
- If count >= `soft_cycles`, the blob is eligible for deletion.

### Combined Semantics

A blob is deleted only when:

1. It is NOT in the Bloom filter (definitely unreferenced in the current scan).
2. Its age exceeds `GC_GRACE_ABSOLUTE_SECONDS`.
3. It has been unreferenced for at least `GC_GRACE_SOFT_CYCLES` consecutive
   scans.

This provides defense in depth. If any single mechanism fails, the others
still prevent data loss.

---

## Crash Recovery via Checkpoint

### Failure Modes

The GC cycle can be interrupted by:

- Process crash (OOM, SIGKILL).
- Network timeout (S3 unavailable).
- Scan timeout (exceeds `GC_SCANNER_MAX_DURATION_SECONDS`).
- Node eviction (Kubernetes preemption).

### Recovery Protocol

On the next GC cycle (or when a new node acquires the lease):

1. Load `.gc/checkpoint.json`. If CRC32 check fails, create a fresh
   checkpoint starting from the beginning.
2. Load `.gc/bloom.bin`. If CRC32 check fails, create a fresh empty Bloom
   filter. This is safe: an empty filter protects nothing, so the next scan
   will populate it from scratch. No deletions will occur until the Bloom
   filter is populated.
3. Resume scanning from the checkpoint's `s3_cursor`.
4. Continue inserting references into the Bloom filter.
5. When the scan completes, proceed to the candidate computation and deletion
   phases as normal.

### Atomic State Writes

Both the Bloom filter and checkpoint are written atomically:

1. Write data to a temporary file (e.g., `.gc/bloom.bin.tmp`).
2. `fsync` the temp file.
3. `rename` the temp file to the final path (atomic on POSIX and S3).

This prevents partial writes from corrupting the state files.

### Worst-Case Recovery

In the worst case (both checkpoint and Bloom filter corrupted), the system:

1. Creates a fresh Bloom filter (empty).
2. Resets the checkpoint to scan from the beginning.
3. Scans all shards, populating the Bloom filter.
4. Since the Bloom filter was empty at the start, NO candidates will be
   identified in the first cycle after recovery (everything looks referenced
   because nothing has been scanned yet).
5. Subsequent cycles will correctly identify orphans.

This means crash recovery is always safe: the worst case is a missed deletion
cycle, never an incorrect deletion.

---

## Safety Invariants

The incremental GC system maintains the following invariants at all times:

### Invariant 1: Degraded Scan → No Deletion

If any shard's references could not be fully determined (sidecar missing AND
shard parse failed), the GC cycle marks itself as "degraded" and skips the
deletion phase entirely. The Bloom filter and checkpoint are still saved so
the next cycle can resume normally.

### Invariant 2: Bloom Filter False Positives Only Retain, Never Delete

The Bloom filter has a configurable false-positive rate (default 0.1%). A
false positive means an unreferenced hash is incorrectly reported as referenced.
This causes the corresponding blob to be retained (wasted space) but NEVER
causes a referenced blob to be deleted (data loss).

### Invariant 3: Grace Period Re-Checked Before Deletion

Even if a blob passed the grace period check during the candidate computation
phase, the grace period is re-checked immediately before each individual
deletion. This prevents a race where a blob is uploaded between the scan and
delete phases.

### Invariant 4: Lease Guarantees Single Writer

Only one node runs the full GC cycle at a time. Other nodes skip their cycle
and retry at the next interval. This prevents concurrent modifications to the
Bloom filter and checkpoint.

### Invariant 5: Checkpoint Cursor Advances Monotonically

The scanner's cursor (S3 continuation token) only advances forward. A crash
and recovery will re-scan some objects (those between the last checkpoint and
the crash point) but will never skip objects. Re-scanning is safe: inserting
the same hash into the Bloom filter twice is a no-op.

### Invariant 6: Delete Batch Size Limits Blast Radius

The `GC_DELETE_BATCH_SIZE` (default 100) limits the number of blobs deleted in
a single GC cycle. If there is a bug in the candidate computation, at most
100 blobs are affected before the cycle ends and the issue can be investigated.

---

## Observability

### Prometheus Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `gc_cycles_total` | Counter | Total GC cycles attempted |
| `gc_cycles_success_total` | Counter | Successfully completed cycles |
| `gc_cycles_failed_total` | Counter | Failed cycles |
| `gc_cycles_skipped_total` | Counter | Skipped (lease held by another node) |
| `gc_shards_scanned_total` | Counter | Total shards scanned |
| `gc_xorbs_scanned_total` | Counter | Total xorbs scanned |
| `gc_blobs_deleted_total` | Counter | Total blobs deleted |
| `gc_bytes_freed_total` | Counter | Total bytes freed |
| `gc_delete_errors_total` | Counter | Individual deletion failures |
| `gc_bloom_queries_total` | Counter | Bloom filter lookups |
| `gc_bloom_hits_total` | Counter | Bloom filter hits (items found) |
| `gc_bloom_rebuilds_total` | Counter | Bloom filter rebuilds triggered |
| `gc_bloom_items_current` | Gauge | Current items in active Bloom filter |
| `gc_bloom_memory_bytes` | Gauge | Memory used by active Bloom filter |
| `gc_sidecar_missing_total` | Counter | Sidecar files not found (fallback used) |
| `gc_lease_acquired_total` | Counter | Lease acquisitions succeeded |
| `gc_lease_failed_total` | Counter | Lease acquisitions failed |
| `gc_lease_renewals_total` | Counter | Lease renewals |
| `gc_cycle_duration_seconds` | Histogram | End-to-end cycle duration |
| `gc_scan_speed_shards_per_second` | Histogram | Scan throughput |

### Health Endpoint

```
GET /gc/health
```

Returns a JSON object with:

- Bloom filter status (items, memory, FPR, rebuild count).
- Last GC cycle summary (timestamp, duration, counts).
- Current lease holder and expiration.
- Sidecar coverage percentage.

### Key Alert Conditions

| Alert | Condition | Severity |
|-------|-----------|----------|
| GC not running | `gc_cycles_total` flat for > 2 intervals | Warning |
| High sidecar miss rate | `gc_sidecar_missing_total` / `gc_shards_scanned_total` > 5% | Warning |
| Bloom filter near capacity | `gc_bloom_items_current` / capacity > 80% | Info |
| Deletion errors | `gc_delete_errors_total` increasing | Warning |
| Lease contention | `gc_lease_failed_total` > `gc_cycles_total` * 0.5 | Warning |
