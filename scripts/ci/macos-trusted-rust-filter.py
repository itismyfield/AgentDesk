#!/usr/bin/env python3
"""Decide whether a CI macOS Trusted push needs the heavy macOS steps.

Prints exactly one line, `run=true` or `run=false`, for `$GITHUB_OUTPUT`.
Changed paths are this branch's own commits: `git diff <merge-base> HEAD`
against the base ref. Anything this script cannot establish prints
`run=true`; only a fully resolved, non-empty diff with no Rust input skips.
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys

# Paths whose change can alter the result of the macOS jobs'
# `cargo check` / `cargo test` / fresh-user smoke steps.
RUST_INPUTS = (
    "**/*.rs",
    "**/Cargo.toml",
    "**/Cargo.lock",
    "rust-toolchain*",
    ".cargo/**",
    "sqlx-data.json",
    ".sqlx/**",
    # `sqlx::migrate!` embeds these; build.rs reruns on them.
    "migrations/**",
    # `include_str!("../defaults.json")` in src/config.rs.
    "defaults.json",
    # Rust tests load these through CARGO_MANIFEST_DIR.
    "policies/**",
    "tests/fixtures/**",
    "tests/e2e/**",
    # Enumerated by `bundled_sample_routines_load_and_validate`.
    "routines/**",
    # ci-pr.yml's relay_contract filter compiles against this doc.
    "docs/relay-state-contract.md",
    # Executed by Rust tests (current_message_anchor, rowless_receipt_tests).
    "scripts/relay_authority_rollout_report.py",
    # Invoked by the macOS jobs' own steps.
    "scripts/ci-timeout.py",
    "scripts/ci-macos-fresh-user-smoke.sh",
    "scripts/operator-init-portable.py",
    ".github/workflows/ci-macos-trusted.yml",
    "scripts/ci/macos-trusted-rust-filter.py",
)


def _glob_to_regex(pattern: str) -> re.Pattern[str]:
    out = []
    i = 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    return re.compile("".join(out) + r"\Z")


_RUST_INPUT_RES = tuple(_glob_to_regex(p) for p in RUST_INPUTS)


def is_rust_input(path: str) -> bool:
    return any(r.match(path) for r in _RUST_INPUT_RES)


def decide(paths: list[str]) -> tuple[bool, str]:
    paths = [p for p in paths if p]
    if not paths:
        return True, "empty change list"
    hits = [p for p in paths if is_rust_input(p)]
    if hits:
        return True, f"{len(hits)} of {len(paths)} changed paths are Rust inputs, e.g. {hits[0]}"
    return False, f"none of {len(paths)} changed paths are Rust inputs"


def _git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout


def branch_changes(base_ref: str) -> list[str]:
    merge_base = _git("merge-base", base_ref, "HEAD").strip()
    # --no-renames lists both sides of a rename; -z keeps git from quoting
    # non-ASCII or special-character paths, which would hide their suffix.
    return _git("diff", "-z", "--no-renames", "--name-only", merge_base, "HEAD").split("\0")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument(
        "--paths-from-stdin",
        action="store_true",
        help="read newline-separated changed paths instead of diffing git",
    )
    args = parser.parse_args(argv)

    if args.event != "push":
        run, reason = True, f"event '{args.event}' always runs"
    else:
        try:
            paths = (
                sys.stdin.read().splitlines()
                if args.paths_from_stdin
                else branch_changes(args.base_ref)
            )
        except (OSError, subprocess.CalledProcessError) as exc:
            run, reason = True, f"cannot resolve changed paths ({exc})"
        else:
            run, reason = decide(paths)

    print(f"macOS trusted filter: run={str(run).lower()} ({reason})", file=sys.stderr)
    print(f"run={str(run).lower()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
