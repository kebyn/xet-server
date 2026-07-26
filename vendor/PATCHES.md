# Vendored security patches

These crates are vendored because the first upstream releases containing the
security fixes require a Rust version newer than this workspace's Rust 1.85
minimum supported version. Keep the package versions unchanged: the source is
the published crate plus the explicitly listed upstream patch.

## `time` 0.3.45

- Advisory: RUSTSEC-2026-0009 / CVE-2026-25727
- Fix: bound RFC 2822 comment recursion to prevent stack exhaustion
- Upstream commit: `1c63dc7985b8fa26bd8c689423cc56b7a03841ee`
- First fixed release: 0.3.47 (requires Rust 1.88)

## `lru` 0.12.5

- Advisory: RUSTSEC-2026-0002 / GHSA-rhfx-m35p-ff5j
- Fix: do not create an exclusive key reference from `IterMut`
- Upstream commit: `b9bca3492d75139097df3b018b6abdf5825ee868`
- First fixed release: 0.16.3 (outside AWS SDK's compatible version range)

When the workspace MSRV and the AWS SDK dependency range permit upgrading to
the fixed upstream releases, remove the corresponding `[patch.crates-io]`
entry and vendored directory.

Because RustSec identifies releases by version rather than source content, run
the strict audit with the two backported advisories explicitly acknowledged:

```bash
cargo audit \
  --ignore RUSTSEC-2026-0009 \
  --ignore RUSTSEC-2026-0002 \
  -D warnings
```

Do not add further ignores without a documented source patch and regression
test.
