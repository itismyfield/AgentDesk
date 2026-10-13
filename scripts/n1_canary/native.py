"""Strict native JSONL grammar; production facts and ledgers are never the turn source."""
from datetime import datetime

from . import Unknown, loads
from .manifest import text, uint
from .policy import canonical, WHITE_SPACE

INTERRUPTS = {"[Request interrupted by user]", "[Request interrupted by user for tool use]"}
CHROME = {"No response requested.", "Continue from where you left off."}


def require(condition, reason="native_record_unsupported"):
    if not condition:
        raise Unknown(reason)


def identity(value):
    require(text(value), "native_identity_missing")
    return value


def unit(key, kind, payload=None, excluded=None):
    return dict(native_key=identity(key), kind=kind, payload=payload, excluded=excluded)


def fact(action="none", key=None, outcome=None, items=()):
    return dict(action=action, key=key, outcome=outcome, items=list(items))


def text_parts(parts, types):
    require(isinstance(parts, list))
    values = []
    for part in parts:
        require(isinstance(part, dict) and part.get("type") in types and isinstance(part.get("text"), str))
        values.append(part["text"])
    return values


def tool_payload(item, fields):
    name = item.get("name", "")
    require(isinstance(name, str))
    for field in fields:
        if field in item:
            value = item[field]
            return name + ": " + (value if isinstance(value, str) else canonical(value).decode())
    return name


def codex_output(value):
    # Only text content items have a pinned output contract in this provider format.
    if not isinstance(value, str):
        text_parts(value, {"input_text"})


def codex(row):
    kind = row.get("type")
    if kind == "turn.completed":
        require(row.get("error") is None)
        return fact("close", row.get("turn_id"), "completed")
    payload = row.get("payload")
    require(isinstance(payload, dict))
    subtype = payload.get("type")
    if kind == "session_meta":
        return fact("session", identity(payload.get("id")))
    if kind == "turn_context":
        return fact("context", identity(payload.get("turn_id")))
    if kind == "event_msg":
        if subtype in ("task_started", "task_complete", "turn_aborted"):
            key = identity(payload.get("turn_id"))
            if subtype == "task_started":
                return fact("open", key)
            return fact("close", key, "aborted" if subtype == "turn_aborted" else "completed")
        if subtype == "item_completed":
            item = payload.get("item")
            require(isinstance(item, dict) and item.get("type") == "AgentMessage")
            return fact("announce", identity(item.get("id")))
        if subtype == "token_count":
            require("info" in payload and (payload["info"] is None or isinstance(payload["info"], dict)))
            return fact()
        if subtype in ("user_message", "agent_message", "agent_reasoning"):
            require(isinstance(payload.get("message" if subtype != "agent_reasoning" else "text"), str))
            return fact()
        raise Unknown("native_event_unsupported")
    require(kind == "response_item")
    if subtype == "message":
        role = payload.get("role")
        if role == "assistant":
            body = "".join(text_parts(payload.get("content"), {"output_text", "input_text", "text"}))
            return fact("units", items=[unit(payload.get("id"), "body", body)])
        if role == "user":
            parts = payload.get("content")
            require(isinstance(parts, list))
            for part in parts:
                require(isinstance(part, dict))
                if part.get("type") == "input_image":
                    require(isinstance(part.get("image_url"), str))
                else:
                    text_parts([part], {"input_text", "text"})
            return fact("prompt")
        require(role in ("system", "developer"))
        text_parts(payload.get("content"), {"input_text", "text"})
        return fact()
    if subtype in ("function_call", "custom_tool_call"):
        field = "arguments" if subtype == "function_call" else "input"
        require(isinstance(payload.get(field), str))
        require(isinstance(payload.get("name"), str) and text(payload.get("call_id")))
        return fact("units", items=[unit(payload.get("id"), "tool", tool_payload(payload, (field,)))])
    if subtype == "tool_search_call":
        require(isinstance(payload.get("execution"), str) and "arguments" in payload)
        require(payload.get("call_id") is None or isinstance(payload["call_id"], str))
        return fact("units", items=[unit(payload.get("id"), "tool", tool_payload(payload, ("arguments",)))])
    if subtype in ("function_call_output", "custom_tool_call_output"):
        codex_output(payload.get("output"))
        return fact("units", items=[unit(payload.get("call_id"), "tool_result", excluded="codex_tool_output")])
    if subtype == "tool_search_output":
        require(isinstance(payload.get("status"), str) and isinstance(payload.get("execution"), str)
                and isinstance(payload.get("tools"), list) and all(isinstance(t, dict) for t in payload["tools"]))
        return fact("units", items=[unit(payload.get("call_id"), "tool_result", excluded="codex_tool_output")])
    if subtype == "reasoning":
        require(text(payload.get("id")) and any(k in payload for k in ("summary", "content", "encrypted_content")))
        for field, kinds in (("summary", {"summary_text"}), ("content", {"reasoning_text", "text"})):
            if field in payload:
                text_parts(payload[field], kinds)
        if "encrypted_content" in payload:
            require(payload["encrypted_content"] is None or isinstance(payload["encrypted_content"], str))
        return fact()
    if subtype == "agent_message":
        identity(payload.get("id"))
        text_parts(payload.get("content"), {"text"})
        return fact()
    raise Unknown("native_item_unsupported")


def claude(row):
    kind = row.get("type")
    if kind == "permission-mode":
        identity(row.get("permissionMode"))
        return fact()
    if kind == "system":
        subtype = row.get("subtype")
        require(subtype in ("turn_duration", "stop_hook_summary"))
        if subtype == "turn_duration":
            require(uint(row.get("durationMs")))
        else:
            require(row.get("preventedContinuation") is False and row.get("hookErrors") == [])
        return fact("close", outcome="completed")
    if kind == "result":
        require(row.get("subtype") == "success" and row.get("is_error") is False, "native_outcome_unknown")
        return fact("close", outcome="completed")
    require(kind in ("assistant", "user"))
    key, message = identity(row.get("uuid")), row.get("message")
    require(isinstance(message, dict))
    content = message.get("content")
    if kind == "user":
        marker = content if isinstance(content, str) else None
        if isinstance(content, list) and len(content) == 1 and isinstance(content[0], dict) and content[0].get("type") == "text":
            marker = content[0].get("text")
        if isinstance(marker, str) and marker.strip(WHITE_SPACE) in INTERRUPTS:
            return fact("close", outcome="aborted")
        require(type(row.get("isMeta", False)) is bool)
        if isinstance(content, str):
            return fact("none" if row.get("isMeta") else "prompt", key)
        require(isinstance(content, list))
        items, prompt = [], False
        for item in content:
            require(isinstance(item, dict))
            if item.get("type") == "text":
                text_parts([item], {"text"})
                prompt = True
                continue
            require(item.get("type") == "tool_result" and "content" in item)
            require(type(item.get("is_error", False)) is bool)
            require(isinstance(item["content"], (str, list)))
            if isinstance(item["content"], list):
                for part in item["content"]:
                    require(isinstance(part, dict))
                    if part.get("type") == "image" and not item.get("is_error"):
                        require(isinstance(part.get("source"), dict))
                    else:
                        text_parts([part], {"text"})
            if item.get("is_error"):
                body = item["content"]
                body = body if isinstance(body, str) else "\n".join(text_parts(body, {"text"}))
                items.append(unit(item.get("tool_use_id"), "tool_result", body))
            else:
                items.append(unit(item.get("tool_use_id"), "tool_result", excluded="normal_tool_result"))
        return fact("prompt" if prompt and not row.get("isMeta") else "units", key, items=items)
    require(isinstance(content, list) and len(content) == 1 and isinstance(content[0], dict))
    block = content[0]
    subtype = block.get("type")
    if subtype in ("thinking", "redacted_thinking"):
        require(isinstance(block.get("thinking" if subtype == "thinking" else "data"), str))
        return fact("assistant", key)
    require(subtype in ("text", "tool_use"))
    api_error = row.get("isApiErrorMessage", False)
    require(type(api_error) is bool)
    synthetic = api_error or message.get("model") == "<synthetic>"
    index = row.get("apiBlockIndex")
    native_key = identity(message.get("id")) + ":" + str(index) if uint(index) else key if synthetic and index is None else None
    identity(native_key)
    if subtype == "tool_use":
        identity(block.get("id"))
        require("input" in block)
        item = unit(native_key, "tool", tool_payload(block, ("input",)))
    else:
        require(isinstance(block.get("text"), str))
        excluded = "claude_synthetic_resume_chrome" if synthetic and not api_error and block["text"].strip(WHITE_SPACE) in CHROME else None
        item = unit(native_key, "body", block["text"], excluded)
    return fact("assistant", key, items=[item])


def parse(raw, scope):
    require(isinstance(raw, bytes), "native_record_invalid")
    end, baseline = scope["closure_offset"], scope["baseline_offset"]
    require(uint(end) and uint(baseline) and 0 <= baseline < end <= len(raw), "native_range_invalid")
    require(raw.endswith(b"\n") and raw[:end].endswith(b"\n"), "native_partial_record")
    provider = scope["provider"]
    require(provider in ("codex", "claude"), "native_provider_unsupported")
    turns, current, last, offset, boundaries = [], None, None, 0, {0}
    assistant_owners = {}
    try:
        for line in raw[:end].splitlines(keepends=True):
            start, offset = offset, offset + len(line)
            boundaries.add(offset)
            row = loads(line)
            require(isinstance(row, dict))
            if "timestamp" in row:
                stamp = datetime.fromisoformat(row["timestamp"].replace("Z", "+00:00"))
                require(stamp.tzinfo is not None)
            if provider == "claude":
                require(row.get("sessionId") == scope["session_id"], "native_session_mismatch")
                require(row.get("isSidechain", False) is False, "native_sidechain_unsupported")
                value = claude(row)
                require(row.get("isApiErrorMessage") is not True, "native_outcome_unknown")
            else:
                value = codex(row)
                if start == 0:
                    require(value["action"] == "session", "native_session_missing")
                if value["action"] == "session":
                    require(value["key"] == scope["session_id"], "native_session_mismatch")
            action = value["action"]
            if action == "context":
                require(current is not None and current["native_turn_id"] == value["key"], "native_context_mismatch")
            opens = action == "open" or (provider == "claude" and action in ("prompt", "assistant") and current is None)
            if opens:
                require(current is None, "native_opener_overlap")
                current = dict(native_turn_id=identity(value["key"]), start_offset=start, records=[], units=[],
                               autonomous=action == "assistant", announced=set())
            if current is None:
                require(action in ("none", "session", "close"), "native_opener_missing")
                if action == "close":
                    require(last is not None and last["outcome"] == value["outcome"], "native_anonymous_close")
                    if provider == "codex":
                        require(value["key"] == last["native_turn_id"] and normalized(row) == last["records"][-1], "native_closer_conflict")
                continue
            if provider == "claude" and (action == "assistant" or opens):
                owner = current["native_turn_id"]
                require(assistant_owners.get(value["key"], owner) == owner, "native_uuid_owner_conflict")
                if action == "assistant":
                    assistant_owners.setdefault(value["key"], owner)
            current["records"].append(normalized(row))
            if action == "announce":
                current["announced"].add(value["key"])
            for item in value["items"]:
                current["units"].append({**item, "range": [start, offset]})
                if item["kind"] == "body":
                    current["announced"].discard(item["native_key"])
            if action == "close":
                require(value["key"] is None or value["key"] == current["native_turn_id"], "native_closer_mismatch")
                require(not current.pop("announced"), "native_unit_unsealed")
                current.update(end_offset=offset, outcome=value["outcome"])
                turns.append(current)
                last, current = current, None
        require(current is None, "native_turn_incomplete")
        require(baseline in boundaries and scope["fiftieth_offset"] in boundaries, "native_range_unaligned")
        return turns
    except (ValueError, UnicodeError, TypeError, AttributeError) as error:
        if isinstance(error, Unknown):
            raise
        raise Unknown("native_record_invalid") from None


def normalized(row):
    value = {k: v for k, v in row.items() if k != "timestamp"}
    canonical(value)
    return value
