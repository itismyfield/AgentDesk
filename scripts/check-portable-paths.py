#!/usr/bin/env python3
"""Reject operator-specific home paths in deployable portable surfaces."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


SPECIFIC_USER_HOME = re.compile(r"/Users/(?!(?:REPLACE_ME|user|me|example)(?:/|$))[A-Za-z0-9._-]+")

DEFAULT_PATTERNS = (
    "scripts/_defaults.sh",
    "scripts/build-release.sh",
    "scripts/deploy.sh",
    "scripts/deploy-dashboard.sh",
    "scripts/deploy-release.sh",
    "scripts/ensure-agentdesk-cli.sh",
    "scripts/install.sh",
    "scripts/queue-stability-batch.sh",
    "scripts/setup-hooks.sh",
    "scripts/resolve-python-runner.sh",
    "scripts/pg_tunnel.sh",
    "scripts/launchd-migrated/*.sh",
    "scripts/launchd-migrated/*.py",
    "scripts/check-portable-paths.py",
    "scripts/relay_watchdog.py",
    "scripts/operator-init-portable.py",
    "scripts/portable-operator-migration-dry-run.py",
    "policies/**/*",
    "routines/**/*.js",
    "agentdesk.example.yaml",
)


def iter_default_paths(root: Path) -> list[Path]:
    paths: list[Path] = []
    for pattern in DEFAULT_PATTERNS:
        paths.extend(
            path
            for path in root.glob(pattern)
            if path.is_file()
            and not any(part.startswith(".") for part in path.relative_to(root).parts)
        )
    return sorted(set(paths))


def rel_display(path: Path, root: Path) -> str:
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return path.as_posix()


def scan_file(path: Path) -> list[tuple[int, str]]:
    hits: list[tuple[int, str]] = []
    text = path.read_text(encoding="utf-8")
    for line_no, line in enumerate(text.splitlines(), start=1):
        if SPECIFIC_USER_HOME.search(line):
            hits.append((line_no, line.strip()))
    return hits


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("paths", nargs="*", type=Path, help="Explicit files to scan instead of the default deployable set")
    args = parser.parse_args(argv)

    root = args.root.resolve()
    paths = [path.resolve() for path in args.paths] if args.paths else iter_default_paths(root)

    failures: list[str] = []
    for path in paths:
        if not path.is_file():
            continue
        for line_no, line in scan_file(path):
            failures.append(f"{rel_display(path, root)}:{line_no}: {line}")

    if failures:
        print("ERROR: operator-specific /Users/<name> paths found in portable deployable surfaces:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print(f"OK: scanned {len(paths)} portable deployable file(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
