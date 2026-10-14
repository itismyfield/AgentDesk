#!/usr/bin/env python3
"""Regenerate review artifacts before a separate correction commit and SHA freeze."""

import argparse
import subprocess
import sys
from pathlib import Path


COMMANDS = (
    ("check_legacy_supervision_census.py", "--sync-skips"),
    ("generate_inventory_docs.py", "--review-docs-only", "--sync-giant-docs"),
    ("generate_env_reference.py",),
    ("generate_cli_reference.py",),
    ("check_test_target_integrity.py", "--write-lib-inventory-manifest"),
)


def prepare(root: Path) -> int:
    for command in COMMANDS:
        argv = [sys.executable, str(root / "scripts" / command[0]), *command[1:]]
        result = subprocess.run(argv, cwd=root, check=False)
        if result.returncode:
            print(f"review preparation failed: {command[0]} exit={result.returncode}", file=sys.stderr)
            return result.returncode if result.returncode > 0 else 1
    print("REVIEW_ARTIFACTS_READY: review and commit corrections before freezing the review SHA")
    print("PG lane membership synchronization remains a separate required step")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    return prepare(args.repo_root.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
