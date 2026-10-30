#!/usr/bin/env python3
"""Ratchet production `const NAME: bool = true|false;` switches under src/.

A literal bool constant is a switch that only a rebuild can flip. The baseline
lists the declarations that already exist; a new one fails, and a listed one
that disappeared fails until it is removed from the list, so the list only
shrinks; with a base ref the list may not gain rows either. Test files (`tests.rs`, `*_tests.rs`) and `#[cfg(test)]` regions are
skipped the same way as scripts/check_inflight_blind_save_ratchet.py.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

try:
    from rust_lex import StripState, strip_line
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_lex import StripState, strip_line

ROOT = Path(__file__).resolve().parents[1]
BASELINE = ROOT / "scripts/const_bool_switch_baseline.json"
CONST_RE = re.compile(r"\bconst\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*bool\s*=\s*(?:true|false)\s*;")
# rustfmt may wrap a long declaration after `=`, leaving the literal on the next line.
OPEN_RE = re.compile(r"\bconst\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*bool\s*=\s*$")
VALUE_RE = re.compile(r"^\s*(?:true|false)\s*;")
CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*(?:all|any)?\s*\(?\s*test\b")
GUIDANCE = "use a config key or an existing switch instead of a new compile-time bool"


def declarations(text: str) -> list[tuple[int, str]]:
    """Production literal bool consts in one file as (line, name)."""
    found = []
    state = StripState()
    depth = 0
    mode = "normal"  # normal | armed (saw cfg(test)) | skip (inside the test item)
    skip_depth = 0
    pending = None
    for lineno, raw in enumerate(text.splitlines(), start=1):
        code = strip_line(raw, state)
        if mode == "normal" and CFG_TEST_RE.search(code):
            mode = "armed"
        if mode == "normal":
            if pending and VALUE_RE.match(code):
                found.append(pending)
            found.extend((lineno, match.group(1)) for match in CONST_RE.finditer(code))
            opened = OPEN_RE.search(code)
            pending = (lineno, opened.group(1)) if opened else None
        else:
            pending = None
        for ch in code:
            if ch == "{":
                if mode == "armed":
                    mode, skip_depth = "skip", depth
                depth += 1
            elif ch == "}":
                depth -= 1
                if mode == "skip" and depth <= skip_depth:
                    mode = "normal"
            elif ch == ";" and mode == "armed":
                mode = "normal"
    return found


def collect(root: Path = ROOT) -> dict[tuple[str, str], list[int]]:
    result: dict[tuple[str, str], list[int]] = {}
    paths = sorted((root / "src").rglob("*.rs"))
    if not paths:
        raise ValueError("missing Rust source")
    for path in paths:
        if path.name == "tests.rs" or path.name.endswith("_tests.rs"):
            continue
        rel = path.relative_to(root).as_posix()
        for lineno, name in declarations(path.read_text(encoding="utf-8")):
            result.setdefault((rel, name), []).append(lineno)
    return result


def parse_baseline(text: str) -> Counter:
    data = json.loads(text)
    if not isinstance(data, list) or any(
        not isinstance(row, list) or len(row) != 2 or not all(isinstance(v, str) and v for v in row)
        for row in data
    ):
        raise ValueError("baseline must be a list of [path, name] pairs")
    return Counter(tuple(row) for row in data)


def problems(actual: dict[tuple[str, str], list[int]], baseline: Counter) -> list[str]:
    failures = []
    for key in sorted(actual.keys() | baseline.keys()):
        lines, allowed = actual.get(key, []), baseline.get(key, 0)
        path, name = key
        if len(lines) > allowed:
            where = ",".join(map(str, lines))
            failures.append(f"{path}:{where} new `const {name}: bool` switch; {GUIDANCE}")
        elif len(lines) < allowed:
            failures.append(f"{path} {name}: no longer declared; remove it from {BASELINE.name}")
    return failures


def admission_problems(root: Path, base_ref: str, baseline: Counter) -> list[str]:
    """Rows the base tree's list did not have; a base without the list admits the first one."""
    rel = BASELINE.relative_to(ROOT).as_posix()
    git = ["git", "-C", str(root)]
    base = subprocess.check_output([*git, "rev-parse", "--verify", "--end-of-options", f"{base_ref}^{{commit}}"],
                                   text=True, stderr=subprocess.PIPE).strip()
    if not subprocess.check_output([*git, "ls-tree", "--name-only", base, "--", rel], text=True).strip():
        return []
    previous = parse_baseline(subprocess.check_output([*git, "show", f"{base}:{rel}"], text=True))
    return [f"{path} {name}: baseline row added over {base_ref}; {GUIDANCE}"
            for (path, name), n in sorted(baseline.items()) if n > previous.get((path, name), 0)]


def main(argv: list[str] | None = None, root: Path = ROOT) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-ref", default=os.environ.get("TEST_LANE_BASELINE_REF"))
    args = parser.parse_args(argv)
    try:
        baseline = parse_baseline((root / BASELINE.relative_to(ROOT)).read_text(encoding="utf-8"))
        actual = collect(root)
        failures = problems(actual, baseline)
        if args.base_ref is not None:
            failures += admission_problems(root, args.base_ref, baseline)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"const bool switch ratchet ERROR: {error}")
        return 1
    total = sum(len(lines) for lines in actual.values())
    print(f"const bool switch ratchet {'FAIL' if failures else 'PASS'}: {total}/{sum(baseline.values())}")
    for failure in failures:
        print(failure)
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
