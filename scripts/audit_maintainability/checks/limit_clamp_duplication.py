"""Check: duplicated limit/days clamp logic in route handlers.

Routes commonly clamp ``limit`` / ``days`` query params with bespoke
``min(...)`` or ``.clamp(...)`` calls. When the same clamp helper appears in
3+ files with identical bounds, it should move to a shared helper.

The check is conservative: it surfaces files that contain *any* clamp call
on identifiers named ``limit`` or ``days``. The harness aggregates duplicates
across files.
"""

from __future__ import annotations

import re
from collections import defaultdict
from typing import Iterable

from ..common import Finding, line_of, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

PATTERN = re.compile(
    r"\b(limit|days)\b\s*\.?\s*(?:clamp|min|max)\s*\(\s*([^)]{1,80})\)",
)


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    by_signature: dict[str, list[tuple[str, int, str]]] = defaultdict(list)
    for path in production_rust_files():
        rel = rel_posix(path)
        if rel in allowlist:
            continue
        text = strip_rust_comments(read_text(path))
        for match in PATTERN.finditer(text):
            ident = match.group(1)
            arg = re.sub(r"\s+", " ", match.group(2)).strip()
            sig = f"{ident}::{arg}"
            by_signature[sig].append((rel, line_of(text, match.start()), match.group(0)))

    findings: list[Finding] = []
    for sig, hits in by_signature.items():
        if len({h[0] for h in hits}) < 3:
            continue
        for rel, line, snippet in hits:
            findings.append(
                Finding(
                    rule="limit_clamp_duplication",
                    severity="info",
                    file=rel,
                    line=line,
                    message=f"duplicated clamp `{sig}`: `{snippet.strip()}`",
                    extra={"signature": sig, "occurrences": str(len(hits))},
                )
            )
    findings.sort(key=lambda f: (-int(f.extra.get("occurrences", "0")), f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="limit_clamp_duplication",
    title="limit/days clamp duplication",
    description=(
        "Identical limit/days clamp expressions appearing in 3+ source files. "
        "Candidate for a shared helper (e.g. db::params::clamp_limit)."
    ),
    hard_gate=False,
    runner=_run,
)
