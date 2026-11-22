"""Token usage of one test cell turn, read only from the transcript bytes its run appended.

A turn is measured only when the transcripts hold both its input and the usage of its finished answer.
"""

import json
from pathlib import Path

TOKENS = ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens")
CLAUDE = ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")
CODEX = ("input_tokens", "cached_input_tokens", "output_tokens")
# The budget leaves out cache reads: reusing a cached prompt costs almost nothing, yet usage still records them.
BUDGET_TOKENS = ("input_tokens", "output_tokens", "cache_write_tokens")
BUDGET_BASIS = "+".join(BUDGET_TOKENS)


def budget_tokens(spent):
    """Tokens charged against the turn and run budgets for measured usage keyed by TOKENS."""
    return sum(spent[field] for field in BUDGET_TOKENS)


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
    messages, asked, finished, calls, results = {}, False, False, set(), set()
    for start, row in _rows(path):
        message = row.get("message")
        if start < offset or not isinstance(message, dict):
            continue
        blocks = message.get("content") if isinstance(message.get("content"), list) else []
        if row.get("type") == "user" and _asks(message.get("content"), marker):
            asked, finished = True, False
        elif row.get("type") == "user" and asked:
            results |= {b.get("tool_use_id") for b in blocks if isinstance(b, dict) and b.get("type") == "tool_result"}
        elif row.get("type") == "assistant" and isinstance(message.get("id"), str):
            messages[message["id"]] = message.get("usage")
            calls |= {b.get("id") for b in blocks if isinstance(b, dict) and b.get("type") == "tool_use"}
            finished = asked and message.get("stop_reason") == "end_turn"
    # A tool result whose call is not recorded means an API message of the turn is missing.
    finished = finished and results <= calls
    # Every counter must be present: a missing one is an unread bill, not zero.
    return [sum(_count(used[field]) for used in messages.values()) for field in CLAUDE], finished


MODEL_OUTPUT = {"message", "reasoning", "function_call", "custom_tool_call", "web_search_call"}


def _codex(path, offset, marker):
    """The turn's own usage records, one per API response, must run through its final answer and add up."""
    current, turn_id, asked, records, last_output, finished = None, None, False, [], -1, False
    for index, (start, row) in enumerate(_rows(path)):
        payload = row.get("payload") if isinstance(row.get("payload"), dict) else {}
        kind = payload.get("type")
        if start < offset:
            continue
        if row.get("type") == "turn_context" or kind == "task_started":
            current = payload.get("turn_id")
        elif row.get("type") == "response_item" and not asked:
            if kind == "message" and payload.get("role") == "user" and _asks(payload.get("content"), marker):
                asked, turn_id = True, current
        elif row.get("type") == "response_item" and current == turn_id and kind in MODEL_OUTPUT \
                and payload.get("role") in (None, "assistant"):
            last_output = index
        elif row.get("type") == "token_usage_record" and asked and payload.get("turn_id") == turn_id:
            records.append((index, payload))
        elif kind == "task_complete" and asked and payload.get("turn_id") == turn_id:
            finished = True
    if not records or not isinstance(turn_id, str) or not turn_id:
        return [0, 0, 0, 0], False
    final = records[-1][1]["turn_token_usage"]
    each = [record["usage"] for _, record in records]
    # A missing response leaves either its answer after the last record or a sum short of the turn total.
    complete = finished and records[-1][0] > last_output and all(
        sum(_count(used[field]) for used in each) == _count(final[field]) for field in CODEX)
    return [_count(final["input_tokens"] - final["cached_input_tokens"]), _count(final["output_tokens"]),
            _count(final["cached_input_tokens"]), _count(final.get("cache_write_input_tokens", 0))], complete


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
