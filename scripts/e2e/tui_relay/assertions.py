"""Assertion primitives for E2E scenarios.

The :class:`Window` is the set of *bot-emitted* Discord messages observed
between the SETUP and TEARDOWN markers for a single scenario channel. Our
own driver-sent prompts and Discord system noise are filtered out at
ingestion time so assertions operate purely on what the TUI relay produced.
"""

from __future__ import annotations

import dataclasses
import datetime as _dt
import os
import re
from typing import Any, Sequence

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


# ---------------------------------------------------------------------------
# #2838 (relay-stability P0-2): completeness / ordering / duplicate-marker /
# latency assertions.
#
# The legacy contract (`text_present` first-hit + `no_duplicate_content`
# byte-identical) is structurally blind to the exact leak classes that recur:
# missing tail/body, duplicate-with-differing-header, out-of-order delivery,
# and relay stalls. These primitives let a scenario assert the *full* expected
# relay set — ordered content, per-marker uniqueness, untruncated bodies, and a
# latency budget — instead of mere presence. See docs/plans/
# tui-relay-e2e-stabilization.md.
# ---------------------------------------------------------------------------


def ordered_text_present(window: Window, *, needles: Sequence[str]) -> None:
    """Assert every needle appears in the relay window in the given order.

    Matching advances a (message_index, char_offset) cursor, so needles may be
    split across separate relay messages or share one message, but a later
    needle must never resolve before an earlier one. Catches missing-fragment
    and out-of-order delivery that single-needle :func:`text_present` passes.
    """

    bodies = [(message.get("content") or "") for message in window.messages]
    cursor_msg = 0
    cursor_pos = 0
    for needle in needles:
        placed = False
        for idx in range(cursor_msg, len(bodies)):
            start = cursor_pos if idx == cursor_msg else 0
            hit = bodies[idx].find(needle, start)
            if hit != -1:
                cursor_msg = idx
                cursor_pos = hit + len(needle)
                placed = True
                break
        if not placed:
            raise AssertionError(
                f"ordered needle {needle!r} not found at/after relay position "
                f"(msg={cursor_msg}, pos={cursor_pos}); {len(bodies)} relay messages, "
                f"raw observed {len(window.raw_messages)}"
            )


def no_duplicate_marker(window: Window, *, marker: str) -> None:
    """Fail if a stable E2E marker appears in more than one relay message.

    Unlike :func:`no_duplicate_content` (which only catches byte-identical
    bodies after chrome stripping), this catches the duplicate-with-differing-
    header re-emit: the watcher re-relaying the same answer with a different
    status prefix after a restart or the 10s ACK-timeout fallback. The marker
    (e.g. ``[E2E:E2:TURN-2]``) is expected exactly once per turn.
    """

    hits = sum(1 for m in window.messages if marker in (m.get("content") or ""))
    if hits > 1:
        raise AssertionError(
            f"E2E marker {marker!r} appeared in {hits} relay messages "
            f"(expected exactly 1) — duplicate re-emit"
        )


def body_complete(window: Window, *, head: str, tail: str) -> None:
    """Assert the relay message containing ``head`` also contains ``tail`` after it.

    Catches a truncated-tail relay even when ``text_present(head)`` passes — the
    classic "first chunk delivered, remainder dropped" leak on long responses.
    """

    for message in window.messages:
        body = message.get("content") or ""
        head_at = body.find(head)
        if head_at != -1:
            if body.find(tail, head_at + len(head)) != -1:
                return
            raise AssertionError(
                f"relay body contains head {head!r} but not tail {tail!r} after it "
                f"(truncated body): {body[:160]!r}"
            )
    raise AssertionError(f"head {head!r} not found in any relay message")


def _parse_discord_ts(value: str) -> _dt.datetime | None:
    """Parse a Discord ISO-8601 timestamp into an aware datetime, or None."""

    text = value.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        return _dt.datetime.fromisoformat(text)
    except ValueError:
        return None


def relay_latency_within(window: Window, *, max_seconds: float) -> None:
    """Assert the first→last relay message span is within ``max_seconds``.

    Uses Discord message timestamps. Catches relay stalls / excessive delay that
    presence-only assertions ignore. A window with fewer than two timestamped
    relay messages is a no-op (nothing to bound).
    """

    times = [
        parsed
        for message in window.messages
        if (parsed := _parse_discord_ts(str(message.get("timestamp") or ""))) is not None
    ]
    if len(times) < 2:
        return
    span = (max(times) - min(times)).total_seconds()
    if span > max_seconds:
        raise AssertionError(
            f"relay span {span:.1f}s exceeds budget {max_seconds:.1f}s "
            f"({len(times)} timestamped relay messages)"
        )


def raw_text_present(window: Window, *, needle: str) -> None:
    for message in window.raw_messages:
        if needle in (message.get("content") or ""):
            return
    raise AssertionError(
        f"expected to find {needle!r} in raw window, got {len(window.raw_messages)} "
        "raw messages"
    )


def no_control_chars(window: Window) -> None:
    forbidden = {chr(c) for c in (0x07, 0x08, 0x0C, 0x1B, 0x7F, 0x85)}
    for message in window.messages:
        body = message.get("content") or ""
        leaked = forbidden.intersection(body)
        if leaked:
            raise AssertionError(f"control byte leaked into Discord message: {sorted(leaked)!r}")


# #2718 chrome chunks the Claude CLI emits when the auto-resume prompt
# (PY6) gets injected at the start of a turn. They must not appear in the
# relay window — `text_present` substring matching alone would happily pass
# even when the assistant body is e.g. \"No response requested.[E2E:E2:TURN-2]\"
# (the chrome glued in front of the real marker).
_RESUME_PROMPT_CHROME: tuple[str, ...] = (
    "No response requested.",
    "Continue from where you left off.",
)


def no_resume_prompt_chrome(window: Window) -> None:
    """Fail if any relay body contains Claude CLI auto-resume chrome.

    Pinned in #2718 after the PY6 (`Continue from where you left off.`)
    auto-prompt was still being prepended every turn — the assistant would
    answer the meta prompt with \"No response requested.\" and glue the real
    marker onto the same Discord message. Substring `text_present` still
    passed, masking the regression.
    """

    for message in window.messages:
        body = message.get("content") or ""
        for chrome in _RESUME_PROMPT_CHROME:
            if chrome in body:
                raise AssertionError(
                    "Claude auto-resume prompt chrome leaked into relay body "
                    f"({chrome!r}): {body[:120]!r}"
                )
