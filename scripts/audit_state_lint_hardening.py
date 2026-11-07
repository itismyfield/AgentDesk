#!/usr/bin/env python3
"""Ratchet checks for #2872 state/lint hardening.

The unwrap and integer guards are diff-based: they block newly introduced
hazards in selected production modules without forcing the branch to clean up
every legacy instance at once. The SQL escape guard scans the whole tree because
the only legacy instance is an exempt immutable migration.
"""

from __future__ import annotations

import collections
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


@dataclass(frozen=True)
class RemovedLine:
    path: str
    line_no: int
    text: str
    rev: str | None  # the commit whose file holds the line; None when it could not be resolved


def parse_removed_lines(diff_text: str) -> list[AddedLine]:
    """Lines a diff removes, numbered in the old file."""
    removed: list[AddedLine] = []
    path: str | None = None
    old_line: int | None = None
    for raw in diff_text.splitlines():
        if raw.startswith("--- a/"):
            path = raw[6:]
            continue
        if raw.startswith("--- /dev/null"):
            path = None
            continue
        if raw.startswith("@@"):
            match = re.search(r"-(\d+)(?:,(\d+))?", raw)
            old_line = int(match.group(1)) if match else None
            continue
        if path is None or old_line is None or raw.startswith(("+++", "---")):
            continue
        if raw.startswith("-"):
            removed.append(AddedLine(path, old_line, raw[1:]))
            old_line += 1
        elif not raw.startswith("+"):
            old_line += 1
    return removed


def old_side_rev(spec: list[str]) -> str | None:
    """The commit on the old side of a ``diff_specs`` entry."""
    if spec[0].endswith("...HEAD"):
        result = run_git(["merge-base", spec[0][: -len("...HEAD")], "HEAD"])
        return (result.stdout.strip() or None) if result.returncode == 0 else None
    return "HEAD"


def collect_removed_lines() -> list[RemovedLine]:
    seen: set[tuple[str | None, str, int]] = set()
    lines: list[RemovedLine] = []
    for spec in diff_specs():
        result = run_git(["diff", "--unified=0", "--no-ext-diff", *spec])
        if result.returncode not in (0, 1):
            print(result.stderr, file=sys.stderr)
            sys.exit(result.returncode)
        rev = old_side_rev(spec)
        for removed in parse_removed_lines(result.stdout):
            key = (rev, removed.path, removed.line_no)
            if key not in seen:
                seen.add(key)
                lines.append(RemovedLine(removed.path, removed.line_no, removed.text, rev))
    return lines


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
                or is_mutant_hook_path(path)
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



# Test-policy rule 3: env-armed mutant switches are proved inside a lane and reverted, never committed.
# A switch named without MUTANT/MUTATION and not reached through a hook fn is left to review.
MUTANT_HOOK_ROOTS = ("src/", "tests/")
MUTANT_ENV_RE = re.compile(r"[A-Z][A-Z0-9_]*(?:MUTANT|MUTATION)[A-Z0-9_]*")
MUTANT_FN_DEF_RE = re.compile(r"\bfn\s+((?:[a-z0-9_]*_)?mutant(?:_[a-z0-9_]+)?)\s*[(<]")
# (path, env) pairs a committed test really arms; each row needs a one-line reason comment.
MUTANT_HOOK_ALLOWED: frozenset[tuple[str, str]] = frozenset(
    {
        # Drops only the metadata name: absence evidence and that name come from one binding log.
        (
            "src/services/discord/turn_presence/activity/unbound/binding_metadata.rs",
            "AGENTDESK_N2B_TEST_MUTATION",
        ),
        # The no-core missing-metadata FIFO test arms the binding_metadata.rs switch above.
        (
            "src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests/"
            "auxiliary_tests/joint_owner_tests.rs",
            "AGENTDESK_N2B_TEST_MUTATION",
        ),
    }
)
MUTANT_HOOK_HINT = (
    "test switches stay out of committed code (docs/agent-maintenance/test-policy.md rule 3): "
    "prove the mutant in the lane, revert it, and inject the situation through an argument"
)

HookSnapshot = tuple[list[str], dict[int, list[tuple[str, str]]]]
_HOOK_SNAPSHOTS: dict[tuple[str | None, str], HookSnapshot | None] = {}


def is_mutant_hook_path(path: str) -> bool:
    return path.endswith(".rs") and path.startswith(MUTANT_HOOK_ROOTS)


def hook_occurrences(source: str) -> dict[int, list[tuple[str, str]]]:
    """(kind, name) of every mutant env literal and hook fn, by line, lexed as one file."""
    found: dict[int, list[tuple[str, str]]] = collections.defaultdict(list)
    for text, lines in rust_string_literals(source):
        if lines and MUTANT_ENV_RE.fullmatch(text):
            found[lines[0]].append(("new mutant env", text))
    for line_no, code in enumerate(strip_rust_non_code(source).splitlines(), start=1):
        found[line_no] += [("new mutant hook fn", name) for name in MUTANT_FN_DEF_RE.findall(code)]
    return found


def _hook_snapshot(rev: str | None, path: str) -> HookSnapshot | None:
    """The file's lines and hook occurrences: the working tree when ``rev`` is None, else that commit."""
    key = (rev, path)
    if key not in _HOOK_SNAPSHOTS:
        source: str | None = None
        if rev is None:
            try:
                source = Path(path).read_text(encoding="utf-8")
            except OSError:
                pass
        else:
            shown = run_git(["show", f"{rev}:{path}"])
            source = shown.stdout if shown.returncode == 0 else None
        _HOOK_SNAPSHOTS[key] = None if source is None else (source.splitlines(), hook_occurrences(source))
    return _HOOK_SNAPSHOTS[key]


def _hooks_at(snapshot: HookSnapshot | None, line_no: int, text: str) -> list[tuple[str, str]] | None:
    if snapshot is None or line_no > len(snapshot[0]) or snapshot[0][line_no - 1] != text:
        return None
    return snapshot[1].get(line_no, [])


def audit_mutant_hooks(
    lines: list[AddedLine], base_resolved: bool, removed: list[RemovedLine] | None = None
) -> list[str]:
    """Rules A (env literal) and B (hook fn), counted as net additions per file.

    A removed line offsets an added one only when lexing the old file shows a real hook on it;
    an unreadable old snapshot offsets nothing.
    """
    if not base_resolved:
        return [
            "mutant-hook audit found no diff base (AGENTDESK_AUDIT_BASE, origin/main, main, HEAD^); "
            "set AGENTDESK_AUDIT_BASE instead of passing without the committed diff"
        ]
    offsets: collections.Counter[tuple[str, str, str]] = collections.Counter()
    for gone in removed or []:
        if not is_mutant_hook_path(gone.path) or gone.rev is None:
            continue
        for kind, name in _hooks_at(_hook_snapshot(gone.rev, gone.path), gone.line_no, gone.text) or []:
            offsets[(gone.path, kind, name)] += 1
    findings: list[str] = []
    for added in lines:
        if not is_mutant_hook_path(added.path):
            continue
        hooks = _hooks_at(_hook_snapshot(None, added.path), added.line_no, added.text)
        if hooks is None:
            hooks = hook_occurrences(added.text).get(1, [])
        for kind, name in hooks:
            if kind == "new mutant env" and (added.path, name) in MUTANT_HOOK_ALLOWED:
                continue
            if offsets[(added.path, kind, name)] > 0:
                offsets[(added.path, kind, name)] -= 1
                continue
            findings.append(f"{added.path}:{added.line_no}: {kind} `{name}`; {MUTANT_HOOK_HINT}")
    return findings


# Test-policy rule 2: a test that reads Rust source text is a source-text guard. Added test-code lines
# that read `.rs` text pass only inside a listed guard, or outside any test fn in a file hosting one.
# Not caught: a new test calling an existing helper that reads source, and paths built at run time.
SOURCE_GUARD_ALLOWLIST = Path("scripts/ci/source_text_guard_allowlist.txt")
SOURCE_GUARD_TOOLS = frozenset({"lexer", "direct", "include_str", "scanner"})
SOURCE_GUARD_ROW_RE = re.compile(
    r"(\S+) ((?:[A-Za-z_][A-Za-z0-9_]*::)+([A-Za-z_][A-Za-z0-9_]*)) \| ([^|]*?) \| (.*)"
)
# The lexer is defined here; every other file naming it reads source.
SOURCE_LEXER_DEFINITION = "src/test_support.rs"
SOURCE_LEXER_RE = re.compile(
    r"\b(?:strip_rust_comments|rust_code_mask|braced_item_span|source_guard_support)\b"
)
SOURCE_INCLUDE_RE = re.compile(r"\binclude_str!\s*\(")
# Matched on the raw line; the match must start in code, not in a comment or string.
SOURCE_TREE_RES = (
    re.compile(r"\benv!\(\s*\"CARGO_MANIFEST_DIR\"\s*\)[^;]*\"/?src(?:/[^\"]*)?\""),
    re.compile(r"\b(?:read_to_string|read_dir|read)\(\s*\"(?:\./)?src/[^\"]*\""),
)
# A bare `src` root is a tree read only in a file anchored at the crate root; elsewhere it is a temp dir.
SOURCE_ROOT_RE = re.compile(r"\benv!\(\s*\"CARGO_MANIFEST_DIR\"")
SOURCE_ROOTED_TREE_RES = (
    re.compile(r"\.join\(\s*\"(?:\./)?src\"\s*\)"),
    re.compile(r"\[\s*\"src\"\s*,|,\s*\"src\"\s*[,\]]"),
)
# `.rs` files under a fixtures directory are test data, not source.
SOURCE_FIXTURE_DIR_RE = re.compile(r"(?:^|/)fixtures/")
TEST_ATTRIBUTE_RE = re.compile(r"#\[\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*test\b")
FN_DECLARATION_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
SOURCE_GUARD_HINT = (
    "tests do not read Rust source (docs/agent-maintenance/test-policy.md rule 2): assert behavior, "
    f"or list a kept guard in {SOURCE_GUARD_ALLOWLIST} with coordinator approval"
)


def load_source_guard_allowlist(path: Path = SOURCE_GUARD_ALLOWLIST) -> tuple[set[str], list[str]]:
    """Allowlisted test names and format findings: two `# ` header lines, then one row per test."""
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return set(), [f"{path}: missing; it lists the source-text guards kept on purpose"]
    findings: list[str] = []
    if len(lines) < 2 or not all(line.startswith("# ") for line in lines[:2]):
        findings.append(f"{path}:1: needs its two `# ` header comment lines")
    names: set[str] = set()
    seen: set[tuple[str, str]] = set()
    for line_no, line in enumerate(lines[2:], start=3):
        match = SOURCE_GUARD_ROW_RE.fullmatch(line)
        if match is None:
            findings.append(f"{path}:{line_no}: expected `<crate> <module path>::<test> | <tool> | <reason>`")
            continue
        crate, test_id, name, tool, reason = match.groups()
        if tool not in SOURCE_GUARD_TOOLS:
            findings.append(f"{path}:{line_no}: tool `{tool}` is not one of {sorted(SOURCE_GUARD_TOOLS)}")
        if not reason.strip():
            findings.append(f"{path}:{line_no}: `{test_id}` needs a reason")
        if (crate, test_id) in seen:
            findings.append(f"{path}:{line_no}: `{test_id}` is listed twice")
        seen.add((crate, test_id))
        names.add(name)
    return names, findings


def source_read_lines(source: str) -> dict[int, str]:
    """Lines whose code reads Rust source text, with the kind of read."""
    code = strip_rust_non_code(source)
    tree_patterns = SOURCE_TREE_RES + (SOURCE_ROOTED_TREE_RES if SOURCE_ROOT_RE.search(source) else ())
    found: dict[int, str] = {}
    for line_no, (raw, code_line) in enumerate(zip(source.splitlines(), code.splitlines()), start=1):
        lexer = SOURCE_LEXER_RE.search(code_line)
        if lexer:
            found[line_no] = f"source lexer `{lexer.group(0)}`"
            continue
        for pattern in tree_patterns:
            if any(m.start() < len(code_line) and not code_line[m.start()].isspace() for m in pattern.finditer(raw)):
                found[line_no] = "source tree read"
                break
    for opener in SOURCE_INCLUDE_RE.finditer(code):
        depth, end = 0, None
        for idx in range(opener.end() - 1, len(code)):
            if code[idx] == "(":
                depth += 1
            elif code[idx] == ")":
                depth -= 1
                if depth == 0:
                    end = idx
                    break
        literals = [text for text, _ in rust_string_literals(source[opener.end() : end])]
        if end is None or not any(t.endswith(".rs") and not SOURCE_FIXTURE_DIR_RE.search(t) for t in literals):
            continue
        first, last = code.count("\n", 0, opener.start()) + 1, code.count("\n", 0, end) + 1
        for line_no in range(first, last + 1):
            found.setdefault(line_no, "include_str! of a .rs file")
    return found


def rust_test_functions(source: str) -> list[tuple[int, int, str, bool]]:
    """(first line, last line, name, is a test) of every fn with a body."""
    code = strip_rust_non_code(source)
    functions: list[tuple[int, int, str, bool]] = []
    for decl in FN_DECLARATION_RE.finditer(code):
        body = re.compile(r"[{;]").search(code, decl.end())
        if body is None or body.group(0) == ";":
            continue
        depth, end = 0, len(code) - 1
        for idx in range(body.start(), len(code)):
            if code[idx] == "{":
                depth += 1
            elif code[idx] == "}":
                depth -= 1
                if depth == 0:
                    end = idx
                    break
        item_start = max(code.rfind(ch, 0, decl.start()) for ch in "{};") + 1
        is_test = TEST_ATTRIBUTE_RE.search(code, item_start, decl.start()) is not None
        functions.append(
            (code.count("\n", 0, decl.start()) + 1, code.count("\n", 0, end) + 1, decl.group(1), is_test)
        )
    return functions


def audit_source_text_guards(lines: list[AddedLine], allowlist: Path = SOURCE_GUARD_ALLOWLIST) -> list[str]:
    allowed, findings = load_source_guard_allowlist(allowlist)
    by_path: dict[str, list[AddedLine]] = collections.defaultdict(list)
    for added in lines:
        if is_mutant_hook_path(added.path) and added.path != SOURCE_LEXER_DEFINITION:
            by_path[added.path].append(added)
    for path, added_lines in sorted(by_path.items()):
        try:
            source = Path(path).read_text(encoding="utf-8")
        except OSError:
            source = ""
        file_lines = source.splitlines()
        reads = source_read_lines(source)
        functions = rust_test_functions(source)
        hosts_guard = any(is_test and name in allowed for _, _, name, is_test in functions)
        for added in added_lines:
            current = added.line_no <= len(file_lines) and file_lines[added.line_no - 1] == added.text
            kind = reads.get(added.line_no) if current else source_read_lines(added.text).get(1)
            if kind is None:
                continue
            enclosing = [f for f in functions if current and f[0] <= added.line_no <= f[1]]
            test = next((name for *_, name, is_test in sorted(enclosing, reverse=True) if is_test), None)
            test_code = test is not None or is_test_path(path) or is_test_region(path, added.line_no)
            if not test_code or test is None and hosts_guard or test in allowed:
                continue
            owner = f"test `{test}`" if test else "code outside any allowlisted guard's file"
            findings.append(f"{path}:{added.line_no}: {kind} in {owner}; {SOURCE_GUARD_HINT}")
    return findings


def audit_source_guard_rows(root: Path, allowlist: Path = SOURCE_GUARD_ALLOWLIST) -> list[str]:
    """Every allowlisted name is still a fn under src/ or tests/; a stale row would admit a new guard."""
    allowed, _ = load_source_guard_allowlist(root / allowlist)
    declared: set[str] = set()
    for base in SQL_ESCAPE_RUST_ROOTS:
        if (root / base).is_dir():
            for path in (root / base).rglob("*.rs"):
                declared.update(FN_DECLARATION_RE.findall(path.read_text(encoding="utf-8", errors="replace")))
    return [f"{allowlist}: `{name}` names no fn under src/ or tests/; drop its row" for name in sorted(allowed - declared)]

# PostgreSQL 17 reads E'\v' as U+000B; 16 and older read it as the letter v.
# Probing every printable ASCII `E'\c'` on 16 and 17 found no other difference.
SQL_VERSION_DEPENDENT_ESCAPES = {"v": "U+000B on PostgreSQL 17, letter v on 16 and older; write \\x0b"}
SQL_ESCAPE_EXEMPT_PATHS = frozenset({"migrations/postgres/0100_block_untrusted_deploy_gate.sql"})
SQL_ESCAPE_RUST_ROOTS = ("src", "tests")
SQL_ESCAPE_MIGRATION_DIR = "migrations/postgres"
# A source that cannot yield a backslash before `v` in its SQL text needs no lexing.
SQL_ESCAPE_PREFILTER_RE = re.compile(r"\\(?:v|x5[cC]|u\{0*5[cC]\})")
RUST_STRING_OPEN_RE = re.compile(r'(?:b|c)?r(#*)"|(?:b|c)?"')
RUST_NUMERIC_ESCAPE_RE = re.compile(r"x([0-9a-fA-F]{2})|u\{([0-9a-fA-F_]{1,6})\}")
RUST_ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "\\": "\\", "0": "\0", "'": "'", '"': '"'}


def sql_version_dependent_escapes(sql: str) -> list[tuple[int, str]]:
    """Return (offset, escape letter) for version-dependent escapes inside E'' literals.

    Comments, standard strings and quoted identifiers are skipped; dollar-quoted
    bodies are scanned as SQL because PL/pgSQL re-lexes them with the core scanner.
    """
    found: list[tuple[int, str]] = []
    idx = 0
    length = len(sql)
    while idx < length:
        ch = sql[idx]
        if sql.startswith("--", idx):
            end = sql.find("\n", idx)
            idx = length if end < 0 else end
            continue
        if sql.startswith("/*", idx):
            depth = 1
            idx += 2
            while idx < length and depth:
                if sql.startswith("/*", idx):
                    depth += 1
                    idx += 2
                elif sql.startswith("*/", idx):
                    depth -= 1
                    idx += 2
                else:
                    idx += 1
            continue
        if ch == '"':
            end = idx + 1
            while end < length:
                if sql[end] == '"':
                    if sql.startswith('""', end):
                        end += 2
                        continue
                    break
                end += 1
            idx = end + 1
            continue
        if ch == "'":
            escaped = idx > 0 and sql[idx - 1] in "Ee" and (
                idx < 2 or not (sql[idx - 2].isalnum() or sql[idx - 2] in "_$")
            )
            idx += 1
            while idx < length:
                if escaped and sql[idx] == "\\":
                    if idx + 1 < length and sql[idx + 1] in SQL_VERSION_DEPENDENT_ESCAPES:
                        found.append((idx, sql[idx + 1]))
                    idx += 2
                    continue
                if sql[idx] == "'":
                    if sql.startswith("''", idx):
                        idx += 2
                        continue
                    break
                idx += 1
            idx += 1
            continue
        idx += 1
    return found


def rust_string_literals(source: str) -> list[tuple[str, list[int]]]:
    """Return each Rust string literal's decoded text with a source line per character."""

    literals: list[tuple[str, list[int]]] = []
    idx = 0
    line = 1
    length = len(source)

    def advance(count: int) -> None:
        nonlocal idx, line
        line += source.count("\n", idx, idx + count)
        idx += count

    while idx < length:
        if source.startswith("//", idx):
            end = source.find("\n", idx)
            advance((length if end < 0 else end) - idx)
            continue
        if source.startswith("/*", idx):
            depth = 1
            advance(2)
            while idx < length and depth:
                if source.startswith("/*", idx):
                    depth += 1
                    advance(2)
                elif source.startswith("*/", idx):
                    depth -= 1
                    advance(2)
                else:
                    advance(1)
            continue
        if source[idx] == "'":
            char_literal = CHAR_LITERAL_RE.match(source, idx)
            advance(char_literal.end() - idx if char_literal else 1)
            continue
        opener = None
        if idx == 0 or not (source[idx - 1].isalnum() or source[idx - 1] == "_"):
            opener = RUST_STRING_OPEN_RE.match(source, idx)
        if opener is None:
            advance(1)
            continue
        text: list[str] = []
        lines: list[int] = []
        advance(opener.end() - idx)
        if opener.group(1) is not None:
            closer = '"' + opener.group(1)
            end = source.find(closer, idx)
            end = length if end < 0 else end
            for ch in source[idx:end]:
                text.append(ch)
                lines.append(line)
                if ch == "\n":
                    line += 1
            idx = end
            advance(min(len(closer), length - idx))
        else:
            while idx < length and source[idx] != '"':
                if source[idx] != "\\" or idx + 1 >= length:
                    text.append(source[idx])
                    lines.append(line)
                    advance(1)
                    continue
                start_line = line
                kind = source[idx + 1]
                if kind == "\n":
                    advance(2)
                    while idx < length and source[idx] in " \t\r\n":
                        advance(1)
                    continue
                hex_match = RUST_NUMERIC_ESCAPE_RE.match(source, idx + 1)
                if hex_match:
                    digits = (hex_match.group(1) or hex_match.group(2)).replace("_", "")
                    text.append(chr(int(digits, 16)))
                    advance(hex_match.end() - idx)
                else:
                    text.append(RUST_ESCAPES.get(kind, kind))
                    advance(2)
                lines.append(start_line)
            advance(1)
        literals.append(("".join(text), lines))
    return literals


def sql_escape_targets(root: Path) -> tuple[list[Path], list[Path]]:
    rust = sorted(
        path
        for base in SQL_ESCAPE_RUST_ROOTS
        if (root / base).is_dir()
        for path in (root / base).rglob("*.rs")
    )
    migrations = sorted((root / SQL_ESCAPE_MIGRATION_DIR).glob("*.sql"))
    return rust, migrations


def audit_sql_version_dependent_escapes(root: Path) -> tuple[list[str], int]:
    """Scan runtime Rust SQL and PostgreSQL migrations; return findings and files scanned."""

    rust, migrations = sql_escape_targets(root)
    findings: list[str] = []
    if not rust or not migrations:
        return [
            f"SQL escape audit found {len(rust)} Rust and {len(migrations)} migration files under {root}; "
            "run it from the repository root"
        ], 0

    def report(rel: str, line_no: int, letter: str) -> None:
        findings.append(
            f"{rel}:{line_no}: E'' literal uses \\{letter}, which reads as "
            f"{SQL_VERSION_DEPENDENT_ESCAPES[letter]}"
        )

    for path in migrations:
        rel = path.relative_to(root).as_posix()
        if rel in SQL_ESCAPE_EXEMPT_PATHS:
            continue
        sql = path.read_text(encoding="utf-8", errors="replace")
        if not SQL_ESCAPE_PREFILTER_RE.search(sql):
            continue
        for offset, letter in sql_version_dependent_escapes(sql):
            report(rel, sql.count("\n", 0, offset) + 1, letter)
    for path in rust:
        source = path.read_text(encoding="utf-8", errors="replace")
        if not SQL_ESCAPE_PREFILTER_RE.search(source):
            continue
        rel = path.relative_to(root).as_posix()
        for text, lines in rust_string_literals(source):
            for offset, letter in sql_version_dependent_escapes(text):
                report(rel, lines[offset], letter)
    return findings, len(rust) + len(migrations)


def main() -> int:
    lines = collect_added_lines()
    sql_findings, sql_files = audit_sql_version_dependent_escapes(Path("."))
    findings = (
        audit_unwrap_panic(lines)
        + audit_migration_integers(lines)
        + audit_mutant_hooks(lines, merge_base() is not None, collect_removed_lines())
        + audit_source_text_guards(lines)
        + audit_source_guard_rows(Path("."))
        + sql_findings
    )
    if findings:
        print("State/lint hardening audit failed:")
        for finding in findings:
            print(f"  - {finding}")
        return 1
    print(f"State/lint hardening audit passed (SQL escapes: {sql_files} files scanned)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
