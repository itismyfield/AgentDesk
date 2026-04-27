"""Check: legacy SQLite references after the Postgres cutover.

After the Postgres migration the only legitimate SQLite mentions live in
``src/cli/migrate/`` (one-shot importers) and ``compat/``. References to
``rusqlite``/``Sqlite`` types or ``.sqlite`` filename literals elsewhere are
either stale comments or genuine legacy paths to remove.
"""

from __future__ import annotations

import re
from typing import Iterable

from ..common import Finding, line_of, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

ALLOWED_PARENTS = (
    "src/cli/migrate",
    "src/compat",
    "src/db/legacy_sqlite",
)

PATTERN = re.compile(
    r"\brusqlite\b"
    r"|\bSqliteConnection\b"
    r"|\bSqlitePool\b"
    r"|\.sqlite\b"
    r"|sqlx::Sqlite\b",
)


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if any(rel.startswith(parent) for parent in ALLOWED_PARENTS):
            continue
        if rel in allowlist:
            continue
        text = strip_rust_comments(read_text(path))
        for match in PATTERN.finditer(text):
            findings.append(
                Finding(
                    rule="legacy_sqlite_refs",
                    severity="warn",
                    file=rel,
                    line=line_of(text, match.start()),
                    message=f"legacy SQLite reference: `{match.group(0)}`",
                )
            )
    findings.sort(key=lambda f: (f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="legacy_sqlite_refs",
    title="Legacy SQLite references",
    description=(
        "rusqlite / Sqlite* / .sqlite references outside cli/migrate, compat, "
        "and db/legacy_sqlite. Surfaces fossils after the Postgres cutover."
    ),
    hard_gate=False,
    runner=_run,
)
