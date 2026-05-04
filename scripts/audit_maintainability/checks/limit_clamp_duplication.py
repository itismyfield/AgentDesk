"""Check: duplicated limit/days clamp logic in route handlers.

Routes commonly clamp ``limit`` / ``days`` query params with bespoke
``min(...)`` or ``.clamp(...)`` calls. The shared helper
``crate::utils::api::clamp_api_limit`` exists for the standard API-limit shape
``clamp(1, 100)``; any inline call with those exact bounds outside the helper
definition is flagged regardless of how many files share the pattern (#1698).
Other clamp signatures are still reported when they recur across 3+ files,
preserving the original conservative aggregation.

The check is conservative: it surfaces files that contain *any* clamp call on
identifiers named ``limit`` or ``days``. Files that legitimately host the
shared helper definition opt out via the
``// audit-allow: limit_clamp_helper_definition`` comment marker so the helper
itself does not get flagged.
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

# Inline ``clamp(1, 100)`` is the canonical API-limit shape that the shared
# helper ``crate::utils::api::clamp_api_limit`` covers (#1698). Any signature
# matching this argument shape is flagged irrespective of file count.
API_LIMIT_SIGNATURE = "1, 100"

HELPER_OPT_OUT_MARKER = "audit-allow: limit_clamp_helper_definition"


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    by_signature: dict[str, list[tuple[str, int, str]]] = defaultdict(list)
    for path in production_rust_files():
        rel = rel_posix(path)
        if rel in allowlist:
            continue
        raw = read_text(path)
        # Honour the helper-definition opt-out before stripping comments so
        # the marker itself can live in a regular ``//`` comment.
        if HELPER_OPT_OUT_MARKER in raw:
            continue
        text = strip_rust_comments(raw)
        for match in PATTERN.finditer(text):
            ident = match.group(1)
            arg = re.sub(r"\s+", " ", match.group(2)).strip()
            sig = f"{ident}::{arg}"
            by_signature[sig].append((rel, line_of(text, match.start()), match.group(0)))

    findings: list[Finding] = []
    for sig, hits in by_signature.items():
        # Strict gate: the canonical API-limit shape ``clamp(1, 100)`` is
        # owned by ``clamp_api_limit``; any inline occurrence is flagged
        # regardless of how many files share it.
        is_api_limit = sig.endswith(f"::{API_LIMIT_SIGNATURE}")
        if not is_api_limit and len({h[0] for h in hits}) < 3:
            continue
        for rel, line, snippet in hits:
            if is_api_limit:
                message = (
                    f"inline `{snippet.strip()}` duplicates "
                    "`crate::utils::api::clamp_api_limit`; call the helper instead"
                )
            else:
                message = f"duplicated clamp `{sig}`: `{snippet.strip()}`"
            findings.append(
                Finding(
                    rule="limit_clamp_duplication",
                    severity="info",
                    file=rel,
                    line=line,
                    message=message,
                    extra={"signature": sig, "occurrences": str(len(hits))},
                )
            )
    findings.sort(key=lambda f: (-int(f.extra.get("occurrences", "0")), f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="limit_clamp_duplication",
    title="limit/days clamp duplication",
    description=(
        "Inline `clamp(1, 100)` is owned by `crate::utils::api::clamp_api_limit` "
        "and is flagged on every site outside the helper definition. Other "
        "limit/days clamp expressions are flagged when they appear in 3+ "
        "source files, signalling another shared helper candidate."
    ),
    hard_gate=False,
    runner=_run,
)
