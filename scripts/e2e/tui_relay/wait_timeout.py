"""Name why an expected relay marker never arrived; the classification never changes FAIL."""

from __future__ import annotations

from typing import Any

from . import assertions

# Subtext the turn-presence supervisor posts when dequeue is held for an unbound watcher.
_WATCHER_UNBOUND_NOTICE = "(사유: watcher_unbound)"


def classify(
    *,
    window: assertions.Window,
    needle: str,
    after_id: str,
    head: str | None,
    prompt_draft_present: bool | None,
    health: dict[str, Any],
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
