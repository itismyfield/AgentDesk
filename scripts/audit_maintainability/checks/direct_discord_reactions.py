"""Check: direct Discord reaction calls outside the lifecycle reconciler.

Turn-lifecycle reactions must flow through ``turn_view_reconciler`` and the
low-level Discord API calls must stay in ``reaction_lifecycle`` so add/remove
identity, thread-parent fallback, and synthetic-id guards cannot diverge.
"""

from __future__ import annotations

import re
from typing import Iterable

from ..common import (
    Finding,
    is_allowlisted,
    line_of,
    read_text,
    rel_posix,
    stable_finding_key,
    strip_rust_comments,
)
from . import CheckSpec

ALLOWED_PARENTS = (
    "src/services/discord/reaction_lifecycle.rs",
    "src/services/discord/turn_view_reconciler.rs",
    "src/services/discord/turn_view_reconciler/",
)

PATTERN = re.compile(
    r"\.\s*(?:create_reaction|delete_reaction)\s*\("
    r"|\b(?:add_reaction_raw|remove_reaction_raw|add_reaction|remove_reaction|delete_own_reaction)\s*\("
)

FN_PATTERN = re.compile(r"\b(?:async\s+)?fn\s+([A-Za-z0-9_]+)\s*\(")


def _enclosing_fn(text: str, offset: int) -> str:
    matches = list(FN_PATTERN.finditer(text[:offset]))
    if not matches:
        return "<module>"
    return matches[-1].group(1)


def _snippet_context(text: str, offset: int, match_text: str) -> str:
    start = max(0, offset - 160)
    end = min(len(text), offset + 160)
    return f"{_enclosing_fn(text, offset)}::{match_text}::{' '.join(text[start:end].split())}"


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if any(rel.startswith(parent) for parent in ALLOWED_PARENTS):
            continue
        text = read_text(path)
        stripped = strip_rust_comments(text)
        for match in PATTERN.finditer(stripped):
            line = line_of(stripped, match.start())
            match_text = match.group(0).strip()
            key = stable_finding_key(
                "direct_discord_reactions",
                rel,
                _snippet_context(stripped, match.start(), match_text),
            )
            if is_allowlisted(allowlist, rel, line, key):
                continue
            findings.append(
                Finding(
                    rule="direct_discord_reactions",
                    severity="warn",
                    file=rel,
                    line=line,
                    message=f"direct Discord reaction call: `{match_text}`",
                    extra={"allowlist_key": key},
                )
            )
    findings.sort(key=lambda f: (f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="direct_discord_reactions",
    title="Direct Discord reactions",
    description=(
        "Direct serenity create_reaction/delete_reaction or raw reaction "
        "wrapper calls outside reaction_lifecycle.rs and turn_view_reconciler*."
    ),
    hard_gate=True,
    runner=_run,
)
