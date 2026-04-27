"""Check: writes to source-of-truth alias paths.

``docs/source-of-truth.md`` is the canonical pointer index. Code that
*writes* to alias paths (e.g. ``adk-config/shared/profiles/``,
``~/.claude/CLAUDE.md`` mirror, ``ARCHITECTURE.md`` outside the BEGIN/END
markers) bypasses the canonical path and creates drift.

This check looks for source code that opens write-mode file handles to
known alias paths. It is intentionally conservative — only flags
``fs::write``/``OpenOptions::write(true)`` patterns combined with literal
alias-path strings.
"""

from __future__ import annotations

import re
from typing import Iterable

from ..common import Finding, line_of, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

ALIAS_FRAGMENTS = (
    "adk-config/shared/profiles",
    ".claude/CLAUDE.md",
    "ARCHITECTURE.md",
    "docs/generated/",
)

WRITE_HINT = re.compile(
    r"fs::write\s*\(|OpenOptions::new\(\)[^;]{0,120}\.write\s*\(\s*true\s*\)"
    r"|tokio::fs::write\s*\(|File::create\s*\(",
)

ALLOWED_PARENTS = (
    "src/runtime_layout",
    "src/services/runtime_layout",
)


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if rel in allowlist:
            continue
        if any(rel.startswith(parent) for parent in ALLOWED_PARENTS):
            continue
        text = strip_rust_comments(read_text(path))
        # Cheap window: when both a write-call and an alias literal appear
        # in the same file, surface the alias literal lines.
        if not WRITE_HINT.search(text):
            continue
        for fragment in ALIAS_FRAGMENTS:
            for match in re.finditer(re.escape(fragment), text):
                findings.append(
                    Finding(
                        rule="source_of_truth_alias_writes",
                        severity="info",
                        file=rel,
                        line=line_of(text, match.start()),
                        message=f"file writes near alias path `{fragment}`",
                        extra={"alias": fragment},
                    )
                )
    findings.sort(key=lambda f: (f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="source_of_truth_alias_writes",
    title="Source-of-truth alias writes",
    description=(
        "File-write callsites that touch alias paths listed in "
        "docs/source-of-truth.md. Writes should target the canonical path."
    ),
    hard_gate=False,
    runner=_run,
)
