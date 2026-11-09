"""Name why an expected relay marker never arrived; the classification never changes FAIL."""

from __future__ import annotations

import hashlib
import re
from typing import Any

from . import assertions

# Subtext the turn-presence supervisor posts when dequeue is held for an unbound watcher.
_WATCHER_UNBOUND_NOTICE = "(사유: watcher_unbound)"
_MARKER = re.compile(r"\[E2E:([^:\]\s]+):([^:\]\s]+):([^:\]\s]+)\]")


def marker_identity(run_id: str) -> str:
    """Keep run binding reproducible while separating similar timestamps at the front."""
    return f"{hashlib.sha256(run_id.encode('utf-8')).hexdigest()[:12]}-{run_id}"


def expand_markers(value: Any, run_id: str) -> Any:
    """Copy YAML values, namespacing marker templates while preserving other run-id uses."""
    if isinstance(value, dict):
        return {key: expand_markers(item, run_id) for key, item in value.items()}
    if isinstance(value, list):
        return [expand_markers(item, run_id) for item in value]
    if isinstance(value, str):
        return re.sub(r"(\[E2E:[^:\]\s]+:)\{run_id\}",
                      lambda match: match[1] + marker_identity(run_id), value)
    return value


def marker_mismatches(window: assertions.Window, needle: str, after_id: str,
                      provider: str | None = None) -> list[str]:
    expected = _MARKER.fullmatch(needle)
    if expected is None:
        return []
    authors = {assertions.provider_bot_id(provider)} if provider else set(assertions.PROVIDER_BOT_IDS.values())
    actual = set()
    for message in window.messages:
        if not _id_after(message.get("id"), after_id) or str((message.get("author") or {}).get("id")) not in authors:
            continue
        for match in _MARKER.finditer(assertions.relay_body(message) or ""):
            if match[1] == expected[1] and match[3] == expected[3] and match[2] != expected[2]:
                actual.add(match[0])
    return sorted(actual)


def classify(
    *,
    window: assertions.Window,
    needle: str,
    after_id: str,
    head: str | None,
    prompt_draft_present: bool | None,
    health: dict[str, Any],
    provider: str | None = None,
) -> str:
    if prompt_draft_present is True:
        return "prompt_not_submitted_input_buffer_still_contains_prompt"

    relay_has_needle = any(
        (body := assertions.relay_body(message)) is not None and needle in body
        for message in window.messages
    )
    if _provider_raw_contains(window, needle) and not relay_has_needle:
        return "relay_surface_filter_miss_raw_contains_needle"

    if head:
        for message in window.messages:
            body = assertions.relay_body(message) or ""
            head_at = body.find(head)
            if head_at != -1 and body.find(needle, head_at + len(head)) == -1:
                return "body_truncated_or_tail_missing_after_head"

    direct_notice_seen = any(
        assertions.DIRECT_INPUT_NOTICE.match(m.get("content") or "")
        for m in window.raw_messages
    )
    target_mailboxes = health.get("target_mailboxes")
    busy = False
    if isinstance(target_mailboxes, list):
        busy = any((m.get("busy_reasons") or []) for m in target_mailboxes if isinstance(m, dict))
    if direct_notice_seen and busy:
        return "direct_input_notified_provider_or_relay_still_busy"
    if marker_mismatches(window, needle, after_id, provider) and not _queue_blocked(window, after_id, target_mailboxes):
        return "marker_mismatch"
    if direct_notice_seen:
        return "direct_input_notified_no_tail_observed"
    if _queue_blocked(window, after_id, target_mailboxes):
        return "turn_not_started_queue_blocked"
    if not isinstance(target_mailboxes, list) or not target_mailboxes:
        # An unreadable or missing mailbox snapshot cannot rule out a held queue.
        return "no_relay_evidence_queue_state_unknown"
    return "no_direct_input_or_relay_evidence"


def _provider_raw_contains(window: assertions.Window, needle: str) -> bool:
    """Only provider output can prove a filter miss; our prompt and its input echo quote the needle."""
    return any(
        needle in (content := message.get("content") or "")
        and not assertions.is_our_send(message)
        and not assertions.DIRECT_INPUT_NOTICE.match(content)
        for message in window.raw_messages
    )


def _queue_blocked(
    window: assertions.Window, after_id: str, target_mailboxes: Any
) -> bool:
    for message in window.raw_messages:
        if (
            _WATCHER_UNBOUND_NOTICE in (message.get("content") or "")
            and not assertions.is_our_send(message)
            and _id_after(message.get("id"), after_id)
        ):
            return True
    if not isinstance(target_mailboxes, list):
        return False
    for mailbox in target_mailboxes:
        if not isinstance(mailbox, dict):
            continue
        if mailbox.get("relay_stall_state") == "queue_blocked":
            return True
        relay = mailbox.get("relay_health")
        idle = isinstance(relay, dict) and relay.get("active_turn") == "none"
        if idle and _positive(mailbox.get("queue_depth")):
            return True
    return False


def _positive(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _id_after(message_id: Any, after_id: str) -> bool:
    try:
        return int(str(message_id)) > int(after_id)
    except (TypeError, ValueError):
        return False
