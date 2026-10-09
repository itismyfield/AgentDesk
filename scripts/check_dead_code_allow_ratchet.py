#!/usr/bin/env python3
"""Ratchet Rust dead-code suppressions, including unused/warnings groups."""
import argparse
import json
import os
import re
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from scripts.check_clippy_allow_ratchet import neutralize_source, rust_attributes, suppression_bodies
BASELINE = ROOT / "scripts/dead_code_allow_occurrences.json"
INNER_BASELINE = ROOT / "scripts/dead_code_inner_allow_occurrences.json"
TOKEN = re.compile(r"(?<![\w:])(?:dead_code|unused|warnings)\b")


def count(text, inner_only=False):
    cleaned, ambiguous = neutralize_source(text)
    attributes, broken = rust_attributes(cleaned)
    total = 0
    for attribute in attributes:
        bodies, bad = suppression_bodies(attribute)
        broken |= bad
        if not inner_only or attribute.startswith("#!"):
            total += sum(bool(TOKEN.search(body)) for body in bodies)
    if ambiguous or broken:
        raise ValueError("ambiguous Rust attributes")
    return total


def collect(root=ROOT, inner_only=False):
    result = {}
    paths = sorted((root / "src").rglob("*.rs"))
    if not paths:
        raise ValueError("missing Rust source")
    for path in paths:
        n = count(path.read_text(encoding="utf-8"), inner_only)
        if n:
            result[path.relative_to(root).as_posix()] = n
    return result


def validate_baseline(baseline):
    if not isinstance(baseline, dict) or any(not isinstance(k, str) or type(v) is not int or v < 1 for k, v in baseline.items()):
        raise ValueError("invalid baseline")


def problems(actual, baseline):
    validate_baseline(baseline)
    failures = []
    for path in sorted(actual.keys() | baseline.keys()):
        n, ceiling = actual.get(path, 0), baseline.get(path, 0)
        if n > ceiling:
            failures.append(f"{path}: {n} suppressions exceed {ceiling}")
        elif n < ceiling:
            failures.append(f"{path}: stale baseline {ceiling}; lower it to {n}")
    return failures


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args], text=True, stderr=subprocess.PIPE)


def collect_at(root, revision):
    # Batch immutable blobs so initial inner admission measures the base tree without checkout.
    entries = []
    for row in git(root, "ls-tree", "-r", "-z", revision, "--", "src").split("\0"):
        if not row:
            continue
        metadata, path = row.split("\t", 1)
        if not path.endswith(".rs"):
            continue
        mode, kind, oid = metadata.split()
        if kind != "blob" or mode not in {"100644", "100755"}:
            raise ValueError("non-file Rust source in base tree")
        entries.append((path, oid))
    if not entries:
        raise ValueError("missing Rust source in base tree")
    data = subprocess.check_output(["git", "-C", str(root), "cat-file", "--batch"],
                                   input="".join(oid + "\n" for _, oid in entries).encode(), stderr=subprocess.PIPE)
    result, cursor = {}, 0
    for path, oid in entries:
        end = data.index(b"\n", cursor)
        observed, kind, size = data[cursor:end].decode().split()
        if observed != oid or kind != "blob":
            raise ValueError("invalid base source blob")
        start, size = end + 1, int(size)
        cursor = start + size + 1
        if data[cursor - 1:cursor] != b"\n":
            raise ValueError("truncated base source blob")
        n = count(data[start:start + size].decode("utf-8"), inner_only=True)
        if n:
            result[path] = n
    if cursor != len(data):
        raise ValueError("unexpected base source bytes")
    return result


def admission_problems(root, base_ref, baseline, inner_baseline):
    base = git(root, "rev-parse", "--verify", "--end-of-options", f"{base_ref}^{{commit}}").strip()
    total_path, inner_path = BASELINE.relative_to(ROOT).as_posix(), INNER_BASELINE.relative_to(ROOT).as_posix()
    admitted = json.loads(git(root, "show", f"{base}:{total_path}"))
    if git(root, "ls-tree", "--name-only", base, "--", inner_path).strip():
        admitted_inner = json.loads(git(root, "show", f"{base}:{inner_path}"))
    else:
        admitted_inner = collect_at(root, base)
    failures = []
    for label, current, previous in (("total", baseline, admitted), ("inner", inner_baseline, admitted_inner)):
        validate_baseline(current)
        validate_baseline(previous)
        failures.extend(f"{label} baseline {path}: {n} exceeds base allocation {previous.get(path, 0)}"
                        for path, n in current.items() if n > previous.get(path, 0))
    return failures


def main(argv=None, root=ROOT):
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-ref", default=os.environ.get("TEST_LANE_BASELINE_REF"))
    args = parser.parse_args(argv)
    try:
        baseline = json.loads((root / BASELINE.relative_to(ROOT)).read_text())
        inner_baseline = json.loads((root / INNER_BASELINE.relative_to(ROOT)).read_text())
        actual, inner = collect(root), collect(root, inner_only=True)
        failures = problems(actual, baseline)
        failures += [f"inner {failure}" for failure in problems(inner, inner_baseline)]
        if args.base_ref is not None:
            failures += admission_problems(root, args.base_ref, baseline, inner_baseline)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"dead-code suppression ratchet ERROR: {error}")
        return 1
    print(f"dead-code suppression ratchet {'FAIL' if failures else 'PASS'}: {sum(actual.values())}/{sum(baseline.values())}")
    print(f"inner suppression allocation: {sum(inner.values())}/{sum(inner_baseline.values())}")
    for failure in failures:
        print(failure)
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
