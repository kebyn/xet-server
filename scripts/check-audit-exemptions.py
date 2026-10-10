#!/usr/bin/env python3
"""Check the dependency versions covered by the h2 audit exception.

The normal `cargo audit` run uses .cargo/audit.toml to keep the evaluated h2
advisory quiet. This guard runs against an unignored JSON report and prevents a
new affected h2 version (or the old rsa phantom dependency) from being added
without a fresh review.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ADVISORY = "RUSTSEC-2026-0258"
ALLOWED = {("h2", "0.3.27")}


def report_from_audit(lockfile: Path) -> dict:
    with tempfile.TemporaryDirectory(prefix="xet-audit-") as cwd:
        result = subprocess.run(
            ["cargo", "audit", "--json", "--no-fetch", "--file", str(lockfile)],
            cwd=cwd,
            text=True,
            capture_output=True,
            check=False,
        )
    if not result.stdout.strip():
        print(result.stderr, file=sys.stderr)
        raise RuntimeError("cargo audit --json produced no report")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        print(result.stdout, file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        raise RuntimeError(f"invalid cargo audit JSON: {error}") from error


def check(report: dict, lockfile: Path) -> None:
    lock_text = lockfile.read_text(encoding="utf-8")
    if 'name = "rsa"' in lock_text:
        raise RuntimeError("rsa is present in Cargo.lock; remove the phantom dependency or reassess it")

    found: set[tuple[str, str]] = set()
    for entry in report.get("vulnerabilities", {}).get("list", []):
        advisory = entry.get("advisory", {})
        if advisory.get("id") != ADVISORY:
            continue
        package = entry.get("package", {})
        found.add((package.get("name", ""), package.get("version", "")))

    unexpected = found - ALLOWED
    if unexpected:
        values = ", ".join(f"{name} {version}" for name, version in sorted(unexpected))
        raise RuntimeError(
            f"{ADVISORY} affects unreviewed dependency versions: {values}; reassess the exception"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", type=Path, help="use an existing cargo audit JSON report")
    parser.add_argument("--lockfile", type=Path, default=Path("Cargo.lock"))
    args = parser.parse_args()
    report = json.loads(args.report.read_text(encoding="utf-8")) if args.report else report_from_audit(args.lockfile.resolve())
    try:
        check(report, args.lockfile)
    except (OSError, RuntimeError, json.JSONDecodeError) as error:
        print(f"audit exception guard failed: {error}", file=sys.stderr)
        return 1
    print(f"audit exception guard passed (allowed h2 versions: {', '.join(f'{name} {version}' for name, version in sorted(ALLOWED))})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
