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
    from rust_cfg import find_test_only_cfg_attribute, find_test_only_cfg_attributes
except ModuleNotFoundError:  # imported from a repo-root unittest
    from scripts.rust_cfg import find_test_only_cfg_attribute, find_test_only_cfg_attributes


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


def collect_removed_lines() -> list[AddedLine]:
    seen: set[tuple[str, int, str]] = set()
    lines: list[AddedLine] = []
    for spec in diff_specs():
        result = run_git(["diff", "--unified=0", "--no-ext-diff", *spec])
        if result.returncode not in (0, 1):
            print(result.stderr, file=sys.stderr)
            sys.exit(result.returncode)
        for removed in parse_removed_lines(result.stdout):
            key = (removed.path, removed.line_no, removed.text)
            if key not in seen:
                seen.add(key)
                lines.append(removed)
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


def strip_rust_non_code(source: str, keep_strings: bool = False) -> str:
    """Blank comments, strings and char literals; ``keep_strings`` leaves string text in place."""
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
                result.extend(closer if keep_strings else " " * len(closer))
                idx += len(closer)
            else:
                result.append(source[idx] if keep_strings or source[idx] == "\n" else " ")
                idx += 1
            continue
        if in_string:
            if source[idx] == "\\" and idx + 1 < len(source):
                if keep_strings:
                    result.extend(source[idx : idx + 2])
                else:
                    result.extend(" \n" if source[idx + 1] == "\n" else "  ")
                idx += 2
            else:
                if source[idx] == '"':
                    in_string = False
                result.append(source[idx] if keep_strings or source[idx] == "\n" else " ")
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
            opener = source[idx : raw_string.end()]
            result.extend(opener if keep_strings else " " * len(opener))
            idx = raw_string.end()
            continue
        if source[idx] == '"':
            in_string = True
            result.append('"' if keep_strings else " ")
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
MUTANT_HOOK_ROOTS = ("src/", "tests/")
MUTANT_HOOK_SETTER_ROOTS = ("src", "tests", "scripts", ".github")
MUTANT_ENV_RE = re.compile(r'"([A-Z][A-Z0-9_]*(?:MUTANT|MUTATION)[A-Z0-9_]*)"')
MUTANT_FN_DEF_RE = re.compile(r"\bfn\s+((?:[a-z0-9_]*_)?mutant(?:_[a-z0-9_]+)?)\s*[(<]")
# Calls into the hooks main still carries; drop this rule once S4b1 lands and no hook remains to call.
MUTANT_CALL_RE = re.compile(
    r"\b((?:[a-z0-9_]*_)?mutant(?:_[a-z0-9_]+)?|behavior_mutation_enabled|parser_bypassed)\s*\("
)
ENV_READ_RE = re.compile(r'\b(?:env::var(?:_os)?|option_env!|env!)\s*\(\s*"([A-Z][A-Z0-9_]*)"')
RUNTIME_ENV_READ_RE = re.compile(r'\b(?:env::var(?:_os)?|option_env!)\s*\(\s*"([A-Z][A-Z0-9_]*)"')
# (path, env) pairs a committed test really arms; each row needs a one-line reason comment.
MUTANT_HOOK_ALLOWED: frozenset[tuple[str, str]] = frozenset()
MUTANT_HOOK_HINT = (
    "test switches stay out of committed code (docs/agent-maintenance/test-policy.md rule 3): "
    "prove the mutant in the lane, revert it, and inject the situation through an argument"
)

_LEXED_CACHE: dict[tuple[str, bool], tuple[list[str], list[str]] | None] = {}
_TEST_ITEM_CACHE: dict[str, set[int]] = {}
_SETTER_TEXTS: list[tuple[str, str]] | None = None


def is_mutant_hook_path(path: str) -> bool:
    return path.endswith(".rs") and path.startswith(MUTANT_HOOK_ROOTS)


def _lexed_line(path: str, line_no: int, text: str, keep_strings: bool) -> str:
    """The added line with comments blanked, lexed in its file when the file still holds it."""
    key = (path, keep_strings)
    if key not in _LEXED_CACHE:
        try:
            source = Path(path).read_text(encoding="utf-8")
        except OSError:
            _LEXED_CACHE[key] = None
        else:
            _LEXED_CACHE[key] = (
                source.splitlines(),
                strip_rust_non_code(source, keep_strings=keep_strings).splitlines(),
            )
    lexed = _LEXED_CACHE[key]
    if lexed is not None and line_no <= len(lexed[0]) and lexed[0][line_no - 1] == text:
        return lexed[1][line_no - 1]
    return strip_rust_non_code(text, keep_strings=keep_strings)


def test_item_lines(path: str) -> set[int]:
    """Lines of items and statements under a test-only cfg attribute."""
    if path in _TEST_ITEM_CACHE:
        return _TEST_ITEM_CACHE[path]
    result: set[int] = set()
    try:
        code = strip_rust_non_code(Path(path).read_text(encoding="utf-8"))
    except OSError:
        _TEST_ITEM_CACHE[path] = result
        return result
    line_of = lambda offset: code.count("\n", 0, offset) + 1  # noqa: E731
    for attribute in find_test_only_cfg_attributes(code):
        if attribute.is_inner:
            result.update(range(1, code.count("\n") + 2))
            break
        # The gated item ends at its closing brace (unless `else` follows), or at `;`/`,` outside brackets.
        braces, brackets, end = 0, 0, len(code)
        for idx in range(attribute.attribute_end(), len(code)):
            char = code[idx]
            if char in "([":
                brackets += 1
            elif char in ")]":
                brackets -= 1
            elif char == "{":
                braces += 1
            elif char == "}":
                braces -= 1
                if braces < 0 or (braces == 0 and not code[idx + 1 :].lstrip().startswith("else")):
                    end = idx
                    break
            elif char in ";," and braces == 0 and brackets <= 0:
                end = idx
                break
        result.update(range(line_of(attribute.start()), line_of(end) + 1))
    _TEST_ITEM_CACHE[path] = result
    return result


def _setter_texts() -> list[tuple[str, str]]:
    global _SETTER_TEXTS
    if _SETTER_TEXTS is None:
        _SETTER_TEXTS = []
        for root in MUTANT_HOOK_SETTER_ROOTS:
            base = Path(root)
            if not base.is_dir():
                continue
            for path in sorted(base.rglob("*")):
                if not path.is_file() or "__pycache__" in path.parts:
                    continue
                try:
                    _SETTER_TEXTS.append((path.as_posix(), path.read_text(encoding="utf-8")))
                except (OSError, UnicodeDecodeError):
                    continue
    return _SETTER_TEXTS


def env_is_armed_somewhere(name: str) -> bool:
    """True when src, tests, scripts or .github name the env in anything but a read."""
    quoted = f'"{name}"'
    for path, text in _setter_texts():
        if name not in text:
            continue
        if not path.endswith(".rs"):
            return True
        for line in text.splitlines():
            reads = sum(1 for read in ENV_READ_RE.findall(line) if read == name)
            if line.count(quoted) > reads:
                return True
    return False


def _hook_names(code: str, with_strings: str) -> list[tuple[str, str]]:
    defined = MUTANT_FN_DEF_RE.findall(code)
    return (
        [("new mutant env", env) for env in MUTANT_ENV_RE.findall(with_strings)]
        + [("new mutant hook fn", name) for name in defined]
        + [("new call to mutant hook", name) for name in MUTANT_CALL_RE.findall(code) if name not in defined]
    )


def audit_mutant_hooks(
    lines: list[AddedLine], base_resolved: bool, removed: list[AddedLine] | None = None
) -> list[str]:
    """Rules A (env name), B (hook fn) and D (hook call) count net additions per file, so a line
    that only moves or reformats an existing hook passes; rule C checks test-only env reads."""
    findings: list[str] = []
    if not base_resolved:
        return [
            "mutant-hook audit found no diff base (AGENTDESK_AUDIT_BASE, origin/main, main, HEAD^); "
            "set AGENTDESK_AUDIT_BASE instead of passing without the committed diff"
        ]
    # Removed lines are gone from the tree, so they are lexed alone.
    removed_names: collections.Counter[tuple[str, str, str]] = collections.Counter(
        (gone.path, kind, name)
        for gone in removed or []
        if is_mutant_hook_path(gone.path)
        for kind, name in _hook_names(
            strip_rust_non_code(gone.text), strip_rust_non_code(gone.text, keep_strings=True)
        )
    )
    for added in lines:
        if not is_mutant_hook_path(added.path):
            continue
        with_strings = _lexed_line(added.path, added.line_no, added.text, keep_strings=True)
        code = _lexed_line(added.path, added.line_no, added.text, keep_strings=False)
        where = f"{added.path}:{added.line_no}"
        for kind, name in _hook_names(code, with_strings):
            if kind == "new mutant env" and (added.path, name) in MUTANT_HOOK_ALLOWED:
                continue
            if removed_names[(added.path, kind, name)] > 0:
                removed_names[(added.path, kind, name)] -= 1
                continue
            findings.append(f"{where}: {kind} `{name}`; {MUTANT_HOOK_HINT}")
        # Rule C covers test-only branches of production functions; inline test modules are tests.
        if is_test_path(added.path) or is_test_region(added.path, added.line_no):
            continue
        gated = "cfg!(test)" in code.replace(" ", "") or added.line_no in test_item_lines(added.path)
        if not gated:
            continue
        for env in RUNTIME_ENV_READ_RE.findall(with_strings):
            if MUTANT_ENV_RE.fullmatch(f'"{env}"') or (added.path, env) in MUTANT_HOOK_ALLOWED:
                continue
            if not env_is_armed_somewhere(env):
                findings.append(
                    f"{where}: test-only read of `{env}`, which nothing in src, tests, scripts "
                    f"or .github sets; {MUTANT_HOOK_HINT}"
                )
    return findings


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
