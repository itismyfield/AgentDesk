#!/usr/bin/env python3
"""Warn on hotfile raw growth; preserve measurement and manifest integrity checks."""

from __future__ import annotations

import os
import re
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

from ratchet_admission import (
    HISTORY_REL_PATH, audit_repository_admissions, parse_cap_table, _git_text, _run_git,
)

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "scripts" / "hotfile_ratchet.toml"
PIN_POLICY_METADATA = frozenset(("scripts/hotfile_ratchet.toml", HISTORY_REL_PATH))
REQUIRED_HOTFILES = (
    "src/services/discord/tmux_watcher.rs",
    "src/services/discord/tui_prompt_relay.rs",
    "src/services/discord/turn_bridge/mod.rs",
)


def line_count(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def changes(base: str, candidate: str) -> list[tuple[str, str, str]]:
    result = _run_git(REPO_ROOT, ["diff-tree", "--no-commit-id", "-r", "-M", "-C",
        "--find-copies-harder", "--name-status", "-z", base, candidate])
    if result.returncode or (result.stdout and not result.stdout.endswith("\0")):
        raise ValueError("hotfile movement query failed: " + result.stderr.strip())
    fields, rows = iter(result.stdout.split("\0")[:-1]), []
    for status in fields:
        if not re.fullmatch(r"[ADMT]|[RC]\d{1,3}", status):
            raise ValueError(f"uncertain hotfile diff status: {status!r}")
        source = next(fields)
        target = next(fields) if status[0] in "RC" else source
        rows.append((status[0], source, target))
    return rows


def regular_blob(ref: str, path: str) -> None:
    result = _run_git(REPO_ROOT, ["ls-tree", "-z", ref, "--", path])
    rows = result.stdout.split("\0")[:-1]
    if (result.returncode or len(rows) != 1 or not rows[0].endswith("\t" + path)
            or rows[0].split()[:2] not in (["100644", "blob"], ["100755", "blob"])):
        raise ValueError(f"hotfile is not a regular candidate blob: {path}")


def pin_transfers(base: str, candidate: str, prior_caps: dict[str, int],
                  ceilings: dict[str, int]) -> dict[str, int]:
    removed = prior_caps.keys() - ceilings.keys()
    for source in sorted(removed):
        _, exists = _git_text(REPO_ROOT, candidate, source)
        if exists:
            raise ValueError(f"ratcheted hotfile ceiling removed while source remains: {source}")
    rows = changes(base, candidate)
    uncertain_targets = {target for code, _, target in rows if code in "ARC"
        or (code in "MT" and target not in prior_caps and target not in PIN_POLICY_METADATA)}
    candidate_text, exists = _git_text(REPO_ROOT, candidate, "scripts/hotfile_ratchet.toml")
    if not exists:
        raise ValueError("candidate hotfile manifest is missing")
    candidate_caps = parse_cap_table(candidate_text, "hotfile_ratchet")
    transferred = {}
    for source in sorted(removed):
        targets = [target for code, old, target in rows if old == source and code in "RC"]
        if not targets:
            if uncertain_targets or ("D", source, source) not in rows:
                raise ValueError(f"uncertain hotfile retirement or movement: {source}")
            print(f"OK: retired hotfile: {source} (candidate {candidate}).")
            continue
        if len(targets) != 1:
            raise ValueError(f"ambiguous hotfile movement targets: {source}")
        target = targets[0]
        if (target in prior_caps or target in transferred or target not in ceilings
                or candidate_caps.get(target) != ceilings[target]):
            raise ValueError(f"hotfile movement lacks a matching pin transfer: {source} -> {target}")
        regular_blob(base, source)
        regular_blob(candidate, target)
        old_text, old_exists = _git_text(REPO_ROOT, base, source)
        new_text, new_exists = _git_text(REPO_ROOT, candidate, target)
        if not old_exists or not new_exists:
            raise ValueError(f"hotfile transfer measurement is unavailable: {source} -> {target}")
        old_size, new_size = len(old_text.splitlines()), len(new_text.splitlines())
        if new_size > old_size:
            print(f"WARNING: giant growth: {target}: +{new_size - old_size} raw lines "
                  f"({old_size} -> {new_size}); transferred candidate {candidate}.", file=sys.stderr)
        if ceilings[target] > prior_caps[source]:
            print(f"WARNING: giant reference increased: {target}: +{ceilings[target] - prior_caps[source]} "
                  f"raw lines ({prior_caps[source]} -> {ceilings[target]}); transferred from {source}.", file=sys.stderr)
        transferred[target] = old_size
        print(f"OK: transferred hotfile: {source} -> {target} (candidate {candidate}).")
    return transferred


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
    transferred = {}
    if audit.base_sha:
        prior_text, exists = _git_text(REPO_ROOT, audit.base_sha, "scripts/hotfile_ratchet.toml")
        prior_caps = parse_cap_table(prior_text, "hotfile_ratchet") if exists else {}
        removed = prior_caps.keys() - ceilings.keys()
        if removed:
            ref = os.environ.get("GFP_CANDIDATE_SHA", "") if os.environ.get("GFP_EVENT_NAME") == "pull_request" else "HEAD"
            resolved = _run_git(REPO_ROOT, ["rev-parse", "--verify", f"{ref}^{{commit}}"])
            if resolved.returncode or not resolved.stdout.strip():
                raise ValueError("cannot resolve hotfile retirement candidate object")
            candidate_sha = resolved.stdout.strip()
            transferred = pin_transfers(audit.base_sha, candidate_sha, prior_caps, ceilings)
    for rel, ceiling in sorted(ceilings.items()):
        if (REPO_ROOT / rel).is_symlink():
            raise ValueError(f"ratcheted hotfile is a symlink: {rel}")
        current = line_count(REPO_ROOT / rel)
        limit, label = ceiling, "ceiling"
        if rel in transferred:
            limit, label = transferred[rel], f"transferred base {audit.base_sha}"
        elif audit.base_sha:
            prior, exists = _git_text(REPO_ROOT, audit.base_sha, rel)
            if exists:
                limit, label = len(prior.splitlines()), f"base {audit.base_sha}"
        if current > limit:
            print(f"WARNING: giant growth: {rel}: +{current - limit} raw lines "
                  f"({limit} -> {current}); {label} is advisory.", file=sys.stderr)
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
