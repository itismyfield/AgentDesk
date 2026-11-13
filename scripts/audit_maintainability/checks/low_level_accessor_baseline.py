#!/usr/bin/env python3
"""Generate approved accessor sites; subsequent updates only reduce or relabel."""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import json
import os
from pathlib import Path
import re
import sys
import subprocess

SCRIPTS = Path(__file__).resolve().parents[2]
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

from audit_maintainability.checks.low_level_accessors import RULES, compare, identity, scan

DIRECTORY = Path("scripts/audit_maintainability/baselines/generated/low_level_accessors")


def _header(rule):
    return {"schema_version": 1, "rule": rule, "accessor": RULES[rule]["accessor"],
            "owned_paths": list(RULES[rule]["owned_paths"])}


def encode(rule, rows):
    # One site per line keeps the generated approval diff reviewable.
    head = json.dumps(_header(rule), ensure_ascii=False, sort_keys=True)[:-1]
    sites = ",\n".join("    " + json.dumps(r, sort_keys=True) for r in sorted(rows, key=identity))
    return head + ', "sites": [\n' + sites + "\n]}\n"


def load_baselines(repo):
    result = []
    for rule in sorted(RULES):
        path = repo / DIRECTORY / (rule + ".json")
        payload = json.loads(path.read_text("utf-8"))
        if not isinstance(payload, dict):
            raise ValueError(f"invalid baseline object: {rule}")
        if {k: payload.get(k) for k in _header(rule)} != _header(rule):
            raise ValueError(f"baseline rule/owner metadata mismatch: {rule}")
        sites = payload.get("sites")
        if not isinstance(sites, list):
            raise ValueError(f"invalid baseline sites: {rule}")
        seen = set()
        for row in sites:
            if (not isinstance(row, dict) or row.get("rule") != rule
                    or row.get("accessor") != RULES[rule]["accessor"]
                    or row.get("owned_paths") != list(RULES[rule]["owned_paths"])
                    or not isinstance(row.get("path"), str) or not row["path"].startswith("src/")
                    or not isinstance(row.get("enclosing_fn"), str)
                    or not isinstance(row.get("normalized_hash"), str)
                    or not re.fullmatch(r"[0-9a-f]{64}", row["normalized_hash"])
                    or type(row.get("count")) is not int or row["count"] < 1
                    or type(row.get("line")) is not int or row["line"] < 1):
                raise ValueError(f"invalid baseline site: {rule}")
            if identity(row) in seen:
                raise ValueError(f"duplicate baseline site: {rule}")
            seen.add(identity(row))
        result.extend(sites)
    return sorted(result, key=identity)


def write_baselines(repo, rows):
    grouped = defaultdict(list)
    for row in rows:
        grouped[row["rule"]].append(row)
    directory = repo / DIRECTORY
    directory.mkdir(parents=True, exist_ok=True)
    for rule in sorted(RULES):
        path = directory / (rule + ".json")
        path.write_text(encode(rule, grouped[rule]), "utf-8")


def baseline_growth(repo, rows, *, base_ref=None):
    """Reject hand-edited approval growth against the PR base as well as generator growth."""
    if not (repo / ".git").exists():
        return []
    def git(*args):
        return subprocess.check_output(["git", "-C", str(repo), *args], text=True, stderr=subprocess.PIPE).strip()
    try:
        base = base_ref or os.environ.get("GFP_BASE_SHA") or git("merge-base", "HEAD", "origin/main")
        files = git("ls-tree", "-r", "--name-only", base, "--", str(DIRECTORY)).splitlines()
        if not files:
            return []  # The approved first corpus has no predecessor.
        expected = {str(DIRECTORY / (rule + ".json")) for rule in RULES}
        if set(files) != expected:
            return ["PR base contains an incomplete accessor baseline"]
        old = []
        for rule in RULES:
            payload = json.loads(git("show", f"{base}:{DIRECTORY / (rule + '.json')}"))
            if {key: payload.get(key) for key in _header(rule)} != _header(rule):
                return ["PR base accessor/owner metadata changed"]
            old.extend(payload["sites"])
        def counts(sites):
            result = Counter()
            for row in sites:
                result[(row["rule"], row["normalized_hash"])] += row["count"]
            return result
        before, after = counts(old), counts(rows)
        return [f"approval baseline grew against PR base: {key}" for key in sorted(after) if after[key] > before[key]]
    except (subprocess.CalledProcessError, OSError, ValueError, KeyError, TypeError) as exc:
        return [f"cannot verify accessor baseline PR base: {exc}"]


def _relabel_mapping(old, new):
    old_groups, new_groups = defaultdict(list), defaultdict(list)
    for rows, groups in ((old, old_groups), (new, new_groups)):
        for row in rows:
            groups[(row["rule"], row["normalized_hash"])].append(
                {"path": row["path"], "enclosing_fn": row["enclosing_fn"], "count": row["count"]})
    return [{"rule": key[0], "normalized_hash": key[1], "old": old_groups[key], "new": new_groups[key]}
            for key in sorted(old_groups) if old_groups[key] != new_groups[key]]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=SCRIPTS.parent)
    mode = parser.add_mutually_exclusive_group(required=True)
    for name in ("initialize", "reduce", "relabel", "check-baseline"):
        mode.add_argument("--" + name, action="store_true")
    args = parser.parse_args(argv)
    try:
        rows = scan(args.repo)
        files = [args.repo / DIRECTORY / (rule + ".json") for rule in RULES]
        if args.initialize:
            if any(path.exists() for path in files):
                raise ValueError("initial baseline already exists; use --reduce or --relabel")
        else:
            accepted = load_baselines(args.repo)
            errors = compare(accepted, rows, relabel=args.relabel)
            errors += baseline_growth(args.repo, accepted)
            if args.check_baseline:
                errors += compare(rows, accepted)
            if errors:
                raise ValueError("; ".join(errors))
            if args.relabel:
                print(json.dumps({"relabel": _relabel_mapping(accepted, rows)}, sort_keys=True))
            if args.check_baseline:
                print(f"ACCESSOR_BASELINE_OK sites={sum(r['count'] for r in rows)}")
                return 0
        write_baselines(args.repo, rows)
        print(f"ACCESSOR_BASELINE_WRITTEN sites={sum(r['count'] for r in rows)}")
        return 0
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(f"accessor baseline: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
