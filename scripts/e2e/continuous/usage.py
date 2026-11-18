"""Token usage of one test cell, read only from the transcript bytes its run appended."""

import json
from pathlib import Path

TOKENS = ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens")


def mark(path):
    """The transcript size before a run; records at or after it belong to the run."""
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


def _claude(path, offset):
    """One API message spans several records with the same id; its last record is the bill."""
    messages = {}
    for start, row in _rows(path):
        message = row.get("message")
        if start >= offset and row.get("type") == "assistant" and isinstance(message, dict) \
                and isinstance(message.get("usage"), dict) and isinstance(message.get("id"), str):
            messages[message["id"]] = message["usage"]
    fields = ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")
    return [sum(_count(used.get(field, 0)) for used in messages.values()) for field in fields], len(messages)


def _codex(path, offset):
    """Counters are cumulative per session, so the run is the last total minus the one before it."""
    before, after, events = {}, None, 0
    for start, row in _rows(path):
        payload = row.get("payload")
        info = payload.get("info") if isinstance(payload, dict) and payload.get("type") == "token_count" else None
        total = info.get("total_token_usage") if isinstance(info, dict) else None
        if isinstance(total, dict):
            if start < offset:
                before = total
            else:
                after, events = total, events + 1
    if after is None:
        return [0, 0, 0, 0], 0
    spent = {field: _count(after.get(field, 0)) - _count(before.get(field, 0))
             for field in ("input_tokens", "cached_input_tokens", "output_tokens")}
    # Cached input is a part of input; reasoning is already inside output.
    return [_count(spent["input_tokens"] - spent["cached_input_tokens"]), _count(spent["output_tokens"]),
            _count(spent["cached_input_tokens"]), 0], events


def measure(provider, marks):
    """Sum the run's records over its transcripts; None when a turn left no readable usage."""
    reader = {"claude": _claude, "codex": _codex}.get(provider)
    try:
        parts = [reader(item["path"], item["offset"]) for item in marks]
    except (OSError, ValueError, TypeError, KeyError):
        return None
    if not parts or not sum(records for _, records in parts):
        return None
    return dict(zip(TOKENS, (sum(values) for values in zip(*(totals for totals, _ in parts)))))
