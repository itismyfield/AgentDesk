#!/usr/bin/env python3
"""Audit remaining legacy SQLite test surface before the sunset sweep.

This is intentionally lexical rather than Rust-AST based: the legacy surface is
mostly cfg gates, sqlite_test symbols, and Db read/separate connection calls.
The report is a Phase 0 map for #2563, not a compiler.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable, Sequence


LEGACY_FEATURE = "legacy-sqlite-tests"
LEGACY_CFG_RE = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*all\s*\(\s*test\s*,\s*feature\s*=\s*"
    r'"legacy-sqlite-tests"\s*\)\s*\)\s*\]'
)
LEGACY_NOT_CFG_RE = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*not\s*\(\s*all\s*\(\s*test\s*,\s*feature\s*=\s*"
    r'"legacy-sqlite-tests"\s*\)\s*\)\s*\)\s*\]'
)
SQLITE_SYMBOL_RE = re.compile(r"\bsqlite_test(?:::|\b)")
SQLITE_TEST_IDENTIFIER_RE = re.compile(r"\b[A-Za-z0-9_]+_sqlite_test\b")
DB_CONN_RE = re.compile(r"\.(read_conn|separate_conn)\s*\(")
OBSOLETE_SQLITE_IGNORE_RE = re.compile(
    r"#\s*\[\s*ignore\s*=\s*\"[^\"]*obsolete SQLite[^\"]*\"\s*\]"
)
RUST_TEST_ATTR_RE = re.compile(r"#\s*\[\s*(?:tokio::|async_std::)?test\b")
RUST_CFG_ATTR_RE = re.compile(r"#\s*\[\s*cfg\s*\((?P<expr>.*)\)\s*\]", re.DOTALL)

DEFAULT_INCLUDE_EXTENSIONS = {".rs", ".toml", ".md"}
SKIP_PARTS = {".git", "target", "node_modules", ".next", "dist"}
SKIP_FILES = {"docs/generated/legacy-sqlite-sunset-audit.md"}


@dataclass(frozen=True)
class FileMetrics:
    path: str
    legacy_feature_mentions: int
    legacy_cfg_gates: int
    legacy_not_cfg_gates: int
    sqlite_symbol_refs: int
    sqlite_test_identifiers: int
    db_conn_calls: int
    prod_db_conn_calls: int
    obsolete_sqlite_ignored_tests: int
    category: str

    @property
    def total_refs(self) -> int:
        return (
            self.legacy_feature_mentions
            + self.legacy_cfg_gates
            + self.legacy_not_cfg_gates
            + self.sqlite_symbol_refs
            + self.sqlite_test_identifiers
            + self.db_conn_calls
            + self.obsolete_sqlite_ignored_tests
        )


@dataclass(frozen=True)
class AuditReport:
    files: list[FileMetrics]

    @property
    def totals(self) -> dict[str, int]:
        fields = (
            "legacy_feature_mentions",
            "legacy_cfg_gates",
            "legacy_not_cfg_gates",
            "sqlite_symbol_refs",
            "sqlite_test_identifiers",
            "db_conn_calls",
            "prod_db_conn_calls",
            "obsolete_sqlite_ignored_tests",
        )
        return {field: sum(getattr(file, field) for file in self.files) for field in fields}

    @property
    def categories(self) -> dict[str, int]:
        counts: dict[str, int] = {}
        for file in self.files:
            counts[file.category] = counts.get(file.category, 0) + 1
        return dict(sorted(counts.items()))


def iter_candidate_files(root: Path) -> Iterable[Path]:
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        try:
            rel = path.relative_to(root)
        except ValueError:
            continue
        if rel.as_posix() in SKIP_FILES:
            continue
        if any(part in SKIP_PARTS for part in rel.parts):
            continue
        if path.suffix not in DEFAULT_INCLUDE_EXTENSIONS:
            continue
        yield path


def is_explicit_test_surface(rel: str) -> bool:
    return (
        rel.endswith("_tests.rs")
        or rel.endswith("/tests.rs")
        or "/tests/" in rel
        or "/routes_tests/" in rel
        or rel == "src/integration_tests.rs"
    )


def rust_brace_delta(line: str) -> int:
    return line.count("{") - line.count("}")


def strip_rust_line_comment(line: str) -> str:
    return line.split("//", 1)[0].rstrip()


def rust_code_before_comment(line: str) -> str:
    comment_starts = [
        index for index in (line.find("//"), line.find("/*")) if index != -1
    ]
    if not comment_starts:
        return line.rstrip()
    return line[: min(comment_starts)].rstrip()


def strip_trailing_rust_comments(line: str) -> str:
    stripped = line.rstrip()
    while stripped:
        without_line_comment = strip_rust_line_comment(stripped)
        if without_line_comment != stripped:
            stripped = without_line_comment
            continue
        if stripped.endswith("*/"):
            block_comment_start = stripped.rfind("/*")
            if block_comment_start != -1:
                stripped = stripped[:block_comment_start].rstrip()
                continue
        break
    return stripped


def split_cfg_args(expr: str) -> list[str]:
    args: list[str] = []
    depth = 0
    start = 0
    for index, char in enumerate(expr):
        if char == "(":
            depth += 1
        elif char == ")":
            depth = max(depth - 1, 0)
        elif char == "," and depth == 0:
            args.append(expr[start:index].strip())
            start = index + 1
    tail = expr[start:].strip()
    if tail:
        args.append(tail)
    return args


def cfg_expr_requires_test(expr: str) -> bool:
    expr = re.sub(r"\s+", "", expr)
    if expr == "test":
        return True
    if expr.startswith("not(") and expr.endswith(")"):
        return False
    if expr.startswith("all(") and expr.endswith(")"):
        return any(cfg_expr_requires_test(arg) for arg in split_cfg_args(expr[4:-1]))
    if expr.startswith("any(") and expr.endswith(")"):
        args = split_cfg_args(expr[4:-1])
        return bool(args) and all(cfg_expr_requires_test(arg) for arg in args)
    return False


def is_test_only_cfg_attr(line: str) -> bool:
    match = RUST_CFG_ATTR_RE.search(line)
    return bool(match and cfg_expr_requires_test(match.group("expr")))


def has_test_only_cfg(text: str) -> bool:
    return any(
        is_test_only_cfg_attr(attr)
        for attrs, _ in iter_rust_attr_targets(text)
        for attr in attrs
    )


def has_rust_test_attr(text: str) -> bool:
    return any(
        RUST_TEST_ATTR_RE.search(attr)
        for attrs, _ in iter_rust_attr_targets(text)
        for attr in attrs
    )


def consume_rust_attribute(
    lines: Sequence[str], start: int, initial: str
) -> tuple[str, str, int]:
    parts = [initial]
    attr_text = initial
    index = start
    while "]" not in attr_text and index + 1 < len(lines):
        index += 1
        parts.append(lines[index].strip())
        attr_text = "\n".join(parts)

    attr_end = attr_text.find("]")
    if attr_end < 0:
        return initial, "", start + 1
    remainder = attr_text[attr_end + 1 :].lstrip()
    return attr_text[: attr_end + 1], remainder, index + 1


def iter_rust_attr_targets(text: str) -> Iterable[tuple[list[str], str]]:
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if not stripped.startswith("#["):
            index += 1
            continue

        attrs: list[str] = []
        remainder = ""
        line_index = index
        attr_index = index
        while stripped.startswith("#["):
            attr, remainder, next_index = consume_rust_attribute(lines, attr_index, stripped)
            attrs.append(attr)
            if remainder:
                stripped = remainder
                index = next_index
                attr_index = line_index
                continue
            index = next_index
            if index >= len(lines):
                break
            stripped = lines[index].strip()
            if not stripped.startswith("#["):
                break
            attr_index = index
        if not remainder:
            index = max(index, attr_index + 1)
        yield attrs, remainder


def prod_db_conn_call_count(rel: str, text: str) -> int:
    if not rel.endswith(".rs") or is_explicit_test_surface(rel):
        return 0

    count = 0
    pending_test_scope = False
    test_scope_depth: int | None = None
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        line_index = index
        line = lines[index]
        if test_scope_depth is not None:
            test_scope_depth += rust_brace_delta(line)
            index += 1
            if test_scope_depth <= 0:
                test_scope_depth = None
            continue

        attrs: list[str] = []
        stripped = line.strip()
        scanned = line
        index += 1
        if stripped.startswith("#["):
            scanned = ""
            attr_index = line_index
            next_index = index
            while stripped.startswith("#["):
                attr, remainder, next_index = consume_rust_attribute(
                    lines, attr_index, stripped
                )
                attrs.append(attr)
                if remainder:
                    stripped = remainder
                    scanned = remainder
                    attr_index = line_index
                    continue
                if next_index >= len(lines):
                    stripped = ""
                    break
                next_stripped = lines[next_index].strip()
                if not next_stripped.startswith("#["):
                    stripped = ""
                    break
                attr_index = next_index
                stripped = next_stripped
            index = next_index
        else:
            stripped = scanned.strip()

        attr_is_test_only = any(
            RUST_TEST_ATTR_RE.search(attr) or is_test_only_cfg_attr(attr) for attr in attrs
        )

        if attr_is_test_only:
            pending_test_scope = True
            if not stripped:
                continue

        if pending_test_scope:
            if "{" in stripped:
                test_scope_depth = rust_brace_delta(stripped)
                if test_scope_depth <= 0:
                    test_scope_depth = None
                pending_test_scope = False
                continue
            if (
                stripped
                and not stripped.startswith("#")
                and not stripped.startswith("//")
                and rust_code_before_comment(stripped).endswith(";")
            ):
                pending_test_scope = False
                continue

        count += len(DB_CONN_RE.findall(scanned))
    return count


def classify_file(rel: str, text: str) -> str:
    if rel in {"Cargo.toml", "Cargo.lock"}:
        return "cargo_feature"
    if rel.startswith("docs/") or rel == "README.md":
        return "documentation"
    if rel in {"src/db/mod.rs", "src/db/schema.rs"}:
        return "sqlite_backend_core"
    if rel.startswith("src/compat/"):
        return "compat_cleanup"
    if rel.startswith("src/") and prod_db_conn_call_count(rel, text) > 0:
        return "prod_stub_dependency"
    if is_explicit_test_surface(rel) or has_rust_test_attr(text) or has_test_only_cfg(text):
        return "test_surface"
    if rel.startswith("src/") and SQLITE_SYMBOL_RE.search(text):
        return "prod_sqlite_symbol"
    return "other"


def collect_metrics(root: Path) -> AuditReport:
    files: list[FileMetrics] = []
    for path in sorted(iter_candidate_files(root)):
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8", errors="replace")
        metrics = FileMetrics(
            path=rel,
            legacy_feature_mentions=text.count(LEGACY_FEATURE),
            legacy_cfg_gates=len(LEGACY_CFG_RE.findall(text)),
            legacy_not_cfg_gates=len(LEGACY_NOT_CFG_RE.findall(text)),
            sqlite_symbol_refs=len(SQLITE_SYMBOL_RE.findall(text)),
            sqlite_test_identifiers=len(set(SQLITE_TEST_IDENTIFIER_RE.findall(text))),
            db_conn_calls=len(DB_CONN_RE.findall(text)),
            prod_db_conn_calls=prod_db_conn_call_count(rel, text),
            obsolete_sqlite_ignored_tests=len(OBSOLETE_SQLITE_IGNORE_RE.findall(text)),
            category=classify_file(rel, text),
        )
        if metrics.total_refs:
            files.append(metrics)
    return AuditReport(files)


def top_files(files: Sequence[FileMetrics], category: str | None, limit: int) -> list[FileMetrics]:
    selected = [file for file in files if category is None or file.category == category]
    return sorted(selected, key=lambda file: (-file.total_refs, file.path))[:limit]


def obsolete_ignore_files(files: Sequence[FileMetrics], limit: int) -> list[FileMetrics]:
    selected = [file for file in files if file.obsolete_sqlite_ignored_tests]
    return sorted(
        selected,
        key=lambda file: (-file.obsolete_sqlite_ignored_tests, -file.total_refs, file.path),
    )[:limit]


def render_markdown(report: AuditReport, *, top_limit: int) -> str:
    totals = report.totals
    lines = [
        "# Legacy SQLite Sunset Audit",
        "",
        "Generated by `scripts/audit_legacy_sqlite_sunset.py`.",
        "",
        "## Totals",
        "",
        "| Metric | Count |",
        "| --- | ---: |",
        f"| files with legacy surface | {len(report.files)} |",
    ]
    for key, value in totals.items():
        lines.append(f"| {key} | {value} |")

    lines.extend(["", "## Categories", "", "| Category | Files |", "| --- | ---: |"])
    for category, count in report.categories.items():
        lines.append(f"| {category} | {count} |")

    blockers = top_files(report.files, "prod_stub_dependency", top_limit)
    lines.extend(
        [
            "",
            "## Phase 0 Blockers",
            "",
            "These production files still call legacy `Db::read_conn()` or "
            "`Db::separate_conn()` paths. They must move to PostgreSQL-backed "
            "queries or be deleted before the final feature sweep can remove "
            "`src/db/mod.rs` stubs.",
            "",
            "| File | refs | prod db_conn_calls | all db_conn_calls | sqlite refs |",
            "| --- | ---: | ---: | ---: | ---: |",
        ]
    )
    if blockers:
        for file in blockers:
            lines.append(
                f"| `{file.path}` | {file.total_refs} | {file.prod_db_conn_calls} | "
                f"{file.db_conn_calls} | {file.sqlite_symbol_refs} |"
            )
    else:
        lines.append("| none | 0 | 0 | 0 | 0 |")

    lines.extend(
        [
            "",
            "## Remove/Migrate Decision Inventory",
            "",
            "Ignored tests already annotated as obsolete SQLite and PostgreSQL-only "
            "are remove candidates, not CI expansion candidates. Production "
            "stub dependencies remain migrate/keep decisions until PG-backed "
            "coverage replaces each callsite.",
            "",
            "| File | obsolete SQLite ignored tests | category | refs |",
            "| --- | ---: | --- | ---: |",
        ]
    )
    obsolete_files = obsolete_ignore_files(report.files, top_limit)
    if obsolete_files:
        for file in obsolete_files:
            lines.append(
                f"| `{file.path}` | {file.obsolete_sqlite_ignored_tests} | "
                f"{file.category} | {file.total_refs} |"
            )
    else:
        lines.append("| none | 0 | n/a | 0 |")

    lines.extend(
        [
            "",
            "## Largest Test/SQLite Surfaces",
            "",
            "| File | category | refs | cfg gates | sqlite refs | identifiers |",
            "| --- | --- | ---: | ---: | ---: | ---: |",
        ]
    )
    for file in top_files(report.files, None, top_limit):
        lines.append(
            f"| `{file.path}` | {file.category} | {file.total_refs} | "
            f"{file.legacy_cfg_gates + file.legacy_not_cfg_gates} | "
            f"{file.sqlite_symbol_refs} | {file.sqlite_test_identifiers} |"
        )

    lines.extend(
        [
            "",
            "## Phase Recommendation",
            "",
            "- Phase 1 should first eliminate `prod_stub_dependency` files, because "
            "those callsites keep the non-feature `LegacySqlite*` stubs alive.",
            "- After production stub dependencies reach zero, the final sweep can "
            "remove the Cargo feature, `sqlite_test` dependency, gated SQLite "
            "test modules, and `src/db/mod.rs` legacy backend types in one PR.",
        ]
    )
    return "\n".join(lines) + "\n"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--format", choices=("markdown", "json"), default="markdown")
    parser.add_argument("--top", type=int, default=25)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    report = collect_metrics(args.root.resolve())
    if args.format == "json":
        print(json.dumps({"files": [asdict(file) for file in report.files]}, indent=2))
    else:
        sys.stdout.write(render_markdown(report, top_limit=args.top))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
