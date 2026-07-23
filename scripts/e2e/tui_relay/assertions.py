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

_COMPLETION_CHROME_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^✅"),
    re.compile(r"응답 완료"),
)

_SUPPRESSED_LABEL_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"SUPPRESSED_INTERNAL_LABEL", re.IGNORECASE),
    re.compile(r"suppressed internal", re.IGNORECASE),
    re.compile(r"보류된 출력"),
    re.compile(r"출력 보류"),
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
    for pat in _STATUS_PATTERNS:
        if pat.search(body):
            return True
    msg_type = message.get("type")
    if msg_type not in (None, 0, 19):
        return True  # only default messages and reply bodies count; system is chrome
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
    message_updates: list[dict[str, Any]] = dataclasses.field(default_factory=list)
    first_prompt_at: _dt.datetime | None = None
    prompt_sent_at: list[_dt.datetime] = dataclasses.field(default_factory=list)

    def mark_prompt_sent(self, when: _dt.datetime | None = None) -> None:
        sent_at = when or _dt.datetime.now(_dt.timezone.utc)
        self.prompt_sent_at.append(sent_at)
        if self.first_prompt_at is None:
            self.first_prompt_at = sent_at

    def add(self, message: dict[str, Any]) -> None:
        # Track every observed message for debug/forensics, but only keep
        # relay-response bodies in the canonical messages list used by
        # assertions.
        message_id = str(message.get("id") or "")
        if message_id:
            for idx, existing in enumerate(self.raw_messages):
                if str(existing.get("id") or "") != message_id:
                    continue
                if _message_changed(existing, message):
                    self.message_updates.append(
                        {
                            "id": message_id,
                            "before": existing.get("content") or "",
                            "after": message.get("content") or "",
                            "before_edited_timestamp": existing.get("edited_timestamp"),
                            "after_edited_timestamp": message.get("edited_timestamp"),
                        }
                    )
                self.raw_messages[idx] = message
                self.messages = [m for m in self.raw_messages if is_relay_response(m)]
                return
        self.raw_messages.append(message)
        if is_relay_response(message):
            self.messages.append(message)


def _message_changed(old: dict[str, Any], new: dict[str, Any]) -> bool:
    return (old.get("content") or "") != (new.get("content") or "") or old.get(
        "edited_timestamp"
    ) != new.get("edited_timestamp")


def _message_order_key(message: dict[str, Any]) -> tuple[int, str]:
    timestamp = _parse_discord_ts(str(message.get("timestamp") or ""))
    if timestamp is not None:
        return (int(timestamp.timestamp() * 1000), str(message.get("id") or ""))
    try:
        return (int(str(message.get("id") or "0")), str(message.get("id") or ""))
    except ValueError:
        return (0, str(message.get("id") or ""))


def _raw_assertion_messages(
    window: Window, *, include_our_send: bool = False
) -> list[dict[str, Any]]:
    if include_our_send:
        return list(window.raw_messages)
    return [message for message in window.raw_messages if not is_our_send(message)]


def _chrome_messages(
    window: Window,
    *,
    text: str | None = None,
    regex: str | None = None,
    include_our_send: bool = False,
) -> list[dict[str, Any]]:
    messages = []
    for message in _raw_assertion_messages(window, include_our_send=include_our_send):
        body = message.get("content") or ""
        if text is not None:
            if text in body:
                messages.append(message)
            continue
        if regex is not None:
            if re.search(regex, body):
                messages.append(message)
            continue
        if is_status_chrome(message):
            messages.append(message)
    return messages


def message_count_between_markers(window: Window, *, low: int, high: int) -> None:
    actual = len(window.messages)
    if not (low <= actual <= high):
        raise AssertionError(
            f"relay message count {actual} outside [{low}, {high}] "
            f"(raw observed: {len(window.raw_messages)})"
        )


def raw_message_count_between_markers(
    window: Window, *, low: int, high: int, include_our_send: bool = False
) -> None:
    actual = len(_raw_assertion_messages(window, include_our_send=include_our_send))
    if not (low <= actual <= high):
        raise AssertionError(
            f"raw message count {actual} outside [{low}, {high}] "
            f"(relay observed: {len(window.messages)})"
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


def raw_text_absent(
    window: Window, *, needle: str, include_our_send: bool = False
) -> None:
    hits = [
        message
        for message in _raw_assertion_messages(window, include_our_send=include_our_send)
        if needle in (message.get("content") or "")
    ]
    if hits:
        raise AssertionError(
            f"unexpected raw text {needle!r} appeared in {len(hits)} message(s): "
            f"{[(m.get('id'), (m.get('content') or '')[:80]) for m in hits[:3]]}"
        )


def marker_absent(
    window: Window,
    *,
    marker: str,
    surface: str = "relay",
    include_our_send: bool = False,
) -> None:
    if surface == "relay":
        messages = window.messages
    elif surface == "raw":
        messages = _raw_assertion_messages(window, include_our_send=include_our_send)
    else:
        raise AssertionError(f"marker_absent surface must be relay or raw, got {surface!r}")
    hits = [message for message in messages if marker in (message.get("content") or "")]
    if hits:
        raise AssertionError(
            f"unexpected marker {marker!r} appeared on {surface} surface "
            f"in {len(hits)} message(s): "
            f"{[(m.get('id'), (m.get('content') or '')[:80]) for m in hits[:3]]}"
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
    """Assert relay latency is within ``max_seconds``.

    When the driver recorded a prompt start timestamp, single-response
    scenarios are bounded by prompt→first relay latency. Otherwise this falls
    back to the historical first→last relay span and remains a no-op with
    fewer than two timestamped relay messages.
    """

    times = [
        parsed
        for message in window.messages
        if (parsed := _parse_discord_ts(str(message.get("timestamp") or ""))) is not None
    ]
    if window.prompt_sent_at and times:
        sorted_times = sorted(times)
        spans: list[float] = []
        for prompt_at in window.prompt_sent_at:
            first_after_prompt = next(
                (relay_at for relay_at in sorted_times if relay_at >= prompt_at),
                None,
            )
            if first_after_prompt is not None:
                spans.append((first_after_prompt - prompt_at).total_seconds())
        if spans and max(spans) > max_seconds:
            span = max(spans)
            raise AssertionError(
                f"prompt→first relay latency max {span:.1f}s exceeds budget "
                f"{max_seconds:.1f}s ({len(spans)} prompt/relay pairs, "
                f"{len(times)} timestamped relay messages)"
            )
        if spans:
            return
        raise AssertionError(
            "relay latency could not be measured: no timestamped relay message "
            "was observed at or after any recorded prompt send time"
        )
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


def chrome_count(
    window: Window,
    *,
    text: str | None = None,
    regex: str | None = None,
    min_count: int = 0,
    max_count: int | None = None,
    exact: int | None = None,
    include_our_send: bool = False,
) -> None:
    if text is None and regex is None:
        raise AssertionError("chrome_count requires text or regex")
    if exact is not None:
        min_count = exact
        max_count = exact
    matches = _chrome_messages(
        window, text=text, regex=regex, include_our_send=include_our_send
    )
    count = len(matches)
    if count < min_count or (max_count is not None and count > max_count):
        label = text if text is not None else regex
        raise AssertionError(
            f"chrome count for {label!r} was {count}, expected "
            f"min={min_count} max={max_count}; hits="
            f"{[(m.get('id'), (m.get('content') or '')[:80]) for m in matches[:5]]}"
        )


def status_panel_after_body(
    window: Window,
    *,
    body_marker: str,
    panel_regex: str = r"Processing\.\.\.|진행 중|^🟢|^📦",
) -> None:
    body_messages = [
        message
        for message in _raw_assertion_messages(window)
        if body_marker in (message.get("content") or "")
    ]
    if not body_messages:
        raise AssertionError(f"body marker {body_marker!r} not found in raw window")
    newest_body = max(body_messages, key=_message_order_key)
    panel_messages = [
        message
        for message in _raw_assertion_messages(window)
        if re.search(panel_regex, message.get("content") or "")
    ]
    if not panel_messages:
        raise AssertionError(
            f"status panel matching {panel_regex!r} not found after body {body_marker!r}"
        )
    newest_panel = max(panel_messages, key=_message_order_key)
    if _message_order_key(newest_panel) <= _message_order_key(newest_body):
        raise AssertionError(
            "status panel was not anchored below the newest answer body: "
            f"panel={newest_panel.get('id')} body={newest_body.get('id')}"
        )


def single_status_panel(
    window: Window,
    *,
    panel_regex: str = r"Processing\.\.\.|진행 중|^🟢|^📦",
) -> None:
    panels = [
        message
        for message in _raw_assertion_messages(window)
        if re.search(panel_regex, message.get("content") or "")
    ]
    if len(panels) != 1:
        raise AssertionError(
            f"expected exactly one status panel matching {panel_regex!r}, got "
            f"{len(panels)}: {[(m.get('id'), (m.get('content') or '')[:80]) for m in panels]}"
        )


def completion_chrome_after_body(
    window: Window, *, body_marker: str, required: bool = False
) -> None:
    body_messages = [
        message
        for message in _raw_assertion_messages(window)
        if body_marker in (message.get("content") or "")
    ]
    if not body_messages:
        raise AssertionError(f"body marker {body_marker!r} not found in raw window")
    first_body = min(body_messages, key=_message_order_key)
    completion_messages = [
        message
        for message in _raw_assertion_messages(window)
        if any(
            pattern.search(message.get("content") or "")
            for pattern in _COMPLETION_CHROME_PATTERNS
        )
    ]
    if not completion_messages:
        if required:
            raise AssertionError(
                f"completion chrome not found after body marker {body_marker!r}"
            )
        return
    first_completion = min(completion_messages, key=_message_order_key)
    if _message_order_key(first_completion) < _message_order_key(first_body):
        raise AssertionError(
            "completion chrome appeared before body marker "
            f"{body_marker!r}: completion={first_completion.get('id')} "
            f"body={first_body.get('id')}"
        )


def body_not_overwritten(window: Window, *, marker: str) -> None:
    hits = [
        m
        for m in _raw_assertion_messages(window)
        if marker in (m.get("content") or "")
    ]
    if not hits:
        raise AssertionError(
            f"marker {marker!r} is absent from final observed raw bodies; "
            f"updates={window.message_updates[:3]}"
        )


def no_suppressed_label_chrome(window: Window) -> None:
    for message in _raw_assertion_messages(window):
        body = message.get("content") or ""
        for pattern in _SUPPRESSED_LABEL_PATTERNS:
            if pattern.search(body):
                raise AssertionError(
                    "suppressed-label chrome leaked into observed Discord body "
                    f"({pattern.pattern!r}): {body[:120]!r}"
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
