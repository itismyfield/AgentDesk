#!/usr/bin/env python3
"""Inventory observed SQL execution surfaces in tracked source inputs.
This is a bounded lexical inventory, not a compiler, name resolver, or SQL
parser.  It records the exact API spellings in the registry below and keeps
dynamic boundaries visible instead of guessing their runtime SQL.
Classification definitions (also shown by ``--help``):
STATIC: plain string/raw-string or literal-only concatenation.
UNRESOLVED: variable, format!, macro, template interpolation, function return, or computed table identifier.
STATIC_FILE: whole tracked migration file fingerprint; SQL meaning is not parsed.
The successful message is limited to the execution-surface fingerprints
observed in tracked inputs.  It does not claim that every SQL writer was
found, that runtime writes are impossible, or that this lexical sweep is a
global completeness property.
"""
from __future__ import annotations
import argparse
import dataclasses
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
from pathlib import Path
from typing import Iterable, Sequence
# Reuse only these exact balanced-call helpers from the policy scanner:
# scan_callsites, first_call_argument, and extract_call_expression.
try:
    from scripts import check_policy_db_capabilities as policy_db_scanner
except ModuleNotFoundError:  # direct ``python3 scripts/<tool>.py`` invocation
    import check_policy_db_capabilities as policy_db_scanner
REPO_ROOT = Path(__file__).resolve().parents[1]
CLASSIFICATION_DEFINITIONS = """STATIC: plain string/raw-string or literal-only concatenation.
UNRESOLVED: variable, format!, macro, template interpolation, function return, or computed table identifier.
STATIC_FILE: whole tracked migration file fingerprint; SQL meaning is not parsed."""
ROOTS = {
    "src": {"prefix": "src/", "kind": "RUST", "extensions": (".rs",)},
    "policies": {
        "prefix": "policies/",
        "kind": "POLICY",
        "extensions": (".js", ".yaml", ".yml"),
    },
    "migrations/postgres": {
        "prefix": "migrations/postgres/",
        "kind": "MIGRATION",
        "extensions": (".sql",),
    },
}
JS_CALL_REGISTRY = ("agentdesk.db.query", "agentdesk.db.execute")
RUST_CALL_REGISTRY = (
    "sqlx::query",
    "sqlx::query_as",
    "sqlx::query_scalar",
    "QueryBuilder::new",
    "db_execute_raw",
    "db_execute_raw_pg",
    "db_query_raw",
    "execute_policy_sql",
    "prepare_policy_sql_for_pg",
    "translate_insert_with_conflict",
    "rewrite_insert_conflict",
    "db_query_raw_with_json_mode",
    "db_query_raw_pg_with_json_mode",
)
LIMITS = (
    "lexical exact-spelling scan only; no compiler, AST, name-resolution, or SQL semantic parsing",
    "unsupported aliases, re-exports, macros, indirection, eval, generated and untracked inputs may be absent",
    "runtime SQL, template interpolation, format!/computed identifiers, reachability, commit, and DB application state are not proven",
    "STATIC table tokens are observed candidates, not a complete runtime table set; migration files are fingerprinted without SQL parsing",
)
TABLE_TOKEN_RE = re.compile(
    r"\b(?:from|join|into|update|delete\s+from|insert\s+into)\s+"
    r"[\"'`]?([A-Za-z_][A-Za-z0-9_$]*(?:\.[A-Za-z_][A-Za-z0-9_$]*)?)[\"'`]?",
    re.IGNORECASE,
)
RUST_RAW_PREFIX_RE = re.compile(r"(?:br|r)(#{0,32})\"")
class InventoryError(RuntimeError):
    pass
@dataclasses.dataclass(frozen=True)
class TrackedInput:
    root: str
    kind: str
    path: Path
    rel_path: str
@dataclasses.dataclass(frozen=True)
class SurfaceRecord:
    root: str
    kind: str
    path: str
    api: str
    symbol: str
    classification: str
    fingerprint: str
    table_tokens: tuple[str, ...] = ()
    line: int | None = dataclasses.field(default=None, compare=False)
    source_id: str = dataclasses.field(default="", compare=False, repr=False)
    @property
    def stable_key(self) -> tuple[str, str, str, str, str, str]:
        return (
            self.root,
            self.kind,
            self.path,
            self.api,
            self.classification,
            self.fingerprint,
        )
    def as_dict(self) -> dict[str, object]:
        result: dict[str, object] = {
            "root": self.root,
            "kind": self.kind,
            "path": self.path,
            "api": self.api,
            "symbol": self.symbol,
            "classification": self.classification,
            "fingerprint": self.fingerprint,
            "table_tokens": list(self.table_tokens),
        }
        if self.line is not None:
            result["diagnostic_line"] = self.line
        return result
@dataclasses.dataclass(frozen=True)
class InventoryResult:
    records: tuple[SurfaceRecord, ...]
    errors: tuple[str, ...] = ()
    @property
    def unresolved(self) -> tuple[SurfaceRecord, ...]:
        return tuple(record for record in self.records if record.classification == "UNRESOLVED")
def _normalise_expression(text: str) -> str:
    return " ".join(text.split())
def _fingerprint(
    *,
    root: str,
    kind: str,
    path: str,
    api: str,
    classification: str,
    canonical: str,
    table_tokens: Sequence[str],
) -> str:
    payload = json.dumps(
        {
            "root": root,
            "kind": kind,
            "path": path,
            "api": api,
            "classification": classification,
            "canonical": canonical,
            "table_tokens": list(table_tokens),
        },
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    return "sha256:" + hashlib.sha256(payload).hexdigest()
def _make_record(
    tracked: TrackedInput,
    *,
    api: str,
    symbol: str,
    classification: str,
    canonical: str,
    table_tokens: Iterable[str] = (),
    line: int | None = None,
    source_id: str = "",
) -> SurfaceRecord:
    tokens = tuple(sorted(set(table_tokens)))
    return SurfaceRecord(
        root=tracked.root,
        kind=tracked.kind,
        path=tracked.rel_path,
        api=api,
        symbol=symbol,
        classification=classification,
        fingerprint=_fingerprint(
            root=tracked.root,
            kind=tracked.kind,
            path=tracked.rel_path,
            api=api,
            classification=classification,
            canonical=canonical,
            table_tokens=tokens,
        ),
        table_tokens=tokens,
        line=line,
        source_id=source_id,
    )
def _run_git_ls_files(repo_root: Path) -> list[str]:
    completed = subprocess.run(
        [
            "git",
            "ls-files",
            "-z",
            "--",
            "src/**.rs",
            "policies/**",
            "migrations/postgres/*.sql",
        ],
        cwd=repo_root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        stderr = completed.stderr or b""
        detail = stderr.decode("utf-8", "replace").strip()
        raise InventoryError(f"git ls-files failed: {detail or completed.returncode}")
    raw = completed.stdout or b""
    if isinstance(raw, str):
        raw = raw.encode("utf-8")
    return [os.fsdecode(part) for part in raw.split(b"\0") if part]
def _root_for_path(rel_path: str) -> tuple[str, dict[str, object]] | None:
    for root, config in ROOTS.items():
        if rel_path.startswith(str(config["prefix"])):
            return root, config
    return None
def _input_kind(root: str, rel_path: str, config: dict[str, object]) -> str:
    if root == "policies" and rel_path.startswith("policies/__tests__/"):
        return "TEST"
    return str(config["kind"])
def enumerate_tracked_inputs(repo_root: Path = REPO_ROOT) -> list[TrackedInput]:
    repo_root = Path(repo_root).resolve()
    paths = _run_git_ls_files(repo_root)
    seen: set[str] = set()
    inputs: list[TrackedInput] = []
    errors: list[str] = []
    for rel_path in paths:
        if rel_path in seen:
            errors.append(f"duplicate tracked path: {rel_path}")
            continue
        seen.add(rel_path)
        selected = _root_for_path(rel_path)
        if selected is None:
            continue
        root, config = selected
        path = repo_root / rel_path
        if path.is_symlink():
            errors.append(f"tracked symlink is not a regular inventory input: {rel_path}")
            continue
        try:
            mode = path.lstat().st_mode
        except OSError as exc:
            errors.append(f"tracked input cannot be inspected: {rel_path}: {exc}")
            continue
        if not stat.S_ISREG(mode):
            errors.append(f"tracked input is not a regular file: {rel_path}")
            continue
        extensions = tuple(config["extensions"])  # type: ignore[arg-type]
        suffix = rel_path[len(str(config["prefix"])) :]
        nested_migration = root == "migrations/postgres" and "/" in suffix
        if nested_migration or not rel_path.endswith(extensions):
            errors.append(
                f"unexpected tracked extension under {root}: {rel_path}; "
                f"expected {', '.join(extensions)}"
            )
            continue
        inputs.append(
            TrackedInput(
                root=root,
                kind=_input_kind(root, rel_path, config),
                path=path,
                rel_path=rel_path,
            )
        )
    if errors:
        raise InventoryError("; ".join(errors))
    return sorted(inputs, key=lambda item: (item.root, item.rel_path, item.kind))
def _strip_js_comments(text: str) -> str:
    result: list[str] = []
    comment: str | None = None
    quote: str | None = None
    escaped = False
    i = 0
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if comment == "line":
            if ch == "\n":
                comment = None
                result.append(ch)
            else:
                result.append(" ")
            i += 1
            continue
        if comment == "block":
            if ch == "*" and nxt == "/":
                comment = None
                result.extend("  ")
                i += 2
            else:
                result.append("\n" if ch == "\n" else " ")
                i += 1
            continue
        if quote:
            result.append(ch)
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                quote = None
            i += 1
            continue
        if ch == "/" and nxt == "/":
            comment = "line"
            result.extend("  ")
            i += 2
            continue
        if ch == "/" and nxt == "*":
            comment = "block"
            result.extend("  ")
            i += 2
            continue
        if ch in {"'", '"', "`"}:
            quote = ch
        result.append(ch)
        i += 1
    return "".join(result)
def _strip_outer_parentheses(text: str) -> str:
    value = text.strip()
    while value.startswith("(") and value.endswith(")"):
        depth = 0
        quote: str | None = None
        escaped = False
        closes_at: int | None = None
        for index, ch in enumerate(value):
            if quote:
                if escaped:
                    escaped = False
                elif ch == "\\":
                    escaped = True
                elif ch == quote:
                    quote = None
                continue
            if ch in {"'", '"', "`"}:
                quote = ch
            elif ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
                if depth == 0:
                    closes_at = index
                    break
        if closes_at != len(value) - 1:
            break
        value = value[1:-1].strip()
    return value
def _split_top_level_plus(text: str) -> list[str]:
    parts: list[str] = []
    start = 0
    depth = 0
    quote: str | None = None
    escaped = False
    for index, ch in enumerate(text):
        if quote:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                quote = None
            continue
        if ch in {"'", '"', "`"}:
            quote = ch
        elif ch in "([{<":
            depth += 1
        elif ch in ")]}>" and depth:
            depth -= 1
        elif ch == "+" and depth == 0:
            parts.append(text[start:index])
            start = index + 1
    parts.append(text[start:])
    return parts
def _literal_value(token: str, language: str) -> str | None:
    value = _strip_outer_parentheses(token)
    if language == "rust":
        value = re.sub(r"^&\s*", "", value).strip()
        raw = re.fullmatch(r"(?:br|r)(?P<hashes>#{0,32})\"(?P<body>.*?)\"(?P=hashes)", value, re.DOTALL)
        if raw:
            return raw.group("body")
        if len(value) >= 2 and value[0] == '"' and value[-1] == '"':
            return value[1:-1]
        if len(value) >= 3 and value.startswith('b"') and value.endswith('"'):
            return value[2:-1]
        return None
    if len(value) < 2 or value[0] not in {"'", '"', "`"} or value[-1] != value[0]:
        return None
    if value[0] == "`" and "${" in value:
        return None
    return value[1:-1]
def _classify_literal_expression(argument: str, language: str) -> str:
    value = _strip_outer_parentheses(argument)
    if not value:
        return "UNRESOLVED"
    if language == "javascript":
        value = _strip_js_comments(value).strip()
        if "${" in value and "`" in value:
            return "UNRESOLVED"
    if _literal_value(value, language) is not None:
        return "STATIC"
    parts = _split_top_level_plus(value)
    if len(parts) > 1 and all(_literal_value(part, language) is not None for part in parts):
        return "STATIC"
    return "UNRESOLVED"
def classify_sql_argument(argument: str, language: str | None = None) -> str:
    if language is not None:
        normalized = language.lower().replace("js", "javascript").replace("rs", "rust")
        if normalized not in {"javascript", "rust"}:
            raise ValueError(f"unsupported SQL argument language: {language}")
        return _classify_literal_expression(argument, normalized)
    js = _classify_literal_expression(argument, "javascript")
    rust = _classify_literal_expression(argument, "rust")
    if js == rust == "STATIC":
        return "STATIC"
    stripped = argument.strip()
    if "${" not in stripped and not re.search(r"\b(?:format|concat)!\s*\(", stripped):
        if _literal_value(stripped, "javascript") is not None or _literal_value(
            stripped, "rust"
        ) is not None:
            return "STATIC"
    return "UNRESOLVED"
def _literal_fragments(argument: str, language: str) -> list[str]:
    fragments: list[str] = []
    if language == "javascript":
        text = _strip_js_comments(argument)
        quote: str | None = None
        start = 0
        escaped = False
        for index, ch in enumerate(text):
            if quote is None:
                if ch in {"'", '"', "`"}:
                    quote = ch
                    start = index
            elif escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                token = text[start:index + 1]
                value = _literal_value(token, language)
                if value is not None:
                    fragments.append(value)
                quote = None
        return fragments
    index = 0
    while index < len(argument):
        match = (
            RUST_RAW_PREFIX_RE.match(argument, index)
            if argument[index] in {"r", "b"}
            else None
        )
        if match is not None:
            hashes = match.group(1)
            end_marker = '"' + hashes
            end = argument.find(end_marker, index + len(match.group(0)))
            if end >= 0:
                fragments.append(argument[index + len(match.group(0)):end])
                index = end + len(end_marker)
                continue
        if argument[index] == '"':
            end = index + 1
            escaped = False
            while end < len(argument):
                if escaped:
                    escaped = False
                elif argument[end] == "\\":
                    escaped = True
                elif argument[end] == '"':
                    fragments.append(argument[index + 1:end])
                    index = end + 1
                    break
                end += 1
            else:
                index += 1
            continue
        index += 1
    return fragments
def _table_tokens(text: str) -> tuple[str, ...]:
    return tuple(sorted({match.group(1) for match in TABLE_TOKEN_RE.finditer(text)}))
def scan_js_calls(path: Path | str, repo_root: Path = REPO_ROOT) -> list[SurfaceRecord]:
    repo_root = Path(repo_root).resolve()
    path = Path(path).resolve()
    rel_path = path.relative_to(repo_root).as_posix()
    tracked = TrackedInput("policies", _input_kind("policies", rel_path, ROOTS["policies"]), path, rel_path)
    callsites = policy_db_scanner.scan_callsites(path, repo_root)
    records: list[SurfaceRecord] = []
    occurrence: dict[tuple[str, str], int] = {}
    for callsite in callsites:
        argument = policy_db_scanner.first_call_argument(callsite.expression)
        classification = classify_sql_argument(argument, "javascript")
        fragments = _literal_fragments(argument, "javascript")
        tokens = _table_tokens(" ".join(fragments))
        api = f"agentdesk.db.{callsite.op}"
        if api not in JS_CALL_REGISTRY:
            continue
        canonical = _normalise_expression(callsite.expression)
        occurrence_key = (api, canonical)
        ordinal = occurrence.get(occurrence_key, 0)
        occurrence[occurrence_key] = ordinal + 1
        stable_canonical = f"{canonical}\0occurrence={ordinal}"
        records.append(
            _make_record(
                tracked,
                api=api,
                symbol=callsite.op,
                classification=classification,
                canonical=stable_canonical,
                table_tokens=tokens,
                line=callsite.line,
                source_id=f"{tracked.rel_path}\0{api}\0{canonical}\0{ordinal}",
            )
        )
    return records
def _mask_rust_comments_and_strings(text: str) -> str:
    chars = list(text)
    i = 0
    block_depth = 0
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if block_depth:
            if ch == "/" and nxt == "*":
                block_depth += 1
                chars[i:i + 2] = [" ", " "]
                i += 2
            elif ch == "*" and nxt == "/":
                block_depth -= 1
                chars[i:i + 2] = [" ", " "]
                i += 2
            else:
                if ch != "\n":
                    chars[i] = " "
                i += 1
            continue
        if ch == "/" and nxt == "/":
            chars[i:i + 2] = [" ", " "]
            i += 2
            while i < len(text) and text[i] != "\n":
                chars[i] = " "
                i += 1
            continue
        if ch == "/" and nxt == "*":
            block_depth = 1
            chars[i:i + 2] = [" ", " "]
            i += 2
            continue
        raw_match = RUST_RAW_PREFIX_RE.match(text, i) if ch in {"r", "b"} else None
        if raw_match is not None:
            hashes = raw_match.group(1)
            marker = '"' + hashes
            end = text.find(marker, i + len(raw_match.group(0)))
            end = len(text) if end < 0 else end + len(marker)
            for index in range(i, end):
                if chars[index] != "\n":
                    chars[index] = " "
            i = end
            continue
        if ch == "'" and not (
            (i + 2 < len(text) and text[i + 2] == "'")
            or (nxt == "\\" and i + 3 < len(text) and text[i + 3] == "'")
        ):
            i += 1
            continue
        if ch in {'"', "'"} or (ch == "b" and nxt == '"'):
            start = i
            if ch == "b":
                i += 1
            quote = text[i]
            i += 1
            escaped = False
            while i < len(text):
                current = text[i]
                if escaped:
                    escaped = False
                elif current == "\\":
                    escaped = True
                elif current == quote:
                    i += 1
                    break
                i += 1
            for index in range(start, i):
                if chars[index] != "\n":
                    chars[index] = " "
            continue
        i += 1
    return "".join(chars)
def _rust_call_open(masked: str, start: int) -> int | None:
    index = start
    angle_depth = 0
    while index < len(masked):
        ch = masked[index]
        if ch == "<":
            angle_depth += 1
        elif ch == ">" and angle_depth:
            angle_depth -= 1
        elif ch == "!" and angle_depth == 0:
            return None
        elif ch == "(" and angle_depth == 0:
            return index
        elif angle_depth == 0 and ch in ";={}\n":
            return None
        index += 1
    return None
def _balanced_rust_expression(text: str, masked: str, start: int, open_paren: int) -> str:
    depth = 0
    for index in range(open_paren, len(masked)):
        if masked[index] == "(":
            depth += 1
        elif masked[index] == ")":
            depth -= 1
            if depth == 0:
                return text[start: index + 1]
    raise InventoryError(f"unterminated Rust call near byte {open_paren}")
def _first_masked_argument(expression: str, masked_expression: str) -> str:
    open_paren = masked_expression.find("(")
    if open_paren < 0:
        return ""
    depth = 0
    for index in range(open_paren + 1, len(masked_expression)):
        ch = masked_expression[index]
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            if ch == ")" and depth == 0:
                return expression[open_paren + 1:index]
            depth = max(0, depth - 1)
        elif ch == "," and depth == 0:
            return expression[open_paren + 1:index]
    return expression[open_paren + 1:]
def _rust_definition(masked: str, start: int) -> bool:
    prefix = masked[max(0, start - 80):start]
    return bool(re.search(r"\bfn\s+$", prefix))
def scan_rust_calls(path: Path | str, repo_root: Path = REPO_ROOT) -> list[SurfaceRecord]:
    repo_root = Path(repo_root).resolve()
    path = Path(path).resolve()
    tracked = TrackedInput("src", "RUST", path, path.relative_to(repo_root).as_posix())
    text = path.read_text(encoding="utf-8")
    masked = _mask_rust_comments_and_strings(text)
    patterns: list[tuple[str, re.Pattern[str]]] = []
    for spelling in RUST_CALL_REGISTRY:
        if "::" in spelling:
            left, right = spelling.split("::", 1)
            pattern = re.compile(rf"(?<![A-Za-z0-9_$]){re.escape(left)}\s*::\s*{re.escape(right)}(?![A-Za-z0-9_$])")
        else:
            pattern = re.compile(rf"(?<![A-Za-z0-9_$]){re.escape(spelling)}(?![A-Za-z0-9_$])")
        patterns.append((spelling, pattern))
    records: list[SurfaceRecord] = []
    seen: set[tuple[int, str]] = set()
    occurrence: dict[tuple[str, str], int] = {}
    for spelling, pattern in patterns:
        for match in pattern.finditer(masked):
            if _rust_definition(masked, match.start()):
                continue
            open_paren = _rust_call_open(masked, match.end())
            if open_paren is None or (open_paren, spelling) in seen:
                continue
            seen.add((open_paren, spelling))
            expression = _balanced_rust_expression(text, masked, match.start(), open_paren)
            masked_expression = masked[match.start(): match.start() + len(expression)]
            argument = _first_masked_argument(expression, masked_expression)
            classification = classify_sql_argument(argument, "rust")
            fragments = _literal_fragments(argument, "rust")
            tokens = _table_tokens(" ".join(fragments))
            canonical = _normalise_expression(expression)
            occurrence_key = (spelling, canonical)
            ordinal = occurrence.get(occurrence_key, 0)
            occurrence[occurrence_key] = ordinal + 1
            stable_canonical = f"{canonical}\0occurrence={ordinal}"
            symbol = spelling.rsplit("::", 1)[-1]
            records.append(
                _make_record(
                    tracked,
                    api=spelling,
                    symbol=symbol,
                    classification=classification,
                    canonical=stable_canonical,
                    table_tokens=tokens,
                    line=text.count("\n", 0, match.start()) + 1,
                    source_id=f"{tracked.rel_path}\0{spelling}\0{canonical}\0{open_paren}",
                )
            )
    return records
def scan_migrations(
    tracked: TrackedInput | Path | str, repo_root: Path = REPO_ROOT
) -> list[SurfaceRecord]:
    if not isinstance(tracked, TrackedInput):
        path = Path(tracked).resolve()
        root = Path(repo_root).resolve()
        tracked = TrackedInput(
            "migrations/postgres", "MIGRATION", path, path.relative_to(root).as_posix()
        )
    content = tracked.path.read_bytes()
    content_hash = hashlib.sha256(content).hexdigest()
    canonical = f"{tracked.rel_path}\0{content_hash}"
    return [
        _make_record(
            tracked,
            api="migration.file",
            symbol="migration.file",
            classification="STATIC_FILE",
            canonical=canonical,
            source_id=tracked.rel_path,
        )
    ]
def _record_sort_key(record: SurfaceRecord) -> tuple[object, ...]:
    return (
        record.root,
        record.kind,
        record.path,
        record.api,
        record.symbol,
        record.classification,
        record.fingerprint,
        record.table_tokens,
    )
def validate_records(records: Sequence[SurfaceRecord]) -> list[SurfaceRecord]:
    ordered = sorted(records, key=_record_sort_key)
    seen: set[tuple[object, ...]] = set()
    duplicates: list[str] = []
    for record in ordered:
        key = record.stable_key
        if key in seen:
            duplicates.append(f"{record.path}:{record.api}:{record.fingerprint}")
        seen.add(key)
    if duplicates:
        raise InventoryError("duplicate surface records: " + ", ".join(duplicates))
    return ordered
def scan_inventory(repo_root: Path = REPO_ROOT) -> InventoryResult:
    inputs = enumerate_tracked_inputs(repo_root)
    records: list[SurfaceRecord] = []
    for tracked in inputs:
        if tracked.root == "policies" and tracked.path.suffix == ".js":
            records.extend(scan_js_calls(tracked.path, repo_root))
        elif tracked.root == "src":
            records.extend(scan_rust_calls(tracked.path, repo_root))
        elif tracked.root == "migrations/postgres":
            records.extend(scan_migrations(tracked))
    return InventoryResult(records=tuple(validate_records(records)))
def _render_human(result: InventoryResult) -> str:
    lines = [
        "SQL execution surface inventory: tracked inputs에서 관측된 execution surface fingerprint",
        f"RECORDS: {len(result.records)}",
    ]
    for record in result.records:
        tables = ",".join(record.table_tokens) if record.table_tokens else "-"
        location = f" line={record.line}" if record.line is not None else ""
        lines.append(
            f"{record.classification} {record.root}/{record.kind} {record.path} "
            f"{record.api} fingerprint={record.fingerprint} tables={tables}{location}"
        )
    lines.append("UNRESOLVED:")
    if result.unresolved:
        for record in result.unresolved:
            lines.append(
                f"  - {record.path} {record.api} fingerprint={record.fingerprint} "
                f"tables={','.join(record.table_tokens) or '-'}"
            )
    else:
        lines.append("  - (none observed; absence is not completeness evidence)")
    lines.append("LIMITS:")
    lines.extend(f"  - {limit}" for limit in LIMITS)
    if result.errors:
        lines.append("ERRORS:")
        lines.extend(f"  - {error}" for error in result.errors)
    return "\n".join(lines) + "\n"
def _render_json(result: InventoryResult) -> str:
    payload = {
        "records": [record.as_dict() for record in result.records],
        "unresolved": [record.as_dict() for record in result.unresolved],
        "limits": list(LIMITS),
        "errors": list(result.errors),
    }
    return json.dumps(payload, indent=2, sort_keys=True) + "\n"
def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument(
        "--check",
        action="store_true",
        help="scan and print the observed inventory (baseline wiring is a later slice)",
    )
    parser.add_argument("--json", action="store_true", help="emit deterministic JSON")
    return parser
def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        result = scan_inventory(args.repo_root.resolve())
    except Exception as exc:  # keep failure output honest and structurally complete
        result = InventoryResult(records=(), errors=(f"{type(exc).__name__}: {exc}",))
    output = _render_json(result) if args.json else _render_human(result)
    stream = sys.stdout if not result.errors else sys.stderr
    print(output, end="", file=stream)
    return 1 if result.errors else 0
if __name__ == "__main__":
    raise SystemExit(main())
