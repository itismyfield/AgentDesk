#!/usr/bin/python3
"""Offline native-stdin provider for the isolated T14 client acceptance fixture."""
import base64
import datetime
import hashlib
import json
import os
import pathlib
import select
import sys
import termios
import time
import traceback
import tty
import urllib.parse
import urllib.request
import uuid

ROOT = pathlib.Path(__ROOT__).resolve()
CASE = pathlib.Path(__CASE__).resolve()
PROVIDER = __PROVIDER__
SCRIPT_PROVIDER = pathlib.Path(sys.argv[0]).name

def append(path, value):
    data = (json.dumps(value, separators=(",", ":")) + "\n").encode()
    with open(path, "ab", buffering=0) as f:
        f.write(data)
        os.fsync(f.fileno())

def atomic(path, value):
    temp = pathlib.Path(str(path) + ".tmp")
    with open(temp, "w") as f:
        json.dump(value, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temp, path)

def provider_error(kind, value, trace):
    atomic(CASE / "provider-error.json", {"type": kind.__name__, "message": str(value),
        "traceback": "".join(traceback.format_exception(kind, value, trace))})
    sys.__excepthook__(kind, value, trace)

sys.excepthook = provider_error

def wait_file(path):
    end = time.monotonic() + 90
    while not pathlib.Path(path).is_file():
        if time.monotonic() >= end:
            raise RuntimeError("T14 missing owned control/release: " + str(path))
        time.sleep(0.025)

def stamp():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")

if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.160.0" if SCRIPT_PROVIDER == "codex" else "2.1.81 (Claude Code)")
    raise SystemExit(0)
if SCRIPT_PROVIDER == "codex" and sys.argv[1:] == ["resume", "--help"]:
    print("--dangerously-bypass-hook-trust --no-daemon")
    raise SystemExit(0)
if SCRIPT_PROVIDER != PROVIDER:
    append(CASE / "unexpected-provider.jsonl", {"pid": os.getpid(), "argv": sys.argv})
    raise SystemExit(97)
expected_isolation = {
    "PATH": str(ROOT / "bin") + ":/usr/bin:/bin",
    "HOME": str(CASE / "home"),
    "CODEX_HOME": str(CASE / ("home/.adk/profiles/codex/t14-empty" if PROVIDER == "codex" else "codex-home")),
    "CLAUDE_CONFIG_DIR": str(CASE / "claude-config"),
    "XDG_CONFIG_HOME": str(CASE / "home/xdg"),
    "XDG_STATE_HOME": str(CASE / "state"),
    "XDG_DATA_HOME": str(CASE / "data"),
    "XDG_CACHE_HOME": str(CASE / "cache"),
    "XDG_RUNTIME_DIR": str(CASE / "run"),
    "TMPDIR": str(CASE / "tmp"),
    "SHELL": "/bin/bash",
}
isolation = {key: os.environ.get(key) for key in expected_isolation}
atomic(CASE / "provider-isolation.json", {"pid": os.getpid(), "expected": expected_isolation,
    "isolation": isolation, "matches": isolation == expected_isolation})
assert isolation == expected_isolation, "T14 actual provider exec environment must remain sealed"
context = pathlib.Path(os.environ["AGENTDESK_BINDING_CONTEXT"]).resolve(strict=True)
assert context.is_relative_to(ROOT), "T14 context must be in the owned scratch"
herdr_all_env = {k: v for k, v in os.environ.items() if k.startswith("HERDR_")}
herdr_env = {k: os.environ[k] for k in ("HERDR_ENV", "HERDR_PANE_ID", "HERDR_BIN_PATH", "HERDR_SOCKET_PATH", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID", "HERDR_CONFIG_PATH") if k in os.environ}
assert not herdr_env, "T14 ADK provider must have Herdr injection variables removed"
assert not herdr_all_env, "T14 provider must not inherit server-only diagnostics"
seen = {"pid": os.getpid(), "pgid": os.getpgrp(), "ppid": os.getppid(),
        "provider": PROVIDER, "script": str(pathlib.Path(sys.argv[0]).resolve()),
        "argv": sys.argv, "context": str(context), "context_sha256": hashlib.sha256(context.read_bytes()).hexdigest(),
        "herdr_env": herdr_env, "herdr_all_env": herdr_all_env, "interpreter": sys.executable,
        "isolation": isolation}
atomic(CASE / "context-seen.json", seen)
append(CASE / "provider-starts.jsonl", seen)
wait_file(CASE / "control.json")
control = json.loads((CASE / "control.json").read_text())
assert control["provider"] == PROVIDER
assert len(control["prompts"]) == len(control["answers"]) == 3
assert hashlib.sha256(context.read_bytes()).hexdigest() == seen["context_sha256"]
context_data = json.loads(context.read_text())
assert context_data["provider"] == PROVIDER
# BindingContext.host identifies the execution node; the actual pane proves its backend.
assert context_data["host"] == control["execution_node"]
assert context_data["launch_mode"] == "fresh"
args = sys.argv[1:]
assert not any(arg in ("resume", "continue") or arg.startswith(("--resume", "--continue")) for arg in args)
if PROVIDER == "claude":
    at = args.index("--session-id")
    assert args[at + 1] == control["session_id"], "T14 native session came from the actual prepared argv"
if context_data.get("expected_native_session_id") is not None:
    assert context_data["expected_native_session_id"] == control["session_id"]
uuid.UUID(control["session_id"])
envelope = control["binding_envelope"]
decoded = json.loads(base64.urlsafe_b64decode(envelope + "=" * (-len(envelope) % 4)))
assert decoded["context"]["Captured"] == context_data, "T14 hook header captures this actual exec context"
source = pathlib.Path(control["transcript_path"]).resolve()
assert source.is_relative_to(ROOT)
source.parent.mkdir(parents=True, exist_ok=True)
assert not source.exists(), "T14 provider alone creates its native source"
parsed = urllib.parse.urlparse(control["hook_url"])
assert parsed.scheme == "http" and parsed.hostname == "127.0.0.1" and parsed.port != 8791
assert parsed.path in ("", "/") and not parsed.query and not parsed.fragment
session = control["session_id"]
logical = json.loads(context.read_text())["tmux_session"]

def hook(event, extra):
    payload = {"hook_event_name": event, "session_id": session, "cwd": os.getcwd(), "transcript_path": str(source)}
    payload.update(extra)
    url = control["hook_url"].rstrip("/") + "/hooks/" + PROVIDER + "/" + event + "?session_id=" + urllib.parse.quote(logical)
    request = urllib.request.Request(url, json.dumps(payload).encode(),
        {"content-type": "application/json", "x-agentdesk-binding-context": control["binding_envelope"]}, method="POST")
    # Empty proxies make the actual HTTP request local even if a caller's environment is unusual.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=10) as response:
        assert response.status == 202, "T14 actual hook ingress must accept"
        response.read()
    append(CASE / "hook-receipts.jsonl", {"event": event, "url": url, "payload": payload, "status": 202})

def startup():
    if PROVIDER == "codex":
        append(source, {"timestamp": stamp(), "type": "session_meta", "payload": {
            "id": session, "session_id": session, "cwd": os.getcwd(), "originator": "codex-tui",
            "cli_version": "0.160.0", "source": "cli"}})
    else:
        with open(source, "xb") as f:
            f.flush()
            os.fsync(f.fileno())
    hook("SessionStart", {"source": "startup"})

def ready(answer=""):
    if PROVIDER == "codex":
        text = answer + "\r\n\r\n›\r\n\r\n  gpt-5.5 xhigh · " + os.getcwd()
    else:
        line = "────────────────────────────────────────────────────"
        text = answer + "\r\n" + line + "\r\n❯ \r\n" + line + "\r\n  🤖 Opus(H) │ 7% │ MCP: 2"
    os.write(1, ("\x1b[2J\x1b[H\x1b[?2004h" + text).encode())

original = termios.tcgetattr(0)
tty.setraw(0)
try:
    if PROVIDER == "claude":
        startup()
    ready("earlier output")
    atomic(CASE / "provider-ready.json", {"pid": os.getpid(), "provider": PROVIDER})
    raw = bytearray()
    seq = 0
    bracket = False
    while seq < 3:
        chunk = os.read(0, 1)
        if not chunk:
            raise RuntimeError("T14 stdin ended before its three real inputs")
        raw.extend(chunk)
        if raw.endswith(b"\x1b[200~"):
            bracket = True
        elif raw.endswith(b"\x1b[201~"):
            bracket = False
        if chunk != b"\r" or bracket:
            continue
        seq += 1
        prompt = bytes(raw[:-1]).replace(b"\x1b[200~", b"").replace(b"\x1b[201~", b"").decode("utf-8")
        assert control["prompts"][seq - 1] in prompt, "T14 actual stdin carries the expected prompt"
        append(CASE / "inputs.jsonl", {"seq": seq, "raw_bytes": list(raw), "prompt": prompt})
        raw.clear()
        if PROVIDER == "codex" and seq == 1:
            startup()
        turn = str(uuid.uuid4())
        if PROVIDER == "claude":
            append(source, {"timestamp": stamp(), "sessionId": session, "type": "user", "uuid": turn,
                "message": {"role": "user", "content": prompt}})
            direct = {"prompt": prompt, "prompt_id": turn}
        else:
            for record in [
                {"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn}},
                {"type": "turn_context", "payload": {"turn_id": turn}},
                {"timestamp": stamp(), "type": "response_item", "payload": {"id": str(uuid.uuid4()),
                    "type": "message", "role": "user", "content": [{"type": "input_text", "text": prompt}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": turn, "create_time": time.time(), "content_item_kinds": ["user.text"]}}},
                {"type": "event_msg", "payload": {"type": "item_completed", "turn_id": turn, "item": {"type": "UserMessage"}}},
            ]:
                append(source, record)
            direct = {"prompt": prompt, "turn_id": turn}
        if seq == 2:
            hook("UserPromptSubmit", direct)
        wait_file(CASE / ("answer-" + str(seq) + ".release"))
        if seq == 2:
            # Same actual hook identity replays after the live claim, before its answer is complete.
            hook("UserPromptSubmit", direct)
        answer = control["answers"][seq - 1]
        if PROVIDER == "claude":
            for record in [
                {"type": "assistant", "uuid": "t14-" + str(uuid.uuid4()), "apiBlockIndex": 0,
                    "message": {"id": str(uuid.uuid4()), "role": "assistant", "stop_reason": "end_turn", "content": [{"type": "text", "text": answer}]}},
                {"type": "system", "subtype": "stop_hook_summary", "hookCount": 1, "hasOutput": True},
                {"type": "system", "subtype": "turn_duration"},
            ]:
                record.update({"timestamp": stamp(), "sessionId": session})
                append(source, record)
        else:
            append(source, {"timestamp": stamp(), "type": "response_item", "payload": {
                "id": str(uuid.uuid4()), "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": answer}]}})
            append(source, {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": turn, "last_agent_message": answer}})
        ready(answer)
    # Keep the actual provider and pane identity alive through all post-roundtrip assertions.
    while True:
        extra = os.read(0, 1)
        if not extra:
            break
        append(CASE / "post-inputs.jsonl", {"raw_bytes": list(extra)})
finally:
    termios.tcsetattr(0, termios.TCSANOW, original)
