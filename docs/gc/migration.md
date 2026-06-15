# Migration Guide: Legacy GC → Incremental GC

This document details the procedure for migrating from the legacy Hub-dependent
GC to the new incremental GC system. The migration is designed as a 4-phase
渐进式 rollout with rollback capability at each stage.

---

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Overview](#overview)
3. [Phase 1: Deploy New Code (Disabled)](#phase-1-deploy-new-code-disabled)
4. [Phase 2: Dry-Run Mode](#phase-2-dry-run-mode)
5. [Phase 3: Actual Deletion + Old GC Parallel](#phase-3-actual-deletion--old-gc-parallel)
6. [Phase 4: Full Cutover](#phase-4-full-cutover)
7. [Sidecar Migration Script](#sidecar-migration-script)
8. [Monitoring During Migration](#monitoring-during-migration)
9. [Rollback Procedures](#rollback-procedures)
10. [Timeline](#timeline)

---

## Prerequisites

Before starting the migration, ensure:

- [ ] All CAS nodes are running a version that includes the incremental GC code
      (Tasks 1-13 complete).
- [ ] Prometheus metrics are being collected from all CAS nodes.
- [ ] Alerting is configured for the key GC metrics listed in
      [architecture.md](./architecture.md#observability).
- [ ] You have access to deploy configuration changes to all CAS nodes.
- [ ] The sidecar migration script (`tools/migrate_gc_sidecars`) is built and
      available.
- [ ] You have reviewed the [configuration reference](./configuration.md).

---

## Overview

```
Phase 1 (1-2 weeks)         Phase 2 (1-2 weeks)         Phase 3 (2-4 weeks)
Deploy new code             Dry-run mode                 Actual deletion
GC_ENABLED=false            GC_DRY_RUN=true              GC_DRY_RUN=false
Verify no bugs              Verify logic correct         Compare old vs new GC
                                                                               Phase 4 (permanent)
                                                                               Full cutover
                                                                               Disable old GC
                                                                               Remove legacy code
```

Each phase has explicit exit criteria that must be met before proceeding.
If any phase fails its criteria, roll back to the previous phase.

---

## Phase 1: Deploy New Code (Disabled)

**Duration:** 1-2 weeks

**Goal:** Deploy the new incremental GC code to production without activating
it, to verify that the code has no bugs and does not affect existing system
behavior.

### Configuration

```bash
# New incremental GC: DISABLED
GC_ENABLED=false

# Legacy GC: keep running as before
# (no changes to legacy GC environment variables)
```

### Actions

1. Deploy the new CAS binary to all nodes.
2. Verify that the legacy GC continues to run normally.
3. Monitor for any regressions in:
   - CAS API latency (p50, p95, p99).
   - CAS error rates.
   - Memory usage (the new code adds ~0 memory when disabled).
   - Storage consumption.

### Exit Criteria

- [ ] All CAS nodes are running the new binary.
- [ ] No regressions in API latency or error rates for 1 week.
- [ ] Legacy GC continues to delete orphans normally.

### Rollback

If any issue is observed:

1. Revert to the previous CAS binary.
2. No data migration is needed (the new code made no changes when disabled).

---

## Phase 2: Dry-Run Mode

**Duration:** 1-2 weeks

**Goal:** Run the incremental GC in dry-run mode to verify that the Bloom
filter is populated correctly and the candidate deletion set is reasonable.

### Configuration

```bash
# New incremental GC: ENABLED, DRY-RUN
GC_ENABLED=true
GC_DRY_RUN=true
GC_INTERVAL_SECONDS=3600

# Legacy GC: keep running as before
# (no changes to legacy GC environment variables)
```

### Actions

1. Enable the incremental GC in dry-run mode on one node first.
2. Monitor the dry-run output for 2-3 GC cycles.
3. Check the following metrics:

| Metric | Expected Value | Action if Unexpected |
|--------|---------------|----------------------|
| `gc_cycles_success_total` | Increments each hour | Investigate failures |
| `gc_sidecar_missing_total` | High initially (historical shards lack sidecars) | Run sidecar migration script |
| `gc_bloom_items_current` | Grows with each cycle | Check for scan issues |
| `gc_bloom_hits_total` / `gc_bloom_queries_total` | High ratio (> 90%) | Low ratio indicates Bloom filter not populated |
| `gc_lease_failed_total` | Low (only multi-node contention) | Check lease TTL |

4. Compare the dry-run candidate count with the legacy GC's deletion count.
   They should be roughly similar (not exact, due to grace period differences).
5. If dry-run looks correct, roll out to all nodes.

### Running the Sidecar Migration Script

Historical shards will not have sidecar files. The migration script backfills
them:

```bash
# Dry-run first to see what would be migrated
cargo run --bin migrate_gc_sidecars -- --dry-run

# Run the actual migration
cargo run --bin migrate_gc_sidecars -- \
  --storage-backend s3 \
  --s3-bucket my-cas-bucket \
  --concurrency 10
```

The script:

1. Lists all shards in storage.
2. For each shard without a sidecar, parses the shard to extract references.
3. Writes the sidecar file atomically.
4. Skips shards that already have sidecars (idempotent).
5. Reports progress: `migrated=X, skipped=Y, errors=Z`.

**Expected duration:** For 1M shards, approximately 1-2 hours depending on
S3 throughput and concurrency.

**Monitor:** Watch `gc_sidecar_missing_total` decrease after the migration
runs. It should approach 0 within a few GC cycles.

### Exit Criteria

- [ ] `gc_cycles_success_total` increments reliably on all nodes.
- [ ] `gc_sidecar_missing_total` is near 0 (migration complete).
- [ ] Dry-run candidate count is consistent with legacy GC deletion count.
- [ ] `gc_bloom_hits_total` / `gc_bloom_queries_total` > 90%.
- [ ] No Bloom filter corruption warnings in logs.
- [ ] Dry-run has been running for at least 1 week.

### Rollback

If dry-run reveals issues:

1. Set `GC_ENABLED=false` to disable the incremental GC.
2. The legacy GC continues to run unchanged.
3. Investigate the issue and fix before re-enabling.

No data cleanup is needed: dry-run mode does not delete anything.

---

## Phase 3: Actual Deletion + Old GC Parallel

**Duration:** 2-4 weeks

**Goal:** Enable actual deletion by the incremental GC while keeping the
legacy GC running in parallel. Compare deletion results to validate
correctness.

### Configuration

```bash
# New incremental GC: ENABLED, ACTUAL DELETION
GC_ENABLED=true
GC_DRY_RUN=false
GC_INTERVAL_SECONDS=3600
GC_DELETE_BATCH_SIZE=100
GC_GRACE_ABSOLUTE_SECONDS=3600
GC_GRACE_SOFT_CYCLES=2

# Legacy GC: ALSO running (parallel operation)
# Legacy GC environment variables unchanged
```

### Actions

1. Set `GC_DRY_RUN=false` on one node first.
2. Monitor closely for the first 3-5 GC cycles:
   - `gc_blobs_deleted_total` should increment.
   - `gc_bytes_freed_total` should increment.
   - `gc_delete_errors_total` should be low.
3. Verify data integrity:
   - Random sample of deleted blobs should not be referenced by any commit.
   - No client reports of missing data.
4. If results look correct, roll out to all nodes.
5. **Compare old vs new GC:**
   - Legacy GC deletion count vs incremental GC deletion count per cycle.
   - They should be similar. Differences are expected due to:
     * Grace period differences (new GC has two tiers).
     * Bloom filter false positives (new GC retains ~0.1% extra orphans).
     * Timing differences (different scan schedules).

### Validation Procedure

For each GC cycle, record:

| Metric | Legacy GC | Incremental GC |
|--------|-----------|----------------|
| Blobs deleted (LFS) | | |
| Blobs deleted (xorbs) | | |
| Bytes freed | | |
| Grace period skipped | | |
| Errors | | |

**Acceptable divergence:** The incremental GC should delete within 10% of
the legacy GC's count (accounting for Bloom false positives and grace period
differences). If the divergence exceeds 10%, investigate before proceeding.

### Exit Criteria

- [ ] Incremental GC has been deleting for at least 2 weeks.
- [ ] No data integrity issues reported.
- [ ] Deletion counts are within 10% of legacy GC.
- [ ] `gc_delete_errors_total` is acceptably low.
- [ ] Client-facing error rates are unchanged.

### Rollback

If data integrity issues are observed:

1. Set `GC_ENABLED=false` immediately to stop the incremental GC.
2. The legacy GC continues to run.
3. Assess the damage:
   - Check if any deleted blobs were actually referenced.
   - If so, restore from backup (LFS blobs can be re-uploaded by clients).
4. Investigate the root cause before re-enabling.

---

## Phase 4: Full Cutover

**Duration:** Permanent

**Goal:** Disable the legacy GC and fully transition to the incremental GC.

### Configuration

```bash
# New incremental GC: ENABLED
GC_ENABLED=true
GC_DRY_RUN=false

# Legacy GC: DISABLED
# Remove or comment out these variables:
# GC_HUB_BASE_URL=...
# GC_HUB_INTERNAL_TOKEN=...
# GC_GRACE_PERIOD_SECONDS=...
```

### Actions

1. **Disable legacy GC:** Set the legacy GC's enabled flag to false (or
   remove the legacy GC configuration entirely if the code supports it).
2. **Run final sidecar migration:** Ensure all shards have sidecars:
   ```bash
   cargo run --bin migrate_gc_sidecars -- \
     --storage-backend s3 \
     --s3-bucket my-cas-bucket \
     --concurrency 10
   ```
3. **Monitor for 1 week:** Verify that the incremental GC continues to
   function correctly without the legacy GC.
4. **Remove legacy code:** ✅ Done — the legacy GC code path and Hub
   `/internal/referenced-hashes` endpoint have been removed.
5. **Update documentation:** Remove references to the legacy GC from
   configuration docs and runbooks.

### Exit Criteria

- [ ] Legacy GC has been disabled for at least 1 week.
- [ ] Incremental GC is running successfully on all nodes.
- [ ] No data integrity issues.
- [ ] Sidecar coverage is 100% (or near 100%).
- [ ] Legacy GC code has been removed (Task 20).

---

## Sidecar Migration Script

The migration script (`tools/migrate_gc_sidecars`) generates sidecar files
for historical shards that were uploaded before the incremental GC was
deployed.

### Usage

```bash
# Build the migration tool
cargo build --release --bin migrate_gc_sidecars

# Run with defaults (reads XET_* env vars for storage config)
cargo run --release --bin migrate_gc_sidecars

# Override storage backend
cargo run --release --bin migrate_gc_sidecars -- \
  --storage-backend s3 \
  --s3-bucket my-cas-bucket \
  --s3-region us-east-1

# Dry-run (show what would be migrated, don't write)
cargo run --release --bin migrate_gc_sidecars -- --dry-run

# Control concurrency (default: 10)
cargo run --release --bin migrate_gc_sidecars -- --concurrency 20
```

### Output

```
[2026-06-14T10:00:00Z] INFO  Starting sidecar migration
[2026-06-14T10:00:00Z] INFO  Storage backend: s3, bucket: my-cas-bucket
[2026-06-14T10:00:05Z] INFO  Found 1,234,567 total shards
[2026-06-14T10:00:05Z] INFO  234,567 already have sidecars, skipping
[2026-06-14T10:00:05Z] INFO  1,000,000 shards need sidecar generation
[2026-06-14T10:01:00Z] INFO  Progress: 10,000/1,000,000 (migrated=10000, skipped=234567, errors=0)
...
[2026-06-14T11:30:00Z] INFO  Migration complete: total=1234567, migrated=1000000, skipped=234567, errors=0
```

### Idempotency

The script is fully idempotent:

- Shards that already have sidecars are skipped (not regenerated).
- Running the script multiple times is safe.
- Interrupted runs can be resumed (the script picks up where it left off).

### Error Handling

If a shard cannot be parsed:

- The error is logged and counted.
- The script continues to the next shard.
- The GC will fall back to parsing the shard directly during scan.

---

## Monitoring During Migration

### Key Dashboards

Create a migration dashboard with the following panels:

1. **GC Cycle Health**
   - `rate(gc_cycles_success_total[1h])` — should be ~1/hour
   - `rate(gc_cycles_failed_total[1h])` — should be 0
   - `rate(gc_cycles_skipped_total[1h])` — expected in multi-node setups

2. **Deletion Progress**
   - `rate(gc_blobs_deleted_total[1h])` — should be > 0 in Phase 3+
   - `rate(gc_bytes_freed_total[1h])` — storage savings
   - `gc_delete_errors_total` — should be low

3. **Bloom Filter Status**
   - `gc_bloom_items_current` — should grow over time
   - `gc_bloom_memory_bytes` — should be stable (~17 MB default)
   - `rate(gc_bloom_rebuilds_total[1d])` — should be low

4. **Sidecar Coverage**
   - `gc_sidecar_missing_total` — should decrease after migration
   - Sidecar coverage % (from `/gc/health` endpoint)

5. **Lease Coordination**
   - `rate(gc_lease_acquired_total[1h])` — should match cycle count
   - `rate(gc_lease_failed_total[1h])` — should be low

### Alert Thresholds During Migration

| Alert | Threshold | Action |
|-------|-----------|--------|
| GC failures | > 0 for 2 consecutive hours | Investigate, consider rollback |
| Delete errors | > 10 in 1 hour | Check S3 connectivity |
| Sidecar miss rate | > 5% after migration | Re-run migration script |
| Bloom filter memory | > 50 MB | Check `GC_BLOOM_EXPECTED_ITEMS` |
| Lease failures | > 50% of cycles | Check lease TTL / network |

---

## Rollback Procedures

### Quick Rollback (Any Phase)

To immediately disable the incremental GC:

```bash
# On all CAS nodes:
GC_ENABLED=false
```

The legacy GC continues to run (if it was running before). No data cleanup
is needed for Phases 1-2 (no deletions occurred).

### Rollback from Phase 3 (Deletions Active)

If data integrity issues are discovered:

1. **Stop incremental GC:** Set `GC_ENABLED=false` on all nodes.
2. **Assess impact:**
   - Identify which blobs were deleted by the incremental GC.
   - Cross-reference with shard references to determine if any were
     actually needed.
3. **Restore if needed:**
   - LFS blobs can be re-uploaded by clients (Git LFS client will
     automatically re-upload missing blobs).
   - Xorbs can be reconstructed from the original LFS blobs.
4. **Investigate root cause:**
   - Check Bloom filter integrity (`gc_bloom_items_current`).
   - Check checkpoint state.
   - Review logs for sidecar parse errors.
5. **Fix and re-test in dry-run mode** before re-enabling deletion.

### Rollback Checklist

- [ ] `GC_ENABLED=false` on all nodes.
- [ ] Legacy GC is running (if applicable).
- [ ] Alerting is updated to reflect rollback.
- [ ] Incident report documents the issue.
- [ ] Root cause is identified and fixed.
- [ ] Dry-run validation passes before re-enabling.

---

## Timeline

| Week | Phase | Actions |
|------|-------|---------|
| 1 | Phase 1 | Deploy new code, `GC_ENABLED=false` |
| 2 | Phase 1 | Monitor for regressions |
| 3 | Phase 2 | Enable dry-run on one node |
| 3 | Phase 2 | Run sidecar migration script |
| 4 | Phase 2 | Roll out dry-run to all nodes |
| 5 | Phase 3 | Enable deletion on one node |
| 6 | Phase 3 | Roll out deletion to all nodes |
| 7-8 | Phase 3 | Monitor and compare with legacy GC |
| 9 | Phase 4 | Disable legacy GC |
| 10 | Phase 4 | Run final sidecar migration, remove legacy code |

**Total estimated duration:** 8-10 weeks.

The timeline can be compressed if exit criteria are met early. Do NOT skip
phases; each phase validates assumptions that the next phase depends on.
