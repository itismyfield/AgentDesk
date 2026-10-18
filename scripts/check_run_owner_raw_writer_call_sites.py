#!/usr/bin/env python3
"""Raw run-owner writes outside the run-owner guard's private write module.

This gate does not prove that a write goes through the guard. It reports raw owner
writes outside `src/db/auto_queue/run_owner_guard/writes.rs` and the pinned exceptions
below; what the guard does is proven by the PG contention tests in
`db::auto_queue::run_owner_guard_pg_tests`.

An owner write is SQL that can make a live dispatch belong to a run: inserting a
dispatch or a phase-gate row, setting a phase-gate row's `dispatch_id`/`run_id`, setting a
dispatch's `status` to a parameter or a live literal, or linking an entry's
`dispatch_id`. For UPDATE patterns only the statement's SET clause counts, so a
terminal writer whose WHERE clause compares `status = $3` is not an owner write.

Bounds: Rust string literals are read with `scripts/rust_lex.py` on a length-preserving
mask, so comments and `#[cfg(test)]` blocks never shift literal offsets. Whole-file test
modules follow the shared skip pin in `scripts/test_only_module_skip_pin.py`. SQL
assembled from fragments (`format!` arguments, macros, `concat!`) or naming a table
through an alias is not seen; no current owner write is built that way. `policies/`
JavaScript is matched on its full text, comments included.
"""

from __future__ import annotations

import importlib.util
import re
import sys
from collections import defaultdict
from collections.abc import Iterable
from pathlib import Path

try:
    from rust_lex import COMMENT, LITERAL, StripState, lex_segments
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_lex import COMMENT, LITERAL, StripState, lex_segments

ROOT = Path(__file__).resolve().parents[1]
SCAN_ROOT = Path("src")
GUARD_WRITES = "src/db/auto_queue/run_owner_guard/writes.rs"

INSERT_DISPATCH = "INSERT task_dispatches"
INSERT_GATE = "INSERT auto_queue_phase_gates"
UPDATE_GATE_OWNER = "UPDATE auto_queue_phase_gates owner column"
UPDATE_DISPATCH_LIVE = "UPDATE task_dispatches status to parameter or live"
UPDATE_ENTRY_DISPATCH = "UPDATE auto_queue_entries dispatch_id parameter"
INSERT_ENTRY_DISPATCH = "INSERT auto_queue_entries with dispatch_id"

_I = re.IGNORECASE
INSERTS = {
    INSERT_DISPATCH: (re.compile(r"\bINSERT\s+INTO\s+task_dispatches\b", _I), None),
    INSERT_GATE: (re.compile(r"\bINSERT\s+INTO\s+auto_queue_phase_gates\b", _I), None),
    INSERT_ENTRY_DISPATCH: (
        re.compile(r"\bINSERT\s+INTO\s+auto_queue_entries\b", _I),
        re.compile(r"\bdispatch_id\b", _I),
    ),
}
UPDATES = {
    UPDATE_GATE_OWNER: (
        re.compile(r"\bUPDATE\s+auto_queue_phase_gates\b", _I),
        re.compile(r"\b(?:dispatch_id|run_id)\s*=", _I),
    ),
    UPDATE_DISPATCH_LIVE: (
        re.compile(r"\bUPDATE\s+task_dispatches\b", _I),
        re.compile(r"\bstatus\s*=\s*(?:\$\d+|'pending'|'dispatched')", _I),
    ),
    UPDATE_ENTRY_DISPATCH: (
        re.compile(r"\bUPDATE\s+auto_queue_entries\b", _I),
        re.compile(r"\bdispatch_id\s*=\s*\$\d+", _I),
    ),
}
SET_RE = re.compile(r"\bSET\b", _I)
SET_END_RE = re.compile(r"\b(?:WHERE|RETURNING|FROM)\b", _I)
CFG_EXCLUSIVELY_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\s*(?:,|\)))")

# (file, pattern) -> (exact count, reason). Each site already serializes with cancel
# or is not an owner publication.
EXPECTED_EXCEPTIONS: dict[tuple[str, str], tuple[int, str]] = {
    ("src/services/auto_queue/phase_gate.rs", INSERT_DISPATCH): (
        1,
        "activate create holds aq_run, re-checks the run and locks entries first",
    ),
    ("src/db/auto_queue/entries.rs", UPDATE_ENTRY_DISPATCH): (
        3,
        "dispatched arms pass the aq_run entry gate; lineage restore writes a terminal entry",
    ),
    ("src/server/task_dispatch_claims.rs", UPDATE_DISPATCH_LIVE): (
        1,
        "claim moves pending to dispatched under FOR UPDATE SKIP LOCKED, not a revival",
    ),
    ("src/db/dispatches/outbox/delivery.rs", UPDATE_DISPATCH_LIVE): (
        1,
        "delivery moves pending to dispatched behind a status CAS, not a revival",
    ),
}


def _load_skip_pin_module():
    name = "test_only_module_skip_pin"
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).resolve().parent / "test_only_module_skip_pin.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/test_only_module_skip_pin.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


_SKIP_PIN = _load_skip_pin_module()
PINNED_TEST_ONLY_MODULE_FILES = _SKIP_PIN.PINNED_TEST_ONLY_MODULE_FILES


def lex_rust(text: str) -> tuple[str, list[tuple[int, int, str]]]:
    """Return a same-length mask (literals and comments blanked) and the literals.

    A string literal continued across lines is one entry: (start offset, line, text).
    """
    state = StripState()
    mask: list[str] = []
    literals: list[list] = []
    offset = 0
    lines = text.split("\n")
    for lineno, line in enumerate(lines, 1):
        continued = state.in_string or state.raw_hashes is not None
        for index, (kind, segment) in enumerate(lex_segments(line, state)):
            if kind == LITERAL:
                if index == 0 and continued and literals:
                    literals[-1][2] += "\n" + segment
                else:
                    literals.append([offset, lineno, segment])
            mask.append(" " * len(segment) if kind in (LITERAL, COMMENT) else segment)
            offset += len(segment)
        if lineno < len(lines):
            mask.append("\n")
            offset += 1
    return "".join(mask), [(start, line, body) for start, line, body in literals]


def test_spans(mask: str) -> list[tuple[int, int]]:
    """Offsets of every `#[cfg(test)]` / `#[cfg(all(test, ..))]` braced item."""
    spans = []
    for match in CFG_EXCLUSIVELY_TEST_RE.finditer(mask):
        brace = mask.find("{", match.end())
        semicolon = mask.find(";", match.end())
        if brace == -1 or (semicolon != -1 and semicolon < brace):
            continue
        depth = 0
        for index in range(brace, len(mask)):
            if mask[index] == "{":
                depth += 1
            elif mask[index] == "}":
                depth -= 1
                if depth == 0:
                    spans.append((match.start(), index))
                    break
    return spans


def set_clause(sql: str, start: int) -> str:
    """The top-level SET clause of the UPDATE starting at `start`, or ''."""
    match = SET_RE.search(sql, start)
    if match is None:
        return ""
    depth = 0
    for index in range(match.end(), len(sql)):
        char = sql[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth < 0:
                return sql[match.end() : index]
        elif char == ";" and depth == 0:
            return sql[match.end() : index]
        elif depth == 0 and SET_END_RE.match(sql, index) and not sql[index - 1].isalnum():
            return sql[match.end() : index]
    return sql[match.end() :]


def owner_writes(sql: str) -> list[str]:
    """Owner-write pattern names in one SQL string, once per statement hit."""
    found = []
    for name, (head, column) in INSERTS.items():
        for match in head.finditer(sql):
            if column is None or column.search(sql, match.end()):
                found.append(name)
    for name, (head, column) in UPDATES.items():
        for match in head.finditer(sql):
            if column.search(set_clause(sql, match.end())):
                found.append(name)
    return found


def scan(
    root: Path,
    *,
    pinned_test_only_files: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> dict[tuple[str, str], list[int]]:
    """Owner-write hits outside the guard writes, keyed by (file, pattern) -> lines."""
    hits: defaultdict[tuple[str, str], list[int]] = defaultdict(list)
    files, whole_file_skips = _SKIP_PIN.validated_scan_files(
        root, SCAN_ROOT, _SKIP_PIN.is_basename_test_file, pinned_paths=pinned_test_only_files
    )
    for path in files:
        rel = path.relative_to(root).as_posix()
        if path in whole_file_skips or rel == GUARD_WRITES:
            continue
        mask, literals = lex_rust(path.read_text(encoding="utf-8"))
        spans = test_spans(mask)
        for start, line, body in literals:
            if any(low <= start <= high for low, high in spans):
                continue
            for name in owner_writes(body):
                hits[(rel, name)].append(line)
    policies = root / "policies"
    for path in sorted(policies.rglob("*.js")) if policies.is_dir() else []:
        if "__tests__" in path.relative_to(policies).parts:
            continue
        text = path.read_text(encoding="utf-8")
        for name in owner_writes(text):
            hits[(path.relative_to(root).as_posix(), name)].append(0)
    return dict(hits)


def violations(
    hits: dict[tuple[str, str], list[int]],
    expected: dict[tuple[str, str], tuple[int, str]] = EXPECTED_EXCEPTIONS,
) -> list[str]:
    errors = []
    for key in sorted(set(hits) | set(expected)):
        lines = hits.get(key, [])
        want = expected.get(key, (0, ""))[0]
        if len(lines) != want:
            where = ", ".join(f"{key[0]}:{line}" for line in lines) or key[0]
            errors.append(f"{key[1]}: {len(lines)} raw owner write(s), expected {want}: {where}")
    return errors


def main() -> int:
    try:
        errors = violations(scan(ROOT))
    except RuntimeError as error:
        print(error, file=sys.stderr)
        return 1
    for error in errors:
        print(f"FAIL: {error}", file=sys.stderr)
    if errors:
        print(
            "Route the write through src/db/auto_queue/run_owner_guard.rs, or pin a site that "
            "already serializes with cancel in EXPECTED_EXCEPTIONS with its reason.",
            file=sys.stderr,
        )
        return 1
    print(f"run-owner raw writer allowlist OK ({len(EXPECTED_EXCEPTIONS)} pinned sites)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
