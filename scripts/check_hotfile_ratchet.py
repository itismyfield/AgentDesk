#!/usr/bin/env python3
"""Freeze hotfile raw line counts at the immutable comparison SHA; exports use caps."""

from __future__ import annotations

import sys
from pathlib import Path

MIN_PYTHON = (3, 11)

# Keep this before importing tomllib so unsupported interpreters fail with the
# repository policy message instead of a raw ModuleNotFoundError.
if sys.version_info < MIN_PYTHON:
    version = (
        f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}"
    )
    print(
        "ERROR: scripts/check_hotfile_ratchet.py requires Python 3.11+ "
        "for stdlib tomllib; "
        f"{sys.executable} is Python {version}.",
        file=sys.stderr,
    )
    print(
        "Run with python3.11+ or set PYTHON=/path/to/python3.11+ when using "
        "scripts/ci-script-checks.sh.",
        file=sys.stderr,
    )
    raise SystemExit(1)

import tomllib

from ratchet_admission import audit_repository_admissions, parse_cap_table, _git_text

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "scripts" / "hotfile_ratchet.toml"
REQUIRED_HOTFILES = (
    "src/services/discord/tmux_watcher.rs",
    "src/services/discord/tui_prompt_relay.rs",
    "src/services/discord/turn_bridge/mod.rs",
)


def line_count(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def check() -> int:
    ceilings = parse_cap_table(MANIFEST.read_text(encoding="utf-8"), "hotfile_ratchet")
    if not ceilings or any(rel not in ceilings for rel in REQUIRED_HOTFILES):
        raise ValueError("hotfile manifest must contain every required hotfile ceiling")
    audit = audit_repository_admissions(repo_root=REPO_ROOT, ratchet="hotfile_ratchet",
        config_rel_path="scripts/hotfile_ratchet.toml", table_name="hotfile_ratchet")
    failed = bool(audit.errors)
    for warning in audit.warnings:
        print(warning, file=sys.stderr)
    for error in audit.errors:
        print(f"FAIL: {error}", file=sys.stderr)
    if audit.base_sha:
        prior_text, exists = _git_text(REPO_ROOT, audit.base_sha, "scripts/hotfile_ratchet.toml")
        prior_caps = parse_cap_table(prior_text, "hotfile_ratchet") if exists else {}
        if prior_caps.keys() - ceilings.keys():
            raise ValueError("ratcheted hotfile ceiling removed: " + ", ".join(sorted(prior_caps.keys() - ceilings.keys())))
    for rel, ceiling in sorted(ceilings.items()):
        current = line_count(REPO_ROOT / rel)
        limit, label = ceiling, "ceiling"
        if audit.base_sha:
            prior, exists = _git_text(REPO_ROOT, audit.base_sha, rel)
            if exists:
                limit, label = len(prior.splitlines()), f"base {audit.base_sha}"
        if current > limit:
            print(f"FAIL: {rel} grew to {current} lines > {label} {limit}.", file=sys.stderr)
            failed = True
        else:
            print(f"OK: {rel} = {current} lines ({label} {limit}).")
    return int(failed)


def main() -> int:
    try:
        return check()
    except tomllib.TOMLDecodeError as error:
        print(f"FAIL: hotfile cap metadata invalid: {error}", file=sys.stderr)
        return 1
    except Exception as error:
        print(f"FAIL: hotfile measurement failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
