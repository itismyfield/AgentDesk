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

from .assertions import is_our_send, relay_body
from .discord import DiscordClient
from .normal_intake_evidence import content, tool_result

DIRECT_NOTICE = re.compile(r"^터미널에 직접 주입된 입력 \(tmux : `[^`]+`\):")


def marker_pattern(*, marker_regex=None, run_id=None):
    if bool(marker_regex) == bool(run_id):
        raise ValueError("provide exactly one of marker_regex or run_id")
    pattern = re.compile(marker_regex or rf"\[E2E:[^:\]\s]+:{re.escape(run_id)}(?::[^\]\s]+)?\]")
    if pattern.search(""):
        raise ValueError("marker regex must not match empty text")
    return pattern


def markers(text, pattern):
    return {m.groupdict().get("marker", m.group(0)) for m in pattern.finditer(text)} - {None, ""}


def native_inputs(path):
    """Yield actual user text, excluding Claude tool results and duplicate Codex event mirrors."""
    with Path(path).open(encoding="utf-8") as stream:
        for number, line in enumerate(stream, 1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
                if not isinstance(row, dict):
                    raise ValueError("expected object")
                if (row.get("type") == "user" and not row.get("isMeta")
                        and not row.get("isCompactSummary") and not tool_result(row)):
                    yield content(row)
                elif row.get("type") == "response_item":
                    payload = row.get("payload", {})
                    if payload.get("type") == "message" and payload.get("role") == "user":
                        blocks = payload.get("content", [])
                        yield "\n".join(b.get("text", "") for b in blocks
                                        if b.get("type") in ("input_text", "text"))
            except (ValueError, TypeError, AttributeError) as error:
                raise ValueError(f"invalid transcript {path}:{number}: {error}") from error


def compare(path, messages, *, marker_regex=None, run_id=None, input_mirror_author_ids=()):
    """Return per-marker counts; same-ID Discord edits count only their last snapshot."""
    pattern = marker_pattern(marker_regex=marker_regex, run_id=run_id)
    native = Counter(marker for text in native_inputs(path) for marker in markers(text, pattern))
    mirrors, bodies, notices = Counter(), Counter(), Counter()
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
        else:
            text = relay_body(message) or ""
            target = bodies
        target.update(markers(text, pattern))
    result = {}
    for marker in sorted(native.keys() | mirrors.keys() | bodies.keys() | notices.keys()):
        n, m, b, d = native[marker], mirrors[marker], bodies[marker], notices[marker]
        if max(n, m, b, d) > 1 or m + d > 1:
            verdict = "duplicated"
        elif n == 0 or m + d == 0:
            verdict = "lost"
        elif b == 0:
            verdict = "missing_relay"
        else:
            verdict = "ok"
        result[marker] = dict(native_user_count=n, discord_input_mirror_count=m,
                              relay_body_count=b, direct_input_notice_count=d, verdict=verdict)
    if not result:
        raise ValueError("no markers found; empty evidence cannot pass")
    return result


def get_json(base, path):
    request = urllib.request.Request(base.rstrip("/") + path, method="GET")
    with urllib.request.urlopen(request, timeout=15) as response:
        return json.load(response)


def resolve_binding(base, *, agent_id=None, channel_id=None, transcript_root=None):
    """Use turn.session_key plus the documented provider-ID lookup; never inspect the DB."""
    bindings = get_json(base, "/api/discord/bindings")["bindings"]
    matches = [b for b in bindings if (not agent_id or b.get("agentId") == agent_id)
               and (not channel_id or str(b.get("channelId")) == str(channel_id))]
    if len(matches) != 1:
        raise ValueError(f"expected one binding, found {len(matches)}; specify agent/channel pair")
    binding = matches[0]
    agent = urllib.parse.quote(binding["agentId"], safe="")
    turn = get_json(base, f"/api/agents/{agent}/turn")
    key, provider = turn.get("session_key"), turn.get("provider")
    if not key or provider not in ("claude", "codex") or provider != binding.get("provider"):
        raise ValueError("turn lacks an unambiguous Claude/Codex binding")
    query = urllib.parse.urlencode({"session_key": key, "provider": provider})
    identity = get_json(base, "/api/dispatched-sessions/claude-session-id?" + query)
    sid = identity.get("raw_provider_session_id") or identity.get("session_id") or identity.get("claude_session_id")
    if not isinstance(sid, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", sid):
        raise ValueError("session binding lacks a safe native provider session id")
    if transcript_root:
        root = Path(transcript_root).expanduser()
    elif provider == "claude":
        root = Path(os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude"))) / "projects"
    else:
        root = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "sessions"
    paths = list(root.glob(f"*/{sid}.jsonl" if provider == "claude" else f"**/rollout-*-{sid}.jsonl"))
    if len(paths) != 1:
        raise ValueError(f"expected one local transcript for {sid}, found {len(paths)} under {root}")
    return dict(agent_id=binding["agentId"], channel_id=str(binding["channelId"]),
                session_key=key, provider=provider, session_id=sid, transcript_path=str(paths[0]))


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
    parser.add_argument("--agent-id")
    parser.add_argument("--channel-id")
    selector = parser.add_mutually_exclusive_group(required=True)
    selector.add_argument("--marker-regex")
    selector.add_argument("--run-id")
    parser.add_argument("--api-base", default="http://127.0.0.1:8791")
    parser.add_argument("--transcript-root", help="local provider projects/sessions root")
    parser.add_argument("--input-mirror-author-id", action="append", default=[],
                        help="additional bot/webhook input-mirror author ID; repeatable")
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
        messages = fetch_messages(args.api_base, binding["channel_id"], after_id=args.after_id,
                                  max_pages=args.max_pages)
        result = compare(binding["transcript_path"], messages,
                         marker_regex=args.marker_regex, run_id=args.run_id,
                         input_mirror_author_ids=args.input_mirror_author_id)
        print(json.dumps({"binding": binding, "markers": result}, ensure_ascii=False, indent=2))
        return int(any(row["verdict"] != "ok" for row in result.values()))
    except (OSError, ValueError, RuntimeError, KeyError, TypeError, re.error) as error:
        print(json.dumps({"error": str(error)}, ensure_ascii=False), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
