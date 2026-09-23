#!/usr/bin/env python3
"""Ratchet checks for #2872 state/lint hardening.

The guard is intentionally diff-based: it blocks newly introduced hazards in
selected production modules without forcing the branch to clean up every legacy
instance at once.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

try:
    from rust_cfg import find_test_only_cfg_attribute
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_cfg import find_test_only_cfg_attribute


PRODUCTION_PREFIXES = (
    "src/db/",
    "src/dispatch/",
    "src/engine/",
    "src/kanban/",
    "src/server/routes/dispatches/",
    "src/server/task_dispatch_claims.rs",
    "src/services/auto_queue/",
    "src/services/dispatches/",
    "src/services/discord/outbound/",
    "src/services/issue_announcements.rs",
    "src/supervisor/",
)

UNWRAP_PATTERNS = (
    (re.compile(r"\.(unwrap|expect)\s*\("), "unwrap/expect"),
    (re.compile(r"\bpanic!\s*\("), "panic!"),
)

INTEGER_COLUMN_RE = re.compile(
    r"(?i)^\s*(?:ALTER\s+TABLE\s+\S+\s+)?(?:ADD\s+COLUMN\s+(?:IF\s+NOT\s+EXISTS\s+)?)?"
    r"([a-z_][a-z0-9_]*)\s+(INTEGER|SERIAL)\b"
)

RISKY_INTEGER_NAMES = {
    "id",
    "attempt",
    "attempt_no",
    "batch_phase",
    "chain_depth",
    "created_at",
    "depth",
    "duration_ms",
    "effective_rounds",
    "item_index",
    "offset",
    "phase",
    "priority_rank",
    "review_round",
    "round",
    "seq",
    "sort_order",
    "stage_order",
    "tokens",
    "updated_at",
    "xp",
}

RISKY_INTEGER_SUFFIXES = (
    "_id",
    "_ids",
    "_count",
    "_tokens",
    "_bytes",
    "_duration_ms",
    "_offset",
    "_seq",
)

MODULE_DECLARATION_RE = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{"
)
RAW_STRING_OPEN_RE = re.compile(r'(?:b|c)?r(#*)"')
CHAR_LITERAL_RE = re.compile(
    r"'(?:\\(?:u\{[0-9a-fA-F_]{1,6}\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'"
)

_TEST_REGION_CACHE: dict[str, set[int]] = {}


@dataclass(frozen=True)
class AddedLine:
    path: str
    line_no: int
    text: str


def run_git(args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def merge_base() -> str | None:
    configured = os.environ.get("AGENTDESK_AUDIT_BASE", "").strip()
    if configured:
        return configured
    for candidate in ("origin/main", "main"):
        result = run_git(["merge-base", candidate, "HEAD"])
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()
    result = run_git(["rev-parse", "HEAD^"])
    if result.returncode == 0 and result.stdout.strip():
        return result.stdout.strip()
    return None


def diff_specs() -> list[list[str]]:
    specs: list[list[str]] = []
    base = merge_base()
    if base:
        specs.append([f"{base}...HEAD"])
    specs.append(["--cached"])
    specs.append(["HEAD"])
    return specs


def parse_added_lines(diff_text: str) -> list[AddedLine]:
    added: list[AddedLine] = []
    path: str | None = None
    new_line: int | None = None
    for raw in diff_text.splitlines():
        if raw.startswith("+++ b/"):
            path = raw[6:]
            continue
        if raw.startswith("+++ /dev/null"):
            path = None
            continue
        if raw.startswith("@@"):
            match = re.search(r"\+(\d+)(?:,(\d+))?", raw)
            new_line = int(match.group(1)) if match else None
            continue
        if path is None or new_line is None:
            continue
        if raw.startswith("+") and not raw.startswith("+++"):
            added.append(AddedLine(path, new_line, raw[1:]))
            new_line += 1
            continue
        if raw.startswith("-") and not raw.startswith("---"):
            continue
        new_line += 1
    return added


def collect_added_lines() -> list[AddedLine]:
    seen: set[tuple[str, int, str]] = set()
    lines: list[AddedLine] = []
    for spec in diff_specs():
        result = run_git(["diff", "--unified=0", "--no-ext-diff", *spec])
        if result.returncode not in (0, 1):
            print(result.stderr, file=sys.stderr)
            sys.exit(result.returncode)
        for added in parse_added_lines(result.stdout):
            key = (added.path, added.line_no, added.text)
            if key not in seen:
                seen.add(key)
                lines.append(added)
    untracked = run_git(["ls-files", "--others", "--exclude-standard"])
    if untracked.returncode == 0:
        for path in untracked.stdout.splitlines():
            if not (
                is_selected_production_path(path)
                or (path.startswith("migrations/postgres/") and path.endswith(".sql"))
            ):
                continue
            try:
                file_lines = Path(path).read_text(encoding="utf-8").splitlines()
            except OSError:
                continue
            for line_no, text in enumerate(file_lines, start=1):
                key = (path, line_no, text)
                if key not in seen:
                    seen.add(key)
                    lines.append(AddedLine(path, line_no, text))
    return lines


def is_test_path(path: str) -> bool:
    name = Path(path).name
    return (
        "/tests/" in path
        or "/test_support" in path
        or name in {"tests.rs", "integration_tests.rs"}
        or name.endswith("_tests.rs")
        or name.endswith("_test.rs")
    )


def strip_rust_non_code(source: str) -> str:
    result: list[str] = []
    block_comment_depth = 0
    in_string = False
    raw_string_hashes: int | None = None
    idx = 0
    while idx < len(source):
        if block_comment_depth:
            if source.startswith("/*", idx):
                block_comment_depth += 1
                result.extend("  ")
                idx += 2
            elif source.startswith("*/", idx):
                block_comment_depth -= 1
                result.extend("  ")
                idx += 2
            else:
                result.append("\n" if source[idx] == "\n" else " ")
                idx += 1
            continue
        if raw_string_hashes is not None:
            closer = '"' + "#" * raw_string_hashes
            if source.startswith(closer, idx):
                raw_string_hashes = None
                result.extend(" " * len(closer))
                idx += len(closer)
            else:
                result.append("\n" if source[idx] == "\n" else " ")
                idx += 1
            continue
        if in_string:
            if source[idx] == "\\" and idx + 1 < len(source):
                result.extend(" \n" if source[idx + 1] == "\n" else "  ")
                idx += 2
            else:
                if source[idx] == '"':
                    in_string = False
                result.append("\n" if source[idx] == "\n" else " ")
                idx += 1
            continue

        if source.startswith("//", idx):
            end = source.find("\n", idx)
            if end < 0:
                result.extend(" " * (len(source) - idx))
                break
            result.extend(" " * (end - idx))
            idx = end
            continue
        if source.startswith("/*", idx):
            block_comment_depth = 1
            result.extend("  ")
            idx += 2
            continue
        raw_string = RAW_STRING_OPEN_RE.match(source, idx)
        if raw_string:
            raw_string_hashes = len(raw_string.group(1))
            result.extend(" " * (raw_string.end() - idx))
            idx = raw_string.end()
            continue
        if source[idx] == '"':
            in_string = True
            result.append(" ")
            idx += 1
            continue
        if source[idx] == "'":
            char_literal = CHAR_LITERAL_RE.match(source, idx)
            if char_literal:
                result.extend(" " * (char_literal.end() - idx))
                idx = char_literal.end()
                continue
        result.append(source[idx])
        idx += 1
    return "".join(result)


def test_region_lines(path: str) -> set[int]:
    """Return lines inside lexically test-only inline modules.

    A cfg/cfg_attr gate counts only when the shared Boolean classifier proves
    that it cannot enable the module with ``test`` disabled.  Merely mentioning
    the token is insufficient: ``not(test)``, ``any(test, unix)``, string
    values such as ``feature = "test-tools"``, and non-gating forms such as
    ``cfg_attr(test, allow(dead_code))`` remain production-visible.  For
    ``cfg_attr``, only nested cfg/cfg_attr payloads contribute gating semantics.

    This is deliberately a lexical inline-module classifier, not rustc cfg
    evaluation.  Unsupported/malformed predicates and attributes spanning
    multiple source lines stay production-visible.
    """

    if path in _TEST_REGION_CACHE:
        return _TEST_REGION_CACHE[path]
    result: set[int] = set()
    try:
        source = Path(path).read_text(encoding="utf-8")
    except OSError:
        _TEST_REGION_CACHE[path] = result
        return result
    code_lines = strip_rust_non_code(source).splitlines()

    depth = 0
    active_depth: int | None = None
    pending_cfg_test_depth: int | None = None
    for idx, code_line in enumerate(code_lines, start=1):
        if active_depth is not None:
            result.add(idx)

        stripped = code_line.strip()
        opens = code_line.count("{")
        closes = code_line.count("}")
        module_text = stripped
        cfg_test_attribute = find_test_only_cfg_attribute(module_text)
        if cfg_test_attribute is not None and cfg_test_attribute.start() != 0:
            cfg_test_attribute = None
        if cfg_test_attribute:
            module_text = module_text[cfg_test_attribute.attribute_end() :].lstrip()
        module_declaration = MODULE_DECLARATION_RE.match(module_text)
        starts_test_module = module_declaration and (
            module_declaration.group(1) == "tests"
            or cfg_test_attribute is not None
            or pending_cfg_test_depth == depth
        )
        depth_after = depth + opens - closes
        if active_depth is None and starts_test_module:
            active_depth = depth + 1
            pending_cfg_test_depth = None
            result.add(idx)
        elif active_depth is None:
            if cfg_test_attribute is not None and not module_text:
                pending_cfg_test_depth = depth
            elif module_declaration is not None:
                pending_cfg_test_depth = None
            elif stripped and not stripped.startswith(("#[", "//")):
                pending_cfg_test_depth = None

        depth = depth_after
        if active_depth is not None and depth < active_depth:
            active_depth = None

    _TEST_REGION_CACHE[path] = result
    return result


def is_test_region(path: str, line_no: int) -> bool:
    return line_no in test_region_lines(path)


def is_selected_production_path(path: str) -> bool:
    return path.endswith(".rs") and any(path.startswith(prefix) for prefix in PRODUCTION_PREFIXES)


def risky_integer_column(column: str) -> bool:
    return column in RISKY_INTEGER_NAMES or any(
        column.endswith(suffix) for suffix in RISKY_INTEGER_SUFFIXES
    )


def audit_unwrap_panic(lines: list[AddedLine]) -> list[str]:
    findings: list[str] = []
    for added in lines:
        if (
            not is_selected_production_path(added.path)
            or is_test_path(added.path)
            or is_test_region(added.path, added.line_no)
        ):
            continue
        stripped = added.text.strip()
        if not stripped or stripped.startswith("//"):
            continue
        if "agentdesk-audit: allow-unwrap" in stripped:
            continue
        for pattern, label in UNWRAP_PATTERNS:
            if pattern.search(added.text):
                findings.append(
                    f"{added.path}:{added.line_no}: new production {label}; "
                    "handle the error or add `agentdesk-audit: allow-unwrap` with rationale"
                )
    return findings


def audit_migration_integers(lines: list[AddedLine]) -> list[str]:
    findings: list[str] = []
    for added in lines:
        if not added.path.startswith("migrations/postgres/") or not added.path.endswith(".sql"):
            continue
        stripped = added.text.strip()
        if not stripped or stripped.startswith("--"):
            continue
        if "agentdesk-audit: allow-int4" in stripped:
            continue
        match = INTEGER_COLUMN_RE.match(added.text)
        if not match:
            continue
        column = match.group(1).lower()
        integer_type = match.group(2).upper()
        if risky_integer_column(column):
            replacement = "BIGSERIAL" if integer_type == "SERIAL" else "BIGINT"
            findings.append(
                f"{added.path}:{added.line_no}: `{column} {integer_type}` is a schema growth hazard; "
                f"use {replacement}, or add `agentdesk-audit: allow-int4` with a bounded-domain rationale"
            )
    return findings


def main() -> int:
    lines = collect_added_lines()
    findings = audit_unwrap_panic(lines) + audit_migration_integers(lines)
    if findings:
        print("State/lint hardening audit failed:")
        for finding in findings:
            print(f"  - {finding}")
        return 1
    print("State/lint hardening audit passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
