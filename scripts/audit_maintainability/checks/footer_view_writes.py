"""Check: completion-footer writes outside footer_view_reconciler.

S4-b1 makes ``src/services/discord/footer_view_reconciler/`` the only owner of
completion-footer registry state and terminal completion-footer Discord edits.
The remaining live in-progress footer/status-panel edit paths are temporary
S4-b2 exceptions and must be explicitly allowlisted.
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
    "src/services/discord/footer_view_reconciler.rs",
    "src/services/discord/footer_view_reconciler/",
)

REGISTRY_PATTERN = re.compile(
    r"\b(?:register_completion_footer_target(?:_for_owner)?"
    r"|completion_footer_(?:"
    r"supersede_registered_target_for_owner"
    r"|record_(?:committed_text_result_for_owner|edit_result(?:_for_edit)?)"
    r"|forget_registered_target(?:_if_message)?"
    r"|edit_for_registered_target(?:_at(?:_for_owner)?|_for_owner)?"
    r"|edit_still_registered"
    r"|registered_failure_count"
    r"))\s*\("
)

LIVE_FOOTER_MARKER_PATTERN = re.compile(
    r"\bbuild_(?:bridge|watcher)_single_message_panel_status_block\s*\("
    r"|\breanchor_(?:bridge|watcher)_two_message_status_panel_below_answer\s*\("
)

DISCORD_WRITE_CALL_PATTERN = re.compile(
    r"\b(?:crate::services::discord::http::|super::http::)?"
    r"(?:send_channel_message|edit_channel_message)\s*\("
    r"|\bTurnGateway::(?:send_message|edit_message)\s*\("
    r"|\b[A-Za-z_][A-Za-z0-9_]*\s*\.\s*"
    r"(?:edit_message|send_message|send_long_message\w*|replace_message\w*)\s*\("
)

LIVE_FOOTER_WRITE_PATTERN = re.compile(
    f"{LIVE_FOOTER_MARKER_PATTERN.pattern}|{DISCORD_WRITE_CALL_PATTERN.pattern}"
)

FN_PATTERN = re.compile(r"\b(?:async\s+)?fn\s+([A-Za-z0-9_]+)\s*\(")
CFG_TEST_MOD_PATTERN = re.compile(r"#\s*\[cfg\s*\(\s*test\s*\)\s*\]\s*mod\s+\w+\s*\{")


def _strip_cfg_test_modules(text: str) -> str:
    chars = list(text)
    cursor = 0
    while True:
        match = CFG_TEST_MOD_PATTERN.search(text, cursor)
        if match is None:
            break
        open_brace = text.find("{", match.start())
        if open_brace < 0:
            break
        depth = 0
        end = open_brace
        while end < len(text):
            ch = text[end]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    end += 1
                    break
            end += 1
        for idx in range(match.start(), min(end, len(chars))):
            if chars[idx] != "\n":
                chars[idx] = " "
        cursor = max(end, match.end())
    return "".join(chars)


def _enclosing_fn(text: str, offset: int) -> str:
    matches = list(FN_PATTERN.finditer(text[:offset]))
    if not matches:
        return "<module>"
    return matches[-1].group(1)


def _snippet_context(text: str, offset: int, match_text: str) -> str:
    start = max(0, offset - 180)
    end = min(len(text), offset + 180)
    return f"{_enclosing_fn(text, offset)}::{match_text}::{' '.join(text[start:end].split())}"


def _window(text: str, offset: int, before: int = 900, after: int = 500) -> str:
    return text[max(0, offset - before) : min(len(text), offset + after)]


def _call_window(text: str, offset: int) -> str:
    limit = min(len(text), offset + 360)
    ends = [
        pos
        for pos in (text.find(".await", offset, limit), text.find(";", offset, limit))
        if pos >= 0
    ]
    if ends:
        limit = min(ends) + len(".await")
    return text[offset:limit]


def _has_any(haystack: str, needles: tuple[str, ...]) -> bool:
    return any(needle in haystack for needle in needles)


def _is_discord_write_call(match_text: str) -> bool:
    return bool(DISCORD_WRITE_CALL_PATTERN.match(match_text))


def _live_footer_write_call_allowed_for_path(rel: str, text: str, offset: int) -> bool:
    call = _call_window(text, offset)
    context = _window(text, offset)

    if rel == "src/services/discord/tmux_watcher.rs":
        if _has_any(call, ("&display_text", "display_text")) and _has_any(
            context, ("build_watcher_streaming_edit_text", "streaming_footer_text_changed")
        ):
            return True
        if _has_any(call, ("&panel_text", "panel_text")) and "render_status_panel" in context:
            return True
        if (
            _has_any(call, ("&panel_seed", "panel_seed"))
            and "build_processing_status_block" in context
        ):
            return True
        if (
            _has_any(call, ("&status_block", "status_block"))
            and "plan_streaming_rollover" in context
        ):
            return True
        if "plan.display_snapshot" in call:
            return True

    if rel == "src/services/discord/turn_bridge/mod.rs":
        if _has_any(call, ("&stable_display_text", "stable_display_text")) and _has_any(
            context, ("build_turn_bridge_streaming_edit_text", "streaming_footer_text_changed")
        ):
            return True
        if _has_any(call, ("&panel_text", "panel_text")) and "render_status_panel" in context:
            return True
        if (
            _has_any(call, ("&status_block", "status_block"))
            and "plan_streaming_rollover" in context
        ):
            return True
        if "plan.display_snapshot" in call:
            return True

    if rel == "src/services/discord/tmux_watcher/two_message_panel.rs":
        return _has_any(call, ("&panel_text", "panel_text"))

    if rel == "src/services/discord/turn_bridge/two_message_panel.rs":
        return _has_any(call, ("&panel_block", "panel_block", "&panel_text", "panel_text"))

    return False


def _live_footer_match_allowed_for_path(
    rel: str, text: str, offset: int, match_text: str
) -> bool:
    if _is_discord_write_call(match_text):
        return _live_footer_write_call_allowed_for_path(rel, text, offset)
    if match_text.startswith("build_bridge_single_message_panel_status_block"):
        return rel == "src/services/discord/turn_bridge/mod.rs"
    if match_text.startswith("build_watcher_single_message_panel_status_block"):
        return rel == "src/services/discord/tmux_watcher.rs"
    if match_text.startswith("reanchor_bridge_two_message_status_panel_below_answer"):
        return rel == "src/services/discord/turn_bridge/mod.rs"
    if match_text.startswith("reanchor_watcher_two_message_status_panel_below_answer"):
        return rel == "src/services/discord/tmux_watcher.rs"
    return False


def _is_function_definition_line(text: str, offset: int) -> bool:
    line_start = text.rfind("\n", 0, offset) + 1
    prefix = text[line_start:offset]
    return "fn " in prefix


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if any(rel.startswith(parent) for parent in ALLOWED_PARENTS):
            continue
        text = _strip_cfg_test_modules(strip_rust_comments(read_text(path)))

        for match in REGISTRY_PATTERN.finditer(text):
            line = line_of(text, match.start())
            match_text = match.group(0).strip()
            key = stable_finding_key(
                "footer_view_writes",
                rel,
                _snippet_context(text, match.start(), match_text),
            )
            if is_allowlisted(allowlist, rel, line, key):
                continue
            findings.append(
                Finding(
                    rule="footer_view_writes",
                    severity="warn",
                    file=rel,
                    line=line,
                    message=f"completion-footer registry access outside reconciler: `{match_text}`",
                    extra={"allowlist_key": key},
                )
            )

        for match in LIVE_FOOTER_WRITE_PATTERN.finditer(text):
            if _is_function_definition_line(text, match.start()):
                continue
            match_text = match.group(0).strip()
            if not _live_footer_match_allowed_for_path(rel, text, match.start(), match_text):
                continue
            line = line_of(text, match.start())
            key = stable_finding_key(
                "footer_view_writes",
                rel,
                _snippet_context(text, match.start(), match_text),
            )
            if is_allowlisted(allowlist, rel, line, key):
                continue
            findings.append(
                Finding(
                    rule="footer_view_writes",
                    severity="warn",
                    file=rel,
                    line=line,
                    message=f"live footer/status-panel Discord write pending S4-b2: `{match_text}`",
                    extra={"allowlist_key": key},
                )
            )
    findings.sort(key=lambda f: (f.file, f.line or 0, f.message))
    return findings


CHECK = CheckSpec(
    key="footer_view_writes",
    title="Footer view writes",
    description=(
        "Completion-footer registry/write calls outside footer_view_reconciler, "
        "plus live footer/status-panel write paths that must be allowlisted until S4-b2."
    ),
    hard_gate=True,
    runner=_run,
)
