"""Check: manual ``row.try_get`` JSON column mappings.

Reading a JSON column with ``row.try_get::<serde_json::Value, _>(...)`` (or
``row.try_get::<sqlx::types::Json<_>, _>(...)``) followed by hand-written
``serde_json::from_value`` is the legacy mapping pattern. The repo has a
typed ``db::row`` helper module — direct JSON ``try_get`` callsites should
migrate to it.
"""

from __future__ import annotations

import re
from typing import Iterable

from ..common import Finding, line_of, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

PATTERN = re.compile(
    r"row\.try_get(?:_unchecked)?::<\s*"
    r"(?:serde_json::Value|sqlx::types::Json|Json\s*<)",
)


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if rel in allowlist:
            continue
        text = strip_rust_comments(read_text(path))
        for match in PATTERN.finditer(text):
            findings.append(
                Finding(
                    rule="manual_json_row_mapping",
                    severity="info",
                    file=rel,
                    line=line_of(text, match.start()),
                    message="manual row.try_get JSON mapping (use db::row helpers)",
                )
            )
    findings.sort(key=lambda f: (f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="manual_json_row_mapping",
    title="Manual row.try_get JSON mapping",
    description=(
        "row.try_get::<serde_json::Value|Json<...>>(...) callsites that should "
        "use db::row typed helpers."
    ),
    hard_gate=False,
    runner=_run,
)
