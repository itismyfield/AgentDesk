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
# Session banners are chrome only when the whole message has the panel shape.
# The first answer message deliberately uses `<banner>\n\n<body>`, so this
# anchored pattern must not consume a banner that has a non-empty body below it.
_SESSION_BANNER_PREFIX_PATTERN = re.compile(
    r"^(?:🆕 새 세션 시작|기존 세션 복원|Lifecycle fallback)"
    r"(?: · [^\n]+)?"
    r"(?:\n\(최근 대화 \d+개를 읽어들였습니다\))?"
)
_SESSION_PANEL_CHROME_PATTERN = re.compile(
    _SESSION_BANNER_PREFIX_PATTERN.pattern + r"$"
)
_COMPLETION_PANEL_CHROME_PATTERN = re.compile(r"^-# ✅ 완료(?:\n|$)")
# The only current non-emoji-led producer is the idle-recap card. Keep this
# anchored to its complete header shape so prose such as "응답 완료를
# 설명합니다" remains a relay body. Monitor-handoff completion is covered by
# the leading `^✅` status family below.
_IDLE_RECAP_COMPLETION_PATTERN = re.compile(r"^📦 응답 완료 ·")

# A single-message completion/status footer is appended at an exact ``\n\n``
# boundary (``single_message_panel::compose_completion_footer_text``).  Keep
# this list tied to producer shapes rather than treating every ``-#`` line as
# chrome: ordinary markdown in an answer is still relay body.  The footer's
# first line may be a terminal status or a spinner-merged activity label; the
# remaining shapes are the completion block's lines after
# ``completion_footer_subtext`` adds Discord's ``-#`` prefix.
_COMPLETION_FOOTER_HEAD_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^-# ✅ (?:완료|백그라운드 완료)$"),
    # ``strip_panel_header_status_marker`` removes the leading status emoji
    # before the spinner is prepended, so the live merged form is
    # ``-# ⠸ 완료`` / ``-# ⠸ 진행 중`` rather than ``⠸ ✅ 완료``.
    re.compile(
        r"^-# (?:⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏) "
        r"(?:진행 중(?: — [^\n]+)?|완료|백그라운드 완료|"
        r"monitor 대기|scheduled wakeup(?: \([^\n]*\))?|"
        r"응답 지연(?: · [^\n]+)?|"
        r"마지막 도구 \([^\n]*\)|도구 실행 중 \([^\n]*\)|"
        r"subagent 실행 중 \([^\n]*\)|workflow 실행 중 \([^\n]*\))$"
    ),
    # Non-merged status panels remain valid footer heads for providers that
    # emit the activity label without the single-message spinner merge.
    re.compile(r"^-# (?:💤 monitor 대기|⏰ scheduled wakeup(?: \([^\n]*\))?)$"),
    re.compile(
        r"^-# (?:🔧 마지막 도구|🧵 subagent 실행 중|🧬 workflow 실행 중) "
        r"\([^\n]*\)$"
    ),
    re.compile(r"^-# 🟡 응답 지연(?: · [^\n]+)?$"),
    re.compile(
        r"^(?:⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏) 계속 처리 중$"
    ),
    re.compile(r"^-# Task     "),
    re.compile(r"^-# 턴 트리거: https://discord\.com/channels/"),
    re.compile(r"^-# (?:📦|⚠️|⚠ ) .+ · auto-compact(?: |$)"),
    re.compile(r"^-# (?:Tasks|Subagents|Background agents)(?: · .+)?$"),
    re.compile(r"^-# (?:⏱ |⏳ |🖥️ )"),
    re.compile(r"^-# ⚠️ \*\*턴을 완료하기 전에 커밋되지 않은 변경사항을 확인하세요\.\*\*$"),
)

# Lines that can continue a recognized footer head.  They are deliberately
# narrower than a generic ``-#`` test: a provider may quote or author arbitrary
# subtext in the answer, and that must not turn an interior paragraph into a
# footer boundary.  These are the metadata/section/slot shapes emitted by the
# completion-footer producers (plus the anonymized fixture form used by E-1).
_COMPLETION_FOOTER_CONTINUATION_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^-# (?:턴 시작|마지막 업데이트)\s*:\s*[^\n]+$"),
    re.compile(r"^-# 턴 트리거: https://discord\.com/channels/"),
    re.compile(r"^-# └ .+$"),
    re.compile(r"^-# (?:📦|⚠️|⚠ ) .+$"),
    re.compile(r"^-# (?:⏱ |⏳ |🖥️ )"),
    re.compile(r"^-# Task     .+$"),
)

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
    _IDLE_RECAP_COMPLETION_PATTERN,
    _SESSION_PANEL_CHROME_PATTERN,
    _COMPLETION_PANEL_CHROME_PATTERN,
    re.compile(r"세션 초기화"),
    re.compile(r"\[Stopped\]"),
)

_COMPLETION_CHROME_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^✅"),
    _IDLE_RECAP_COMPLETION_PATTERN,
    _COMPLETION_PANEL_CHROME_PATTERN,
)

_SUPPRESSED_LABEL_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"SUPPRESSED_INTERNAL_LABEL", re.IGNORECASE),
    re.compile(r"suppressed internal", re.IGNORECASE),
    re.compile(r"보류된 출력"),
    re.compile(r"출력 보류"),
)


class AssertionError(Exception):
    pass


class CompletionOrderError(AssertionError):
    """Surviving completion chrome sits above the body, which a panel move can still fix."""


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


def relay_body(message: dict[str, Any]) -> str | None:
    """Return the body used by relay-marker assertions for one message.

    A first answer may carry a session banner and body in one Discord post,
    joined by the product's exact ``<banner>\n\n<body>`` contract.  Marker
    assertions must search only the body: a marker in the banner is not
    evidence that the answer was delivered.  A banner-shaped post with any
    other separator is treated as bodyless (fail closed for malformed relay
    output).  Messages without a banner retain their historical full-content
    behavior.  The product may append a completion/status footer to that same
    post; only a recognized footer at the message tail is removed.  A
    banner-shaped prefix without the required blank separator remains
    fail-closed as a malformed boundary, while provider prose itself is allowed
    to be arbitrary.
    """

    if not is_relay_response(message):
        return None
    content = message.get("content") or ""
    prefix = _SESSION_BANNER_PREFIX_PATTERN.match(content)
    if prefix is None:
        return _strip_relay_body_chrome(content)
    remainder = content[prefix.end() :]
    if not remainder.startswith("\n\n"):
        return None
    return _strip_relay_body_chrome(remainder[2:])


def _is_completion_footer_head(line: str) -> bool:
    line = line.strip()
    return any(pattern.search(line) for pattern in _COMPLETION_FOOTER_HEAD_PATTERNS)


def _is_completion_footer_line(line: str) -> bool:
    line = line.strip()
    if not line:
        return True
    if _is_completion_footer_head(line):
        return True
    return any(
        pattern.search(line) for pattern in _COMPLETION_FOOTER_CONTINUATION_PATTERNS
    )


def _strip_completion_chrome_tail(body: str) -> str:
    """Drop only the maximal producer-shaped completion/status run at the tail.

    A footer-looking block quoted in the middle of an answer is followed by
    non-footer prose and therefore fails the all-lines check.  When several
    candidate boundaries are valid (for example, a status head followed by
    blank-separated Tasks/Subagents sections), choose the earliest valid one
    so the whole trailing chrome run is removed.

    Stripping takes evidence away from the body-scoped detectors, so it fails
    closed: a candidate suffix carrying #2718 resume-prompt chrome is never a
    strip candidate.  Some footer shapes (`-# └ {label} {summary}`, the icon-led
    metadata lines) render provider-supplied free text, so without this guard a
    forbidden string placed on such a line would be cut away before
    `no_resume_prompt_chrome` ever sees it.  Keeping it means a tool label that
    happens to quote the phrase turns the smoke red — the safe direction for a
    leak detector.
    """

    valid_boundaries: list[int] = []
    for boundary in re.finditer(r"\n\n", body):
        footer = body[boundary.end() :]
        if any(chrome in footer for chrome in _RESUME_PROMPT_CHROME):
            continue
        first = next((line for line in footer.splitlines() if line.strip()), "")
        if _is_completion_footer_head(first) and all(
            _is_completion_footer_line(line) for line in footer.splitlines()
        ):
            valid_boundaries.append(boundary.start())
    if valid_boundaries:
        return body[: min(valid_boundaries)].rstrip()
    return body


def _strip_relay_body_chrome(body: str) -> str:
    body = _strip_completion_chrome_tail(body)
    # Session-banner claims are one-shot in the product, but a repeated banner
    # is a useful fail-closed regression fixture.  Remove only another exact
    # banner + blank separator at the *start* of the already-isolated body; a
    # normal body mentioning a banner later is untouched.
    while True:
        prefix = _SESSION_BANNER_PREFIX_PATTERN.match(body)
        if prefix is None:
            return body
        remainder = body[prefix.end() :]
        if not remainder.startswith("\n\n"):
            return body
        body = _strip_completion_chrome_tail(remainder[2:])


@dataclasses.dataclass
class Window:
    setup_marker_id: str
    teardown_marker_id: str | None = None
    messages: list[dict[str, Any]] = dataclasses.field(default_factory=list)
    raw_messages: list[dict[str, Any]] = dataclasses.field(default_factory=list)
    message_updates: list[dict[str, Any]] = dataclasses.field(default_factory=list)
    # Observed ids missing from the latest channel snapshot; raw_messages keeps them.
    deleted_ids: set[str] = dataclasses.field(default_factory=set)
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

    def reconcile_snapshot(self, rows: Sequence[dict[str, Any]], *, after_id: str) -> None:
        # Discord pages the oldest messages after the cursor, so only ids below the newest
        # returned id lie inside the fetch; a truncated page never reads as a deletion.
        present = {_numeric_id(row) for row in rows} - {None}
        if not present or (cursor := _numeric_id({"id": after_id})) is None:
            return
        newest = max(present)
        self.deleted_ids = {
            str(message_id)
            for message in self.raw_messages
            if (message_id := _numeric_id(message)) is not None
            and cursor < message_id < newest
            and message_id not in present
        }


def _numeric_id(message: dict[str, Any]) -> int | None:
    value = str(message.get("id") or "")
    return int(value) if value.isdigit() else None


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
    """Fail if the same ADK relay body is left twice in the channel's final state.

    Messages deleted before the latest snapshot (per-turn '...' placeholders) are
    transient and not counted; edits already replace the observed content.
    """

    seen: dict[str, str] = {}
    for message in window.messages:
        message_id = str(message.get("id") or "")
        if message_id in window.deleted_ids:
            continue
        body = (relay_body(message) or "").strip()
        if not body:
            continue
        if body in seen:
            raise AssertionError(
                f"duplicate Discord relay body: {body[:80]!r} "
                f"(ids {seen[body]}, {message_id})"
            )
        seen[body] = message_id


def text_present(window: Window, *, needle: str) -> None:
    for message in window.messages:
        body = relay_body(message)
        if body is not None and needle in body:
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
    if surface == "relay":
        hits = [
            message
            for message in messages
            if (body := relay_body(message)) is not None and marker in body
        ]
    else:
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

    bodies = [relay_body(message) or "" for message in window.messages]
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


def relay_bodies_limited_to(
    window: Window, *, after_id: int, containing: Sequence[str], exact: Sequence[str]
) -> None:
    """Every relay body after ``after_id`` must contain an allowed needle or equal an allowed body.
    Rejects an extra distinct reply, such as a provider answer to a local control."""

    for message in window.messages:
        if int(str(message.get("id") or "0")) <= after_id:
            continue
        body = relay_body(message)
        if body is not None and (
            body.strip() in exact or any(needle in body for needle in containing)
        ):
            continue
        raise AssertionError(
            f"unexpected relay body after {after_id}: id={message.get('id')} "
            f"body={(body if body is not None else message.get('content') or '')[:120]!r}"
        )


def relay_marker_hits(window: Window, *, marker: str) -> int:
    """Count relay messages whose body (banner and chrome stripped) carries ``marker``."""

    return sum(
        1
        for message in window.messages
        if (body := relay_body(message)) is not None and marker in body
    )


def no_duplicate_marker(window: Window, *, marker: str) -> None:
    """Fail if a stable E2E marker appears in more than one relay message.

    Unlike :func:`no_duplicate_content` (which only catches byte-identical
    bodies after chrome stripping), this catches the duplicate-with-differing-
    header re-emit: the watcher re-relaying the same answer with a different
    status prefix after a restart or the 10s ACK-timeout fallback. The marker
    (e.g. ``[E2E:E2:TURN-2]``) is expected exactly once per turn.
    """

    hits = relay_marker_hits(window, marker=marker)
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
        body = relay_body(message) or ""
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
    panel_regex: str = r"Processing\.\.\.|진행 중|응답 완료|^🟢|^✅|^🔴|^📦",
) -> None:
    body_messages = [
        message
        for message in _raw_assertion_messages(window)
        if (body := relay_body(message)) is not None and body_marker in body
    ]
    if not body_messages:
        raise AssertionError(f"body marker {body_marker!r} not found in raw window")
    newest_body = max(body_messages, key=_message_order_key)
    panel_messages = find_status_panels(window, panel_regex=panel_regex)
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


def find_status_panels(
    window: Window,
    *,
    panel_regex: str = r"Processing\.\.\.|진행 중|응답 완료|^🟢|^✅|^🔴|^📦",
) -> list[dict[str, Any]]:
    return [
        message
        for message in _raw_assertion_messages(window)
        if re.search(panel_regex, message.get("content") or "")
    ]


def latest_status_panel(
    window: Window,
    *,
    panel_regex: str = r"Processing\.\.\.|진행 중|응답 완료|^🟢|^✅|^🔴|^📦",
) -> dict[str, Any]:
    panels = find_status_panels(window, panel_regex=panel_regex)
    if not panels:
        raise AssertionError(f"status panel matching {panel_regex!r} not found")
    return max(panels, key=_message_order_key)


def single_status_panel(
    window: Window,
    *,
    panel_regex: str = r"Processing\.\.\.|진행 중|응답 완료|^🟢|^✅|^🔴|^📦",
) -> None:
    panels = find_status_panels(window, panel_regex=panel_regex)
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
        if (body := relay_body(message)) is not None and body_marker in body
    ]
    if not body_messages:
        raise AssertionError(f"body marker {body_marker!r} not found in raw window")
    first_body = min(body_messages, key=_message_order_key)
    # Ordering is judged on the channel's final state: a panel moved below the body
    # leaves its deleted predecessor in raw_messages.
    completion_messages = [
        message
        for message in _raw_assertion_messages(window)
        if str(message.get("id") or "") not in window.deleted_ids
        and any(
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
        raise CompletionOrderError(
            "completion chrome appeared before body marker "
            f"{body_marker!r}: completion={first_completion.get('id')} "
            f"body={first_body.get('id')}"
        )


def body_not_overwritten(window: Window, *, marker: str) -> None:
    hits = [
        m
        for m in _raw_assertion_messages(window)
        if (body := relay_body(m)) is not None and marker in body
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
        # This is a wire-surface invariant, not a body-marker invariant.  A
        # control byte in a session banner is still leaked Discord content.
        content = message.get("content") or ""
        leaked = forbidden.intersection(content)
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
    passed, masking the regression.  Unlike control-byte scanning, this stays
    body-scoped: the CLI chrome is emitted after the session-banner separator
    and is exactly the answer-surface leak the marker contract is meant to
    reject.
    """

    for message in window.messages:
        body = relay_body(message) or ""
        for chrome in _RESUME_PROMPT_CHROME:
            if chrome in body:
                raise AssertionError(
                    "Claude auto-resume prompt chrome leaked into relay body "
                    f"({chrome!r}): {body[:120]!r}"
                )


def no_placeholder_left(window: Window) -> None:
    """Reject a surviving placeholder in the final relay body snapshot."""
    ids = [str(row.get("id") or "") for row in _raw_assertion_messages(window)
           if str(row.get("id") or "") not in window.deleted_ids
           and re.fullmatch(r"[\s\u200b\ufeff\u2060]*(?:\.\.\.|…)[\s\u200b\ufeff\u2060]*",
                            relay_body(row) or "")]
    if ids:
        raise AssertionError(f"relay placeholders remain in final snapshot: {ids}")


def completion_per_turn(window: Window, *, exact: int = 1, marker: str | None = None) -> None:
    """Count cards by sequential send timestamps, falling back to E2E body boundaries.
    Queued/concurrent turns require caller-supplied request-owned windows.
    """
    from .normal_intake_evidence import completion_candidates, completion_id

    if isinstance(exact, bool) or not isinstance(exact, int) or exact < 0:
        raise AssertionError("completion_per_turn exact must be a nonnegative integer")
    if marker is not None and (not isinstance(marker, str) or not marker):
        raise AssertionError("completion_per_turn marker must be a nonempty literal string")
    rows = [row for row in _raw_assertion_messages(window)
            if str(row.get("id") or "") not in window.deleted_ids
            and (row.get("author") or {}).get("bot")
            and row.get("type") in (None, 0, 19)]
    rows.sort(key=_message_order_key)
    groups: list[list[dict[str, Any]]] = []
    if window.prompt_sent_at:
        starts = sorted(window.prompt_sent_at)
        groups = [[] for _ in starts]
        for row in rows:
            timestamp = _parse_discord_ts(str(row.get("timestamp") or ""))
            if timestamp is None:
                raise AssertionError("completion_per_turn requires Discord timestamps for prompt attribution")
            owners = [index for index, start in enumerate(starts) if timestamp >= start]
            if owners:
                groups[owners[-1]].append(row)
    else:
        # A body marker is the fallback boundary when the caller has no prompt clock.
        anchors = []
        for index, row in enumerate(rows):
            body = relay_body(row) or ""
            found = set(re.findall(r"\[E2E:[^\]\r\n]+\]", body))
            if not found and marker is not None and marker in body:
                found.add(marker)
            if len(found) > 1:
                raise AssertionError("completion_per_turn ambiguous body markers; supply prompt timestamps")
            if found:
                anchors.append(index)
        if not anchors:
            raise AssertionError("completion_per_turn has no turn markers or prompt timestamps")
        groups = [rows[0 if index == 0 else start:anchors[index + 1] if index + 1 < len(anchors) else len(rows)]
                  for index, start in enumerate(anchors)]
    if marker is not None:
        hits = sum((relay_body(row) or "").count(marker) for row in rows)
        if hits != 1:
            raise AssertionError(f"completion_per_turn body marker {marker!r} must occur once, got {hits}")
        groups = [group for group in groups if any(marker in (relay_body(row) or "") for row in group)]
    if not groups:
        raise AssertionError("completion_per_turn has no attributable turns")
    for index, group in enumerate(groups, 1):
        sub = Window(window.setup_marker_id)
        for row in group:
            sub.add(row)
        observed = {str(row["id"]): row for row in group}
        ids = completion_candidates(sub) | edited_completion_candidates(window, observed)
        inline_ids = set()
        views = sub.messages + [{**observed[str(update["id"])], "content": update.get(field) or ""}
                                for update in window.message_updates if str(update["id"]) in observed
                                for field in ("before", "after")]
        for row in views:
            content = row.get("content") or ""
            stripped = _strip_completion_chrome_tail(content)
            footer = content[len(stripped):] if stripped != content else ""
            if any(re.fullmatch(r"-# (?:✅|⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏) (?:완료|백그라운드 완료)", line.strip())
                   for line in footer.splitlines()):
                inline_ids.add(str(row["id"]))
        ids |= inline_ids
        if len(ids) != exact:
            raise AssertionError(f"completion_per_turn turn {marker or index!r}: expected {exact}, got {len(ids)} IDs {sorted(ids)}")
        if exact and inline_ids:
            bodies = [row for row in sub.messages if relay_body(row)
                      and (marker is None or marker in (relay_body(row) or ""))]
            if not bodies:
                raise AssertionError(f"completion_per_turn turn {index}: no relay body")
            first_card = min((row for row in group if str(row["id"]) in ids), key=_message_order_key)
            if _message_order_key(first_card) < _message_order_key(bodies[0]):
                raise CompletionOrderError("completion chrome appeared before body marker")
        elif exact and marker is not None:
            if exact == 1:
                completion_id(sub, {"body_marker": marker})
            else:
                completion_chrome_after_body(sub, body_marker=marker, required=True)
        elif exact:
            bodies = [relay_body(row) for row in sub.messages if relay_body(row)]
            if not bodies:
                raise AssertionError(f"completion_per_turn turn {index}: no relay body")
            completion_chrome_after_body(sub, body_marker=bodies[0], required=True)


def edited_completion_candidates(window, observed, after_id=None):
    """Chrome already observed cannot be retired by a later edit of the same ID."""
    ids = set()
    for update in window.message_updates:
        mid = str(update["id"])
        if (row := observed.get(mid)) is None or is_our_send(row):
            continue
        if after_id is not None and int(mid) <= after_id:
            continue
        if any(p.search(update.get(field) or "") for field in ("before", "after")
               for p in _COMPLETION_CHROME_PATTERNS):
            ids.add(mid)
    return ids
