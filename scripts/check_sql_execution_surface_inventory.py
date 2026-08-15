#!/usr/bin/env python3
"""Inventory observed SQL execution surfaces in tracked source inputs.

This bounded lexical inventory finds exact registered API spellings; it is not
a compiler, name resolver, or SQL parser. Classification definitions (also
shown by ``--help``):

STATIC: plain string/raw-string or literal-only concatenation.
UNRESOLVED: variable, format!, macro, template interpolation, function return, or computed table identifier.
STATIC_FILE: whole tracked migration file fingerprint; SQL meaning is not parsed.
NON_SQL_TRACKED: allowlisted tracked migration metadata; content is not interpreted.
GUARD_EXPECTED=blocked: a known static policy write that the current sql_guard rejects.

Success means only that fingerprints were observed in the three enumerated
tracked roots. With ``--check``, it additionally means that the live record set
matches the baseline in both directions. It does not claim that every SQL
writer was found or that runtime writes are impossible. Parenthesized literals
and comment-separated literal concatenation may conservatively remain
UNRESOLVED. Query APIs do not prove read-only SQL, migration deployment state
is unknown, and this inventory neither blocks runtime calls nor repairs
existing guard mismatches.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

try:
    from scripts import check_policy_db_capabilities as policy_scanner
except ModuleNotFoundError:  # direct ``python3 scripts/<tool>.py`` invocation
    import check_policy_db_capabilities as policy_scanner


REPO_ROOT = Path(__file__).resolve().parents[1]
BASELINE_PATH = Path("scripts/sql_execution_surface_inventory.json")
BASELINE_FIELDS = ("root", "kind", "path", "api", "symbol", "classification", "fingerprint")
CLASSIFICATION_DEFINITIONS = """STATIC: plain string/raw-string or literal-only concatenation.
UNRESOLVED: variable, format!, macro, template interpolation, function return, or computed table identifier.
STATIC_FILE: whole tracked migration file fingerprint; SQL meaning is not parsed.
NON_SQL_TRACKED: allowlisted tracked migration metadata; content is not interpreted.
GUARD_EXPECTED=blocked: a known static policy write that the current sql_guard rejects."""
JS_APIS = {"query": "agentdesk.db.query", "execute": "agentdesk.db.execute"}
RUST_API_SQL_ARGUMENT = {
    "sqlx::query": 0, "sqlx::query_as": 0, "sqlx::query_scalar": 0,
    "sqlx::raw_sql": 0, "QueryBuilder::new": 0,
    "db_execute_raw": 1, "db_execute_raw_pg": 1,
    "db_query_raw": 1, "db_query_json_raw": 1, "db_query_raw_pg": 1,
    "db_query_raw_with_json_mode": 1, "db_query_raw_pg_with_json_mode": 1,
    "execute_policy_sql": 1, "prepare_policy_sql_for_pg": 0,
    "translate_insert_with_conflict": 0, "rewrite_insert_conflict": 0,
}
RUST_APIS = tuple(RUST_API_SQL_ARGUMENT)
QUERY_BUILDER_PATTERN = r"QueryBuilder(?:::\s*<[A-Za-z_][A-Za-z0-9_:]*\s*>)?::new"
RUST_API_RE = re.compile(
    r"(?<![A-Za-z0-9_$])(" + "|".join(
        QUERY_BUILDER_PATTERN if api == "QueryBuilder::new" else re.escape(api)
        for api in sorted(RUST_APIS, key=len, reverse=True)
    )
    + r")(?![A-Za-z0-9_$])"
)
MIGRATION_NON_SQL_ALLOWLIST = frozenset({
    "migrations/postgres/checksum-repair-allowlist.json",
    "migrations/postgres/immutable-checksums.json",
})
LIMITS = (
    "enumerated roots are only tracked src/**/*.rs, policies/**, and migrations/postgres/*; migrations/001_initial.sql is outside scope",
    "exact-spelling lexical calls only; no compiler, name resolution, or SQL semantic parsing",
    "unsupported aliases, re-exports, macros, indirection, eval, generated and untracked inputs may be absent",
    "runtime SQL, interpolation, format!/computed identifiers, reachability, commit, and DB state are not proven",
    "STATIC table tokens are observed candidates, not a complete runtime table set; migrations are not SQL-parsed",
    "query API names do not prove read-only SQL or authorize the observed statement",
    "migration deployment state, ordering, and successful application are not proven",
    "this inventory is not a runtime blocker and does not repair existing raw-writer/guard mismatches",
    "parenthesized or comment-separated literals may conservatively remain UNRESOLVED",
)
REPIN_GUIDANCE = (
    "의도된 surface 변경이면: --write-baseline 재실행 → JSON diff 를 커밋에 포함해 "
    "리뷰 표면에 노출 → measured_at_sha 갱신 및 재핀 커밋 → 커밋 후 aggregate 재실행 "
    "(재핀 직후 커밋 전 aggregate dirty guard rc=1은 정상)"
)
REQUIRED_ROOTS = ("src", "policies", "migrations/postgres")
REWRITE_PATH = "src/engine/ops/db_ops.rs"
GUARD_EXPECTED_CONTRACTS = (
    ("rotateActiveRunSweepCursor", "policies/lib/auto-queue-dispatch.js", "auto_queue_entries"),
    ("timeouts._section_E review auto-accept", "policies/timeouts/review-auto-accept.js", "task_dispatches"),
)
TABLE_TOKEN_RE = re.compile(
    r"\b(?:from|join|into|update|delete\s+from|insert\s+into)\s+"
    r"[\"'`]?([A-Za-z_][A-Za-z0-9_$]*(?:\.[A-Za-z_][A-Za-z0-9_$]*)?)[\"'`]?",
    re.IGNORECASE,
)


class InventoryError(RuntimeError):
    pass


@dataclass(frozen=True)
class TrackedInput:
    root: str
    kind: str
    path: Path
    rel_path: str


@dataclass(frozen=True)
class SurfaceRecord:
    root: str
    kind: str
    path: str
    api: str
    symbol: str
    classification: str
    fingerprint: str
    table_tokens: tuple[str, ...] = ()
    line: int | None = None

    def stable_key(self) -> tuple[object, ...]:
        return (self.root, self.kind, self.path, self.api, self.symbol,
                self.classification, self.fingerprint)


def _record(
    tracked: TrackedInput,
    api: str,
    classification: str,
    canonical: str,
    table_tokens: Iterable[str] = (),
    line: int | None = None,
) -> SurfaceRecord:
    tokens = tuple(sorted(set(table_tokens)))
    payload = "\0".join((tracked.root, tracked.kind, tracked.rel_path, api,
                          classification, canonical, ",".join(tokens))).encode()
    return SurfaceRecord(
        tracked.root,
        tracked.kind,
        tracked.rel_path,
        api,
        api.rsplit("::", 1)[-1].rsplit(".", 1)[-1],
        classification,
        "sha256:" + hashlib.sha256(payload).hexdigest(),
        tokens,
        line,
    )


def enumerate_tracked_inputs(repo_root: Path = REPO_ROOT) -> list[TrackedInput]:
    """Return regular tracked inputs in the three design-enumerated roots."""
    repo_root = Path(repo_root).resolve()
    completed = subprocess.run(
        ["git", "ls-files", "-z", "--", "src", "policies", "migrations/postgres"],
        cwd=repo_root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode:
        detail = (completed.stderr or b"").decode("utf-8", "replace").strip()
        raise InventoryError(f"git ls-files failed: {detail or completed.returncode}")
    raw = completed.stdout or b""
    if isinstance(raw, str):
        raw = raw.encode()
    rel_paths = [os.fsdecode(part) for part in raw.split(b"\0") if part]
    inputs: list[TrackedInput] = []
    errors: list[str] = []
    seen: set[str] = set()
    for rel_path in rel_paths:
        if rel_path in seen:
            errors.append(f"duplicate tracked path: {rel_path}")
            continue
        seen.add(rel_path)
        if rel_path.startswith("src/"):
            root, kind, allowed = "src", "RUST", rel_path.endswith(".rs")
        elif rel_path.startswith("policies/"):
            root = "policies"
            kind = "TEST" if rel_path.startswith("policies/__tests__/") else "POLICY"
            allowed = rel_path.endswith((".js", ".yaml", ".yml"))
        elif rel_path.startswith("migrations/postgres/"):
            root = "migrations/postgres"
            if "/" in rel_path[len("migrations/postgres/") :]:
                errors.append(f"nested tracked migration is outside the inventory: {rel_path}")
                continue
            if rel_path.endswith(".sql"):
                kind, allowed = "MIGRATION", True
            else:
                kind = "MIGRATION_METADATA"
                allowed = rel_path in MIGRATION_NON_SQL_ALLOWLIST
        else:
            continue
        path = repo_root / rel_path
        try:
            mode = path.lstat().st_mode
        except OSError as exc:
            errors.append(f"tracked input cannot be inspected: {rel_path}: {exc}")
            continue
        if path.is_symlink() or not stat.S_ISREG(mode):
            errors.append(f"tracked input is not a regular non-symlink file: {rel_path}")
        elif not allowed:
            errors.append(f"unexpected tracked extension under {root}: {rel_path}")
        else:
            inputs.append(TrackedInput(root, kind, path, rel_path))
    if errors:
        raise InventoryError("; ".join(errors))
    return sorted(inputs, key=lambda item: (item.root, item.rel_path, item.kind))


def _take_literal(text: str, start: int, language: str) -> tuple[int, str] | None:
    index = start
    while index < len(text) and text[index].isspace():
        index += 1
    if language == "rust" and index < len(text) and text[index] == "&":
        index += 1
        while index < len(text) and text[index].isspace():
            index += 1
    if language == "rust":
        raw = re.match(r"(?:br|r)(#{0,32})\"", text[index:])
        if raw:
            marker = '"' + raw.group(1)
            body_start = index + raw.end()
            end = text.find(marker, body_start)
            return None if end < 0 else (end + len(marker), text[body_start:end])
        if text.startswith('b"', index):
            index += 1
    quotes = {"'", '"', "`"} if language == "javascript" else {'"'}
    if index >= len(text) or text[index] not in quotes:
        return None
    quote = text[index]
    body_start = index + 1
    index += 1
    escaped = False
    while index < len(text):
        char = text[index]
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == quote:
            body = text[body_start:index]
            if quote == "`" and "${" in body:
                return None
            return index + 1, body
        index += 1
    return None


def _static_sql(argument: str, language: str) -> str | None:
    if language not in {"javascript", "rust"}:
        raise ValueError(f"unsupported SQL argument language: {language}")
    index = 0
    values: list[str] = []
    while True:
        literal = _take_literal(argument, index, language)
        if literal is None:
            return None
        index, value = literal
        values.append(value)
        while index < len(argument) and argument[index].isspace():
            index += 1
        if index == len(argument):
            return "".join(values)
        if argument[index] != "+":
            return None
        index += 1


def classify_sql_argument(argument: str, language: str) -> str:
    return "STATIC" if _static_sql(argument.strip(), language) is not None else "UNRESOLVED"


def _table_tokens(static_sql: str | None) -> tuple[str, ...]:
    if static_sql is None:
        return ()
    return tuple(sorted({match.group(1) for match in TABLE_TOKEN_RE.finditer(static_sql)}))


def scan_js_calls(path: Path | str, repo_root: Path = REPO_ROOT) -> list[SurfaceRecord]:
    repo_root = Path(repo_root).resolve()
    path = Path(path).resolve()
    rel_path = path.relative_to(repo_root).as_posix()
    kind = "TEST" if rel_path.startswith("policies/__tests__/") else "POLICY"
    tracked = TrackedInput("policies", kind, path, rel_path)
    records: list[SurfaceRecord] = []
    occurrences: dict[tuple[str, str], int] = {}
    for callsite in policy_scanner.scan_callsites(path, repo_root):
        api = JS_APIS.get(callsite.op)
        if api is None:
            continue
        argument = policy_scanner.first_call_argument(callsite.expression)
        static_sql = _static_sql(argument.strip(), "javascript")
        classification = "STATIC" if static_sql is not None else "UNRESOLVED"
        canonical = callsite.expression.strip()
        key = api, canonical
        ordinal = occurrences.get(key, 0)
        occurrences[key] = ordinal + 1
        records.append(
            _record(
                tracked,
                api,
                classification,
                f"{canonical}\0occurrence={ordinal}",
                _table_tokens(static_sql),
                callsite.line,
            )
        )
    return records


def _mask_rust(text: str) -> str:
    chars = list(text)
    index = 0
    block_depth = 0
    while index < len(text):
        pair = text[index : index + 2]
        if block_depth:
            if pair == "/*":
                block_depth += 1
                end = index + 2
            elif pair == "*/":
                block_depth -= 1
                end = index + 2
            else:
                end = index + 1
            for offset in range(index, end):
                chars[offset] = "\n" if chars[offset] == "\n" else " "
            index = end
            continue
        if pair == "//":
            end = text.find("\n", index)
            end = len(text) if end < 0 else end
        elif pair == "/*":
            block_depth = 1
            end = index + 2
        else:
            raw = re.match(r"(?:br|r)(#{0,32})\"", text[index:])
            if raw:
                marker = '"' + raw.group(1)
                body_start = index + raw.end()
                found = text.find(marker, body_start)
                end = len(text) if found < 0 else found + len(marker)
            elif text[index] == '"' or pair == 'b"':
                quote_at = index + (1 if pair == 'b"' else 0)
                end = quote_at + 1
                escaped = False
                while end < len(text):
                    if escaped:
                        escaped = False
                    elif text[end] == "\\":
                        escaped = True
                    elif text[end] == '"':
                        end += 1
                        break
                    end += 1
            elif text[index] == "'":
                # Mask compact char literals, but leave Rust lifetimes intact.
                close = text.find("'", index + 1, min(len(text), index + 5))
                if close < 0:
                    index += 1
                    continue
                end = close + 1
            else:
                index += 1
                continue
        for offset in range(index, end):
            chars[offset] = "\n" if chars[offset] == "\n" else " "
        index = end
    return "".join(chars)


def _rust_expression(
    text: str, masked: str, start: int, symbol_end: int, argument_index: int
) -> tuple[str, str] | None:
    index = symbol_end
    while index < len(masked) and masked[index].isspace():
        index += 1
    if masked.startswith("::", index):
        index += 2
        while index < len(masked) and masked[index].isspace():
            index += 1
    if index < len(masked) and masked[index] == "<":
        depth = 0
        while index < len(masked):
            if masked[index] == "<":
                depth += 1
            elif masked[index] == ">":
                depth -= 1
                if depth == 0:
                    index += 1
                    break
            index += 1
        while index < len(masked) and masked[index].isspace():
            index += 1
    if index >= len(masked) or masked[index] != "(":
        return None
    open_paren = index
    parens = brackets = braces = 0
    commas: list[int] = []
    while index < len(masked):
        char = masked[index]
        if char == "(":
            parens += 1
        elif char == ")":
            parens -= 1
            if parens == 0:
                starts = [open_paren + 1] + [comma + 1 for comma in commas]
                ends = commas + [index]
                if argument_index >= len(starts):
                    raise InventoryError("registered SQL argument is absent")
                return text[start : index + 1], text[starts[argument_index] : ends[argument_index]]
        elif char == "[":
            brackets += 1
        elif char == "]":
            brackets = max(0, brackets - 1)
        elif char == "{":
            braces += 1
        elif char == "}":
            braces = max(0, braces - 1)
        elif char == "," and parens == 1 and brackets == braces == 0:
            commas.append(index)
        index += 1
    raise InventoryError(f"unterminated Rust call near byte {open_paren}")


def scan_rust_calls(path: Path | str, repo_root: Path = REPO_ROOT) -> list[SurfaceRecord]:
    repo_root = Path(repo_root).resolve()
    path = Path(path).resolve()
    tracked = TrackedInput("src", "RUST", path, path.relative_to(repo_root).as_posix())
    text = path.read_text(encoding="utf-8")
    if not any(("QueryBuilder" if api == "QueryBuilder::new" else api) in text for api in RUST_APIS):
        return []
    masked = _mask_rust(text)
    records: list[SurfaceRecord] = []
    occurrences: dict[tuple[str, str], int] = {}
    for match in RUST_API_RE.finditer(masked):
        if re.search(r"\bfn\s*$", masked[max(0, match.start() - 40) : match.start()]):
            continue
        spelling = match.group(1)
        api = "QueryBuilder::new" if spelling.startswith("QueryBuilder") else spelling
        found = _rust_expression(
            text, masked, match.start(), match.end(), RUST_API_SQL_ARGUMENT[api]
        )
        if found is None:
            continue
        expression, argument = found
        static_sql = _static_sql(argument.strip(), "rust")
        # QueryBuilder grows through later push calls; its first literal is not
        # a complete statement and therefore cannot establish STATIC.
        if api == "QueryBuilder::new":
            static_sql = None
        classification = "STATIC" if static_sql is not None else "UNRESOLVED"
        canonical = expression.strip()
        key = api, canonical
        ordinal = occurrences.get(key, 0)
        occurrences[key] = ordinal + 1
        records.append(_record(
            tracked, api, classification, f"{canonical}\0occurrence={ordinal}",
            _table_tokens(static_sql), text.count("\n", 0, match.start()) + 1,
        ))
    return records


def scan_migrations(tracked: TrackedInput | Path | str, repo_root: Path = REPO_ROOT) -> list[SurfaceRecord]:
    if not isinstance(tracked, TrackedInput):
        path = Path(tracked).resolve()
        root = Path(repo_root).resolve()
        tracked = TrackedInput(
            "migrations/postgres", "MIGRATION", path, path.relative_to(root).as_posix()
        )
    content_hash = hashlib.sha256(tracked.path.read_bytes()).hexdigest()
    if tracked.kind == "MIGRATION_METADATA":
        return [_record(tracked, "migration.non_sql_tracked", "NON_SQL_TRACKED", content_hash)]
    return [_record(tracked, "migration.file", "STATIC_FILE", content_hash)]


def validate_records(records: Sequence[SurfaceRecord]) -> list[SurfaceRecord]:
    ordered = sorted(records, key=SurfaceRecord.stable_key)
    duplicates = [record for before, record in zip(ordered, ordered[1:])
                  if before.stable_key() == record.stable_key()]
    if duplicates:
        detail = ", ".join(f"{record.path}:{record.api}:{record.fingerprint}" for record in duplicates)
        raise InventoryError("duplicate surface records: " + detail)
    return ordered


def scan_inventory(repo_root: Path = REPO_ROOT) -> list[SurfaceRecord]:
    records: list[SurfaceRecord] = []
    for tracked in enumerate_tracked_inputs(repo_root):
        if tracked.root == "src":
            records.extend(scan_rust_calls(tracked.path, repo_root))
        elif tracked.path.suffix == ".js":
            records.extend(scan_js_calls(tracked.path, repo_root))
        elif tracked.root == "migrations/postgres":
            records.extend(scan_migrations(tracked))
    return validate_records(records)


def baseline_snapshot(records: Sequence[SurfaceRecord], measured_sha: str) -> dict[str, object]:
    if not records:
        raise InventoryError("baseline records must not be empty")
    return {
        "schema_version": 1,
        "measured_at_sha": measured_sha,
        "records": [
            {field: getattr(record, field) for field in BASELINE_FIELDS}
            for record in validate_records(records)
        ],
    }


def write_baseline(path: Path, records: Sequence[SurfaceRecord], measured_sha: str) -> None:
    path.write_text(
        json.dumps(baseline_snapshot(records, measured_sha), indent=2) + "\n",
        encoding="utf-8",
    )


def load_baseline(path: Path) -> dict[str, object]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if data.get("schema_version") != 1 or not re.fullmatch(r"[0-9a-f]{40}", data.get("measured_at_sha", "")):
        raise InventoryError("baseline schema or measured_at_sha is invalid")
    rows = data.get("records")
    if not isinstance(rows, list) or not rows or any(set(row) != set(BASELINE_FIELDS) for row in rows):
        raise InventoryError("baseline records do not match the line-number-free schema")
    keys = [tuple(row[field] for field in BASELINE_FIELDS) for row in rows]
    if keys != sorted(keys) or len(keys) != len(set(keys)):
        raise InventoryError("baseline records are not sorted and unique")
    return data


def baseline_drift(records: Sequence[SurfaceRecord], baseline: dict[str, object]) -> list[str]:
    current = {tuple(getattr(record, field) for field in BASELINE_FIELDS) for record in records}
    expected = {tuple(row[field] for field in BASELINE_FIELDS) for row in baseline["records"]}
    errors = []
    for label, rows in (("GONE", expected - current), ("UNLISTED", current - expected)):
        for row in sorted(rows):
            errors.append(f"baseline {label}: {row[2]} {row[3]} {row[5]} {row[6]}")
    return errors


def cardinality_errors(records: Sequence[SurfaceRecord]) -> list[str]:
    """Enforce a conservative fail-closed floor for both gated CLI modes."""
    errors = []
    if not records:
        errors.append("inventory record set is empty")
    for root in REQUIRED_ROOTS:
        count = sum(record.root == root for record in records)
        if count < 1:
            errors.append(f"inventory root {root} has {count} records; expected at least 1")
    return errors


def _rewrite_dynamic_records(records: Sequence[SurfaceRecord]) -> list[SurfaceRecord]:
    return [
        record for record in records
        if record.path == REWRITE_PATH
        and record.api == "rewrite_insert_conflict"
        and record.classification == "UNRESOLVED"
    ]


def _last_rewrite_table_binding_error(repo_root: Path) -> str | None:
    """Check the current direct, one-line ``table_name`` binding lexically.

    This deliberately narrow vocabulary catches a later direct rebinding in
    ``rewrite_insert_conflict``. It cannot prove Rust data flow or detect an
    indirect bypass through another function; doing that would require the AST
    or broader semantic analysis that this inventory explicitly does not use.
    """
    path = repo_root / REWRITE_PATH
    try:
        source = path.read_text(encoding="utf-8")
    except OSError as exc:
        return f"known blind spot source cannot be read: {REWRITE_PATH}: {exc}"
    masked = _mask_rust(source)
    signature = "fn rewrite_insert_conflict("
    start = masked.find(signature)
    if start < 0:
        return "known blind spot function rewrite_insert_conflict was not observed"
    open_brace = masked.find("{", start + len(signature))
    if open_brace < 0:
        return "known blind spot function rewrite_insert_conflict has no lexical body"
    depth = 0
    body_end = -1
    for index in range(open_brace, len(masked)):
        if masked[index] == "{":
            depth += 1
        elif masked[index] == "}":
            depth -= 1
            if depth == 0:
                body_end = index
                break
    if body_end < 0:
        return "known blind spot function rewrite_insert_conflict has an unterminated lexical body"
    body = masked[open_brace + 1 : body_end]
    bindings = re.findall(
        r"(?m)^[ \t]*let[ \t]+(?:mut[ \t]+)?table_name[ \t]*=[ \t]*([^;\n]+);[ \t]*$",
        body,
    )
    expected = "rest[..table_end].trim()"
    if not bindings or re.sub(r"\s+", "", bindings[-1]) != expected:
        return (
            "known blind spot requires the last direct table_name binding in "
            f"rewrite_insert_conflict to be {expected}"
        )
    return None


def _guard_expected_matches(
    records: Sequence[SurfaceRecord], path: str, table: str
) -> list[SurfaceRecord]:
    return [
        record for record in records
        if record.path == path
        and record.api == "agentdesk.db.execute"
        and record.classification == "STATIC"
        and table in record.table_tokens
    ]


def live_contract_errors(records: Sequence[SurfaceRecord], repo_root: Path) -> list[str]:
    """Pin live known-blind-spot and already-blocked writer observations."""
    errors = []
    dynamic = _rewrite_dynamic_records(records)
    if not dynamic:
        errors.append("known blind spot lost rewrite_insert_conflict UNRESOLVED records")
    binding_error = _last_rewrite_table_binding_error(repo_root)
    if binding_error:
        errors.append(binding_error)
    for symbol, path, table in GUARD_EXPECTED_CONTRACTS:
        matches = _guard_expected_matches(records, path, table)
        if len(matches) != 1:
            errors.append(
                f"GUARD_EXPECTED=blocked {symbol}: expected 1 {path} write to {table}, "
                f"observed {len(matches)}"
            )
    return errors


def _auto_queue_runs_report(records: Sequence[SurfaceRecord], repo_root: Path) -> list[str]:
    lines = ["AUTO_QUEUE_RUNS OBSERVED (not a complete runtime table set):"]
    for root in ("src", "policies"):
        count = sum(record.root == root and "auto_queue_runs" in record.table_tokens for record in records)
        lines.append(f"  - root={root} static_token_records={count}")
    migrations = [
        record for record in records
        if record.kind == "MIGRATION" and b"auto_queue_runs" in (repo_root / record.path).read_bytes()
    ]
    lines.append(f"  - root=migrations/postgres tracked_token_files={len(migrations)} (SQL meaning not parsed)")
    dynamic = _rewrite_dynamic_records(records)
    detail = f"{REWRITE_PATH} rewrite_insert_conflict.table_name" if dynamic else "(not observed)"
    lines.append(f"  - UNRESOLVED dynamic_boundary={detail} records={len(dynamic)}")
    return lines


def _render(
    records: Sequence[SurfaceRecord], errors: Sequence[str] = (), repo_root: Path = REPO_ROOT,
    baseline_status: str = "", verbose: bool = False,
) -> str:
    lines = [
        "SQL execution surface inventory: tracked inputs에서 관측된 fingerprint",
        f"RECORDS: {len(records)}",
    ]
    lines.append("ROOT COUNTS:")
    lines.extend(f"  - {root}={sum(record.root == root for record in records)}" for root in REQUIRED_ROOTS)
    if verbose or errors:
        for record in records:
            tables = ",".join(record.table_tokens) or "-"
            location = f" line={record.line}" if record.line is not None else ""
            guard_expected = ""
            if any(
                record in _guard_expected_matches(records, path, table)
                for _symbol, path, table in GUARD_EXPECTED_CONTRACTS
            ):
                guard_expected = " GUARD_EXPECTED=blocked"
            lines.append(
                f"{record.classification} {record.root}/{record.kind} {record.path} "
                f"{record.api} fingerprint={record.fingerprint} tables={tables}"
                f"{guard_expected}{location}"
            )
    lines.append("UNRESOLVED:")
    unresolved = [record for record in records if record.classification == "UNRESOLVED"]
    lines.extend(
        f"  - {record.path} {record.api} fingerprint={record.fingerprint}"
        for record in unresolved
    )
    if not unresolved:
        lines.append("  - (none observed; absence is not completeness evidence)")
    lines.append("GUARD_EXPECTED=blocked:")
    for symbol, path, table in GUARD_EXPECTED_CONTRACTS:
        matches = _guard_expected_matches(records, path, table)
        lines.append(f"  - {symbol} {path} table={table} records={len(matches)}")
    lines.extend(_auto_queue_runs_report(records, Path(repo_root)))
    if baseline_status:
        lines.append(baseline_status)
    lines.append("LIMITS:")
    lines.extend(f"  - {limit}" for limit in LIMITS)
    if errors:
        lines.append("ERRORS:")
        lines.extend(f"  - {error}" for error in errors)
        if any(error.startswith("baseline GONE:") or error.startswith("baseline UNLISTED:") for error in errors):
            lines.append(REPIN_GUIDANCE)
    return "\n".join(lines) + "\n"


def main(argv: Sequence[str] | None = None, repo_root: Path = REPO_ROOT) -> int:
    class InventoryArgumentParser(argparse.ArgumentParser):
        def error(self, message: str) -> None:
            print(_render((), (f"ArgumentError: {message}",)), file=sys.stderr, end="")
            super().error(message)

    parser = InventoryArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--check", action="store_true", help="compare live observations with the baseline")
    modes.add_argument("--write-baseline", action="store_true", help="record the live observations as the baseline")
    parser.add_argument("--verbose", action="store_true", help="print every inventory record on success")
    args = parser.parse_args(argv)
    root = Path(repo_root).resolve()
    baseline_status = ""
    try:
        records = scan_inventory(root)
        errors = []
        baseline_path = root / BASELINE_PATH
        if args.check or args.write_baseline:
            errors.extend(cardinality_errors(records))
            errors.extend(live_contract_errors(records, root))
        if args.check:
            baseline = load_baseline(baseline_path)
            errors.extend(baseline_drift(records, baseline))
            if not errors:
                baseline_status = "BASELINE: tracked-input observations match the recorded fingerprints"
        elif args.write_baseline and not errors:
            measured_sha = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=root, text=True, capture_output=True, check=True
            ).stdout.strip()
            write_baseline(baseline_path, records, measured_sha)
            baseline_status = f"BASELINE: recorded live observations measured at {measured_sha}"
    except Exception as exc:  # failure output must retain UNRESOLVED and LIMITS
        records = []
        errors = [f"{type(exc).__name__}: {exc}"]
    print(
        _render(records, errors, root, baseline_status, args.verbose),
        file=sys.stderr if errors else sys.stdout,
        end="",
    )
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
