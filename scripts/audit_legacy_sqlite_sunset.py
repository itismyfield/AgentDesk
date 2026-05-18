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


def classify_file(rel: str, text: str) -> str:
    if rel in {"Cargo.toml", "Cargo.lock"}:
        return "cargo_feature"
    if rel.startswith("docs/") or rel == "README.md":
        return "documentation"
    if rel in {"src/db/mod.rs", "src/db/schema.rs"}:
        return "sqlite_backend_core"
    if rel.startswith("src/compat/"):
        return "compat_cleanup"
    if (
        rel.endswith("_tests.rs")
        or rel.endswith("/tests.rs")
        or "/tests/" in rel
        or rel == "src/integration_tests.rs"
        or "#[test]" in text
    ):
        return "test_surface"
    if rel.startswith("src/") and DB_CONN_RE.search(text):
        return "prod_stub_dependency"
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
            category=classify_file(rel, text),
        )
        if metrics.total_refs:
            files.append(metrics)
    return AuditReport(files)


def top_files(files: Sequence[FileMetrics], category: str | None, limit: int) -> list[FileMetrics]:
    selected = [file for file in files if category is None or file.category == category]
    return sorted(selected, key=lambda file: (-file.total_refs, file.path))[:limit]


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
            "| File | refs | db_conn_calls | sqlite refs |",
            "| --- | ---: | ---: | ---: |",
        ]
    )
    if blockers:
        for file in blockers:
            lines.append(
                f"| `{file.path}` | {file.total_refs} | {file.db_conn_calls} | "
                f"{file.sqlite_symbol_refs} |"
            )
    else:
        lines.append("| none | 0 | 0 | 0 |")

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
