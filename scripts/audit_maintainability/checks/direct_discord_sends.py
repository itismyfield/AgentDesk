"""Check: direct Discord HTTP send/edit calls outside the outbound module.

Outbound Discord traffic must funnel through ``src/services/discord/outbound``
or ``src/services/message_outbox.rs``. Direct ``serenity::http::Http::send_message``
/ ``edit_message`` / ``ChannelId::send_message`` calls in other modules are
the regression we keep re-introducing (see #1282 background).
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
    "src/services/discord/outbound",
    "src/services/message_outbox.rs",
    "src/services/discord/http",
    "src/services/discord/http.rs",
)

PATTERN = re.compile(
    # serenity-style call: `<receiver>.send_message(http, ...)` /
    # `.send_files(...)` / `.edit_message(...)`. The await is optional but
    # we anchor on the open-paren so the call itself is unambiguous.
    r"\.\s*(?:send_message|send_files|edit_message|create_message)\s*\("
    # explicit type-qualified calls are a stronger signal — keep them too.
    r"|\b(?:ChannelId|UserId|GuildChannel|PrivateChannel|Http)::"
    r"(?:send_message|send_files|edit_message|create_message)\b",
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
                "direct_discord_sends",
                rel,
                _snippet_context(stripped, match.start(), match_text),
            )
            if is_allowlisted(allowlist, rel, line, key):
                continue
            findings.append(
                Finding(
                    rule="direct_discord_sends",
                    severity="warn",
                    file=rel,
                    line=line,
                    message=f"direct Discord send/edit call: `{match_text}`",
                    extra={"allowlist_key": key},
                )
            )
    findings.sort(key=lambda f: (f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="direct_discord_sends",
    title="Direct Discord send/edit",
    description=(
        "Direct serenity send_message/edit_message/reply calls outside the "
        "outbound queue (src/services/discord/outbound/, message_outbox.rs)."
    ),
    hard_gate=True,
    runner=_run,
)
