"""Assertion primitives for E2E scenarios.

The :class:`Window` is the set of *bot-emitted* Discord messages observed
between the SETUP and TEARDOWN markers for a single scenario channel. Our
own driver-sent prompts and Discord system noise are filtered out at
ingestion time so assertions operate purely on what the TUI relay produced.
"""

from __future__ import annotations

import dataclasses
import os
import re
from typing import Any

# Discord author ids:
#   - Our driver uses the "명령봇" account to push prompts via /api/discord/send.
#     We must exclude this id from the relay window so prompts are not counted
#     as ADK responses.
#   - Override via `AGENTDESK_E2E_OUR_BOT_ID` if the deployment uses a different
#     announce bot.
OUR_BOT_ID = os.environ.get("AGENTDESK_E2E_OUR_BOT_ID", "1479017284805722200")

# Status/header chrome the TUI relay posts around real responses. These are
# legitimate ADK output but they repeat across turns by design, so excluding
# them from `no_duplicate_content` keeps the assertion focused on actual
# response bodies. (See #2702 / #2625 for the chrome format.)
_STATUS_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"Processing\.\.\."),
    re.compile(r"^🟢"),
    re.compile(r"^✅"),
    re.compile(r"^🔴"),
    re.compile(r"^⏸"),
    re.compile(r"^📋"),
    re.compile(r"^📦"),
    re.compile(r"^🔚"),
    re.compile(r"^▶️"),
    re.compile(r"^⚠️"),
    re.compile(r"진행 중"),
    re.compile(r"응답 완료"),
    re.compile(r"세션 복원"),
    re.compile(r"세션 초기화"),
    re.compile(r"\[Stopped\]"),
)


class AssertionError(Exception):
    pass


def is_our_send(message: dict[str, Any]) -> bool:
    author = message.get("author") or {}
    return str(author.get("id") or "") == OUR_BOT_ID


def is_status_chrome(message: dict[str, Any]) -> bool:
    body = message.get("content") or ""
    if not body:
        return True  # empty / reply-only messages are noise
    msg_type = message.get("type")
    if msg_type not in (None, 0):
        return True  # only default messages count; replies/system are chrome
    for pat in _STATUS_PATTERNS:
        if pat.search(body):
            return True
    return False


def is_relay_response(message: dict[str, Any]) -> bool:
    """A message qualifies as ADK-side relay output if it is a bot post that is
    not from our driver and not pure status chrome."""

    if is_our_send(message):
        return False
    author = message.get("author") or {}
    if not author.get("bot"):
        return False
    if is_status_chrome(message):
        return False
    return True


@dataclasses.dataclass
class Window:
    setup_marker_id: str
    teardown_marker_id: str | None = None
    messages: list[dict[str, Any]] = dataclasses.field(default_factory=list)
    raw_messages: list[dict[str, Any]] = dataclasses.field(default_factory=list)

    def add(self, message: dict[str, Any]) -> None:
        # Track every observed message for debug/forensics, but only keep
        # relay-response bodies in the canonical messages list used by
        # assertions.
        if message.get("id") and any(m.get("id") == message["id"] for m in self.raw_messages):
            return
        self.raw_messages.append(message)
        if is_relay_response(message):
            self.messages.append(message)


def message_count_between_markers(window: Window, *, low: int, high: int) -> None:
    actual = len(window.messages)
    if not (low <= actual <= high):
        raise AssertionError(
            f"relay message count {actual} outside [{low}, {high}] "
            f"(raw observed: {len(window.raw_messages)})"
        )


def no_duplicate_content(window: Window) -> None:
    """Fail if the same ADK relay body is emitted twice in the window."""

    seen: set[str] = set()
    for message in window.messages:
        body = (message.get("content") or "").strip()
        if not body:
            continue
        if body in seen:
            raise AssertionError(f"duplicate Discord relay body: {body[:80]!r}")
        seen.add(body)


def text_present(window: Window, *, needle: str) -> None:
    for message in window.messages:
        if needle in (message.get("content") or ""):
            return
    raise AssertionError(
        f"expected to find {needle!r} in relay window, got {len(window.messages)} "
        f"relay messages (raw observed: {len(window.raw_messages)})"
    )


def no_control_chars(window: Window) -> None:
    forbidden = {chr(c) for c in (0x07, 0x08, 0x0C, 0x1B, 0x7F, 0x85)}
    for message in window.messages:
        body = message.get("content") or ""
        leaked = forbidden.intersection(body)
        if leaked:
            raise AssertionError(f"control byte leaked into Discord message: {sorted(leaked)!r}")
