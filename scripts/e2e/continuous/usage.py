"""Token usage of one test cell turn, read only from the transcript bytes its run appended.

A turn is measured only when the transcripts hold both its input and the usage of its finished answer.
"""

import json
from pathlib import Path

TOKENS = ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens")
CLAUDE = ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")
CODEX = ("input_tokens", "cached_input_tokens", "output_tokens")


def mark(path):
    """The transcript size before a turn; records at or after it belong to the turn."""
    return {"path": str(path), "offset": Path(path).stat().st_size}


def _rows(path):
    position = 0
    with Path(path).open("rb") as stream:
        for line in stream:
            start, position = position, position + len(line)
            if line.endswith(b"\n") and line.strip():
                row = json.loads(line)
                if isinstance(row, dict):
                    yield start, row


def _count(value):
    if type(value) is not int or value < 0:
        raise ValueError("token counter is not a non-negative integer")
    return value


def _asks(content, marker):
    return marker in (content if isinstance(content, str) else json.dumps(content, ensure_ascii=False))


def _claude(path, offset, marker):
    """One API message spans several records with the same id; its last record is the bill."""
    messages, asked, finished = {}, False, False
    for start, row in _rows(path):
        message = row.get("message")
        if start < offset or not isinstance(message, dict):
            continue
        if row.get("type") == "user" and _asks(message.get("content"), marker):
            asked, finished = True, False
        elif row.get("type") == "assistant" and isinstance(message.get("id"), str):
            messages[message["id"]] = message.get("usage")
            finished = asked and message.get("stop_reason") == "end_turn"
    # Every counter must be present: a missing one is an unread bill, not zero.
    return [sum(_count(used[field]) for used in messages.values()) for field in CLAUDE], finished


def _codex(path, offset, marker):
    """Counters are cumulative per session, so the turn is the last total minus the one before it."""
    before, after, asked, counted, finished = None, None, False, False, False
    for start, row in _rows(path):
        payload = row.get("payload")
        payload = payload if isinstance(payload, dict) else {}
        info = payload.get("info") if payload.get("type") == "token_count" else None
        total = info.get("total_token_usage") if isinstance(info, dict) else None
        if isinstance(total, dict) and start < offset:
            before = total
        elif isinstance(total, dict):
            after, counted = total, True
        elif start >= offset and payload.get("type") == "message" and payload.get("role") == "user" \
                and _asks(payload.get("content"), marker):
            asked, counted, finished = True, False, False
        elif start >= offset and payload.get("type") == "task_complete":
            finished = asked and counted
    if after is None:
        return [0, 0, 0, 0], False
    spent = {field: _count(after[field]) - (_count(before[field]) if before is not None else 0) for field in CODEX}
    # Cached input is a part of input; reasoning is already inside output.
    return [_count(spent["input_tokens"] - spent["cached_input_tokens"]), _count(spent["output_tokens"]),
            _count(spent["cached_input_tokens"]), 0], finished


def turn(provider, marks, marker):
    """Tokens of the turn that answered `marker`; None unless its input and finished usage are both recorded."""
    reader = {"claude": _claude, "codex": _codex}.get(provider)
    try:
        parts = [reader(item["path"], item["offset"], marker) for item in marks]
    except (OSError, ValueError, TypeError, KeyError):
        return None
    if not any(finished for _, finished in parts):
        return None
    return dict(zip(TOKENS, (sum(values) for values in zip(*(totals for totals, _ in parts)))))
