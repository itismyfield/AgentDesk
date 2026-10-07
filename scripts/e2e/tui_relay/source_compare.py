"""Read-only native/Discord marker audit; run with ``python3 -m scripts.e2e.tui_relay.source_compare``."""

from __future__ import annotations

import argparse
from collections import Counter
import json
import os
from pathlib import Path
import re
import sys
import urllib.parse
import urllib.request

from .assertions import DIRECT_INPUT_NOTICE as DIRECT_NOTICE, is_our_send, relay_body
from .discord import DiscordClient
from .normal_intake_evidence import content, tool_result

# Discord text command (`!clear ...`): the router consumes it, so no native input or body follows.
TEXT_COMMAND = re.compile(r"^![a-z][a-z_-]*(?:\s|$)")
# turn/deliver input has no Discord mirror yet. Started turns carry `human_input` headless-trigger
# metadata (its only producer); queued ones keep only the `[User: … (ID: author)]` prefix.
WRAPPER = r'\s*(?:<pasted_content id="[^"\n]+">\n)?'
DELIVER_INPUT = re.compile(WRAPPER + r"\[Headless trigger context\]\n(?:source: [^\n]*\n)?metadata: ([^\n]+)\n")
AUTHOR_PREFIX = re.compile(WRAPPER + r"\[User: [^\n]*? \(ID: (\d+)\)\]")
DELIVER_MIRROR_GAP = "#6245 deliver input mirror not implemented"
# Claude's `last-prompt` row is a copy of an input already recorded as a user row; only observed key sets.
LAST_PROMPT_SHAPES = ({"type", "leafUuid", "sessionId"}, {"type", "lastPrompt", "leafUuid", "sessionId"})
PASSING_VERDICTS = {"ok", "command", "known_gap"}


def marker_pattern(*, marker_regex=None, run_id=None):
    if bool(marker_regex) == bool(run_id):
        raise ValueError("provide exactly one of marker_regex or run_id")
    pattern = re.compile(marker_regex or rf"\[E2E:[^:\]\s]+:{re.escape(run_id)}(?::[^\]\s]+)?\]")
    if pattern.search(""):
        raise ValueError("marker regex must not match empty text")
    return pattern


def markers(text, pattern):
    return {m.groupdict().get("marker", m.group(0)) for m in pattern.finditer(text)} - {None, ""}


def transcript_rows(path):
    with Path(path).open(encoding="utf-8") as stream:
        for number, line in enumerate(stream, 1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
                if not isinstance(row, dict):
                    raise ValueError("expected object")
                yield row
            except (ValueError, TypeError, AttributeError) as error:
                raise ValueError(f"invalid transcript {path}:{number}: {error}") from error


def native_inputs(path, pattern):
    """Only understood native shapes may count or be ignored when carrying a target marker."""
    ignored = {"summary", "file-history-snapshot", "queue-operation", "permission-mode",
               "session_meta", "turn_context"}
    events = {"user_message", "agent_message", "agent_reasoning", "task_started", "task_complete",
              "token_count", "item_completed", "turn_aborted", "context_compacted"}
    for row in transcript_rows(path):
        kind, known, text = row.get("type"), False, None
        if kind in ("user", "assistant"):
            message = row.get("message")
            blocks = message.get("content") if isinstance(message, dict) else None
            allowed = {"text", "tool_result"} if kind == "user" else {"text", "thinking", "redacted_thinking", "tool_use"}
            known = isinstance(blocks, str) or (isinstance(blocks, list) and all(
                isinstance(b, dict) and b.get("type") in allowed and
                (isinstance(b.get("text"), str) if b.get("type") == "text" else
                 "content" in b if b.get("type") == "tool_result" else
                 isinstance(b.get("input"), dict) and bool(b.get("id")) and bool(b.get("name"))
                 if b.get("type") == "tool_use" else True) for b in blocks))
            if known and kind == "user" and not (row.get("isMeta") or row.get("isCompactSummary") or tool_result(row) or notification_fields(row)):
                text = content(row)
        elif kind == "response_item":
            payload = row.get("payload")
            payload = payload if isinstance(payload, dict) else {}
            if payload.get("type") == "message":
                blocks = payload.get("content")
                known = payload.get("role") in ("user", "assistant", "developer", "system") and isinstance(blocks, list) and all(
                    isinstance(b, dict) and b.get("type") in ("input_text", "text", "output_text")
                    and isinstance(b.get("text"), str) for b in blocks)
                if known and payload.get("role") == "user":
                    text = "\n".join(b.get("text", "") for b in blocks)
            else:
                known = payload.get("type") in {"function_call", "function_call_output", "reasoning", "custom_tool_call", "custom_tool_call_output", "web_search_call"}
        elif kind == "event_msg":
            payload = row.get("payload")
            known = isinstance(payload, dict) and payload.get("type") in events
        elif kind == "system":
            known = row.get("subtype") in {"turn_duration", "stop_hook_summary", "task_started", "task_notification", "task_progress", "compact_boundary", "local_command"}
        elif kind == "last-prompt":
            # Only the lastPrompt copy may carry a marker; marked metadata stays fail-closed.
            known = set(row) in LAST_PROMPT_SHAPES and isinstance(row.get("lastPrompt", ""), str) and not any(
                markers(value if isinstance(value, str) else json.dumps(value, ensure_ascii=False), pattern)
                for key, value in row.items() if key != "lastPrompt")
        else:
            known = kind in ignored
        if not known and markers(json.dumps(row, ensure_ascii=False), pattern):
            raise ValueError(f"unsupported marker-bearing transcript record: {kind}")
        if text is not None:
            yield text


def is_deliver_input(text, deliver_author_ids=()):
    author = AUTHOR_PREFIX.match(text)
    if author and author[1] in deliver_author_ids:
        return True
    match = DELIVER_INPUT.match(text)
    try:
        metadata = json.loads(match[1]) if match else None
    except ValueError:
        return False
    return isinstance(metadata, dict) and isinstance(metadata.get("human_input"), dict)


def compare(path, messages, *, marker_regex=None, run_id=None, input_mirror_author_ids=(),
            deliver_author_ids=()):
    """Return per-marker counts; same-ID Discord edits count only their last snapshot."""
    pattern = marker_pattern(marker_regex=marker_regex, run_id=run_id)
    native, delivered = Counter(), Counter()
    for text in native_inputs(path, pattern):
        found = markers(text, pattern)
        native.update(found)
        if is_deliver_input(text, {str(a) for a in deliver_author_ids}):
            delivered.update(found)
    mirrors, bodies, notices, commands = Counter(), Counter(), Counter(), Counter()
    final = {}
    for message in messages:
        if not str(message.get("id", "")).isdigit():
            raise ValueError("Discord message lacks numeric id")
        final[str(message["id"])] = message
    for message in final.values():
        text = message.get("content") or ""
        author = message.get("author") or {}
        if author.get("bot") and DIRECT_NOTICE.match(text):
            target = notices
        elif (not author.get("bot") or is_our_send(message)
              or str(author.get("id")) in input_mirror_author_ids):
            target = mirrors
            if TEXT_COMMAND.match(text):
                commands.update(markers(text, pattern))
        else:
            text = relay_body(message) or ""
            target = bodies
        target.update(markers(text, pattern))
    result = {}
    for marker in sorted(native.keys() | mirrors.keys() | bodies.keys() | notices.keys()):
        n, m, b, d = native[marker], mirrors[marker], bodies[marker], notices[marker]
        if max(n, m, b, d) > 1 or m + d > 1:
            verdict = "duplicated"
        elif commands[marker]:
            verdict = "command" if (n, b, d) == (0, 0, 0) else "command_forwarded"
        elif delivered[marker] and m + d == 0:
            verdict = "known_gap" if b == 1 else "missing_relay"
        elif n == 0 or m + d == 0:
            verdict = "lost"
        elif b == 0:
            verdict = "missing_relay"
        else:
            verdict = "ok"
        result[marker] = dict(native_user_count=n, discord_input_mirror_count=m,
                              relay_body_count=b, direct_input_notice_count=d, verdict=verdict)
        if verdict == "known_gap":
            result[marker]["known_gap"] = DELIVER_MIRROR_GAP
    if not result:
        raise ValueError("no markers found; empty evidence cannot pass")
    return result


def get_json(base, path):
    request = urllib.request.Request(base.rstrip("/") + path, method="GET")
    with urllib.request.urlopen(request, timeout=15) as response:
        return json.load(response)


def resolve_binding(base, *, agent_id=None, channel_id=None, transcript_root=None):
    """Resolve identity from one SELECT-only server snapshot; never infer a provider ID."""
    selector = urllib.parse.quote(str(agent_id or channel_id), safe="")
    identity = get_json(base, f"/api/agents/{selector}/session-evidence")
    if ((agent_id and identity.get("agent_id") != agent_id)
            or (channel_id and str(identity.get("channel_id")) != str(channel_id))):
        raise ValueError("session evidence does not match requested agent/channel")
    key, provider, sid = (identity.get(k) for k in ("session_key", "provider", "raw_provider_session_id"))
    if not identity.get("agent_id") or not str(identity.get("channel_id", "")).isdigit() or not key or provider not in ("claude", "codex"):
        raise ValueError("session evidence lacks an unambiguous Claude/Codex binding")
    if not isinstance(sid, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", sid):
        raise ValueError("session evidence lacks a safe raw_provider_session_id")
    if transcript_root:
        root = Path(transcript_root).expanduser()
    elif provider == "claude":
        root = Path(os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude"))) / "projects"
    else:
        root = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "sessions"
    paths = list(root.glob(f"*/{sid}.jsonl" if provider == "claude" else f"**/rollout-*-{sid}.jsonl"))
    if len(paths) != 1:
        raise ValueError(f"expected one local transcript for {sid}, found {len(paths)} under {root}")
    return dict(agent_id=identity["agent_id"], channel_id=str(identity["channel_id"]),
                session_key=key, provider=provider, session_id=sid, transcript_path=str(paths[0]))


def notification_fields(row):
    if row.get("type") == "system" and row.get("subtype") == "task_notification":
        return tuple(row.get(k) for k in ("task_id", "tool_use_id", "status"))
    if row.get("type") != "user" or tool_result(row):
        return None
    match = re.fullmatch(r"\s*<task-notification>(.*?)</task-notification>\s*", content(row), re.S)
    if not match:
        return None
    values = [re.findall(r"<" + name + r">([^<>]+)</" + name + r">", match[1])
              for name in ("task-id", "tool-use-id", "status")]
    return tuple(v[0].strip() if len(v) == 1 else None for v in values)


def autonomous_background_turn(path, *, armed_marker, auto_marker):
    """Prove a linked background Bash notification separates two terminal native responses."""
    list(native_inputs(path, re.compile(re.escape(armed_marker) + "|" + re.escape(auto_marker))))
    rows = list(transcript_rows(path))
    starts = [i for i, r in enumerate(rows) if r.get("type") == "user" and not tool_result(r)
              and not r.get("isMeta") and armed_marker in content(r)]
    if len(starts) != 1:
        raise ValueError("S4 requires one initiating native input")
    rows = rows[starts[0]:]
    end = next((i for i, r in enumerate(rows[1:], 1) if r.get("type") == "user"
                and not tool_result(r) and not r.get("isMeta") and not notification_fields(r)), len(rows))
    rows = rows[:end]
    by_id = {r.get("uuid"): i for i, r in enumerate(rows) if r.get("uuid")}
    def linked(start, target):
        current = start
        while current > target:
            parent = by_id.get(rows[current].get("parentUuid"))
            if parent is None or parent >= current:
                return False
            current = parent
        return current == target
    work = [(i, r) for i, r in enumerate(rows) if r.get("type") == "assistant"]
    tools = [(i, b) for i, r in work for b in r.get("message", {}).get("content", [])
             if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("name") == "Bash"]
    if len(tools) != 1 or tools[0][1].get("input", {}).get("run_in_background") is not True:
        raise ValueError("S4 requires exactly one background Bash tool_use")
    tool_index, tool = tools[0]
    tool_id = tool.get("id")
    if not tool_id:
        raise ValueError("S4 Bash lacks tool identity")
    positions = []
    for marker in (armed_marker, auto_marker):
        hits = [(i, r) for i, r in work if marker in content(r)]
        if sum(content(r).count(marker) for _, r in work) != 1 or len(hits) != 1:
            raise ValueError("S4 requires each assistant marker exactly once")
        i, r = hits[0]
        next_work = next((j for j in range(i + 1, len(rows))
            if rows[j].get("type") in ("assistant", "user")), len(rows))
        stop = r.get("message", {}).get("stop_reason")
        duration = any(rows[j].get("type") == "system" and rows[j].get("subtype") == "turn_duration"
                       and linked(j, i) for j in range(i + 1, next_work))
        if stop != "end_turn" and not (stop is None and duration):
            raise ValueError("S4 marker must terminate its native turn")
        positions.append(i)
    armed, auto = positions
    task_ids = set()
    for r in rows[tool_index + 1:armed]:
        for block in r.get("message", {}).get("content", []) if tool_result(r) else []:
            if block.get("type") == "tool_result" and block.get("tool_use_id") == tool_id:
                result = r.get("toolUseResult") or {}
                task = result.get("backgroundTaskId") if isinstance(result, dict) else None
                match = re.search(r"Command running in background with ID: ([A-Za-z0-9_-]+)", str(block.get("content", "")))
                if task or match:
                    task_ids.add(task or match.group(1))
    notices = []
    for i, row in enumerate(rows):
        fields = notification_fields(row)
        if not fields:
            continue
        task, source, status = fields
        if task in task_ids and source == tool_id and status == "completed":
            notices.append(i)
    if len(task_ids) != 1 or len(notices) != 1 or not tool_index < armed < notices[0] < auto:
        raise ValueError("S4 requires a matching completion notice after ARMED and before AUTO")
    notice = notices[0]
    message_ids = [rows[i].get("message", {}).get("id") for i in (armed, auto)]
    if all(message_ids) and message_ids[0] == message_ids[1]:
        raise ValueError("S4 AUTO belongs to the initiating assistant message")
    if any(armed < i < notice or i > auto for i, _ in work):
        raise ValueError("S4 assistant work crosses a terminal turn boundary")
    # Follow native parent links so disconnected or spoofed marker rows cannot prove a turn.
    if not linked(notice, armed) or not linked(auto, notice):
        raise ValueError("S4 lacks a continuous native parent chain across turn boundaries")
    return {"bash_tool_use_id": tool_id, "task_id": next(iter(task_ids)),
            "armed_uuid": rows[armed]["uuid"], "notification_uuid": rows[notice]["uuid"],
            "auto_uuid": rows[auto]["uuid"]}


def fetch_messages(base, channel_id, *, after_id="0", max_pages=100):
    """Walk forward from the supplied cursor; refuse a truncated audit."""
    client, result = DiscordClient(base.rstrip("/")), {}
    cursor = int(after_id)
    for _ in range(max_pages):
        page = client.fetch_messages(channel_id, after_id=str(cursor), limit=100)
        fresh = [m for m in page if int(m["id"]) > cursor]
        if not fresh:
            if page:
                raise ValueError("Discord pagination did not advance")
            return list(result.values())
        result.update((str(m["id"]), m) for m in fresh)
        cursor = max(int(m["id"]) for m in fresh)
        if len(page) < 100:
            return list(result.values())
    raise ValueError("Discord page limit reached; narrow with --after-id or raise --max-pages")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--autonomous-background-turn", action="store_true",
                        help="verify S4 background native turn evidence (requires --run-id)")
    parser.add_argument("--agent-id")
    parser.add_argument("--channel-id")
    selector = parser.add_mutually_exclusive_group(required=True)
    selector.add_argument("--marker-regex")
    selector.add_argument("--run-id")
    parser.add_argument("--api-base", default="http://127.0.0.1:8791")
    parser.add_argument("--transcript-root", help="local provider projects/sessions root")
    parser.add_argument("--input-mirror-author-id", action="append", default=[],
                        help="additional bot/webhook input-mirror author ID; repeatable")
    parser.add_argument("--deliver-author-id", action="append", default=[],
                        help="turn/deliver author ID whose queued inputs lack a Discord mirror; repeatable")
    parser.add_argument("--after-id", default="0", help="Discord lower cursor; default scans full history")
    parser.add_argument("--max-pages", type=int, default=100)
    args = parser.parse_args(argv)
    if not args.agent_id and not args.channel_id:
        parser.error("--agent-id or --channel-id is required")
    try:
        marker_pattern(marker_regex=args.marker_regex, run_id=args.run_id)
        if not args.after_id.isdigit() or args.max_pages < 1:
            raise ValueError("after-id must be digits and max-pages must be positive")
        binding = resolve_binding(args.api_base, agent_id=args.agent_id, channel_id=args.channel_id,
                                  transcript_root=args.transcript_root)
        if args.autonomous_background_turn:
            if not args.run_id or binding["provider"] != "claude":
                raise ValueError("S4 evidence requires --run-id and Claude provider")
            evidence = autonomous_background_turn(binding["transcript_path"],
                armed_marker=f"[E2E:S4:{args.run_id}:ARMED]", auto_marker=f"[E2E:S4:{args.run_id}:AUTO]")
            print(json.dumps({"binding": binding, "autonomous_background_turn": evidence}, indent=2))
            return 0
        messages = fetch_messages(args.api_base, binding["channel_id"], after_id=args.after_id,
                                  max_pages=args.max_pages)
        result = compare(binding["transcript_path"], messages,
                         marker_regex=args.marker_regex, run_id=args.run_id,
                         input_mirror_author_ids=args.input_mirror_author_id,
                         deliver_author_ids=args.deliver_author_id)
        print(json.dumps({"binding": binding, "markers": result}, ensure_ascii=False, indent=2))
        return int(any(row["verdict"] not in PASSING_VERDICTS for row in result.values()))
    except (OSError, ValueError, RuntimeError, KeyError, TypeError, AttributeError, re.error) as error:
        print(json.dumps({"error": str(error)}, ensure_ascii=False), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
