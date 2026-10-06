#!/usr/bin/env python3
"""Ratchet Rust dead-code suppressions, including unused/warnings groups."""
import json
import re
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from scripts.check_clippy_allow_ratchet import neutralize_source, rust_attributes, suppression_bodies
BASELINE = ROOT / "scripts/dead_code_allow_occurrences.json"
TOKEN = re.compile(r"(?<![\w:])(?:dead_code|unused|warnings)\b")


def count(text):
    cleaned, ambiguous = neutralize_source(text)
    attributes, broken = rust_attributes(cleaned)
    total = 0
    for attribute in attributes:
        bodies, bad = suppression_bodies(attribute)
        broken |= bad
        total += sum(bool(TOKEN.search(body)) for body in bodies)
    if ambiguous or broken:
        raise ValueError("ambiguous Rust attributes")
    return total


def collect(root=ROOT):
    result = {}
    paths = sorted((root / "src").rglob("*.rs"))
    if not paths:
        raise ValueError("missing Rust source")
    for path in paths:
        n = count(path.read_text(encoding="utf-8"))
        if n:
            result[path.relative_to(root).as_posix()] = n
    return result


def problems(actual, baseline):
    if not isinstance(baseline, dict) or any(not isinstance(k, str) or type(v) is not int or v < 1 for k, v in baseline.items()):
        raise ValueError("invalid baseline")
    return [f"{p}: {n} suppressions exceed {baseline.get(p, 0)}" for p, n in actual.items() if n > baseline.get(p, 0)]


def main():
    try:
        baseline = json.loads(BASELINE.read_text())
        actual = collect()
        failures = problems(actual, baseline)
    except (OSError, ValueError) as error:
        print(f"dead-code suppression ratchet ERROR: {error}")
        return 1
    print(f"dead-code suppression ratchet {'FAIL' if failures else 'PASS'}: {sum(actual.values())}/{sum(baseline.values())}")
    for failure in failures:
        print(failure)
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
