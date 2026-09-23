#!/usr/bin/env python3
"""Exercise actual hook-relay CLI consumers without contacting a real MCP server.

Only synthetic hook observations are submitted. No MCP tool
is executed. All config, relay queues, and receipts live in a temporary runtime.
This verifies the AgentDesk consumer, not provider enforcement of hook output.
"""

import argparse
import concurrent.futures
import copy
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class MockRelay(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        # Any accidental MCP request is a test failure, even on loopback.
        valid = self.path.startswith(("/hooks/claude/", "/hooks/codex/"))
        with self.server.observation_lock:
            self.server.observations.append({"path": self.path, "hook": valid})
        if valid:
            json.loads(body)
        self.send_response(200 if valid else 500)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"{}")

    def log_message(self, *_args):
        pass


def queues_idle(root):
    """Wait for our detached relay workers using their locks, never process kills."""
    runtime = root / "runtime"
    if any(runtime.rglob("*.ingress.json")) or any(runtime.rglob("*.request.json")):
        return False
    for path in runtime.rglob("worker.lock"):
        try:
            with path.open("rb") as handle:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(handle, fcntl.LOCK_UN)
        except BlockingIOError:
            return False
    return True


class ConsumerFixture:
    def __init__(self, binary, root, endpoint, evidence):
        self.binary = binary
        self.root = root
        self.endpoint = endpoint
        self.evidence = evidence
        self.calls = 0
        self.lock = threading.Lock()
        config_path = root / "config" / "agentdesk.yaml"
        config_path.parent.mkdir()
        # JSON is valid YAML; no third-party serializer or real config is used.
        config_path.write_text(json.dumps({
            "server": {"host": "127.0.0.1", "port": 0},
            "data": {"dir": str(root / "data")},
            "mcp_servers": {"memento": {
                "url": endpoint + "/fake-mcp-must-never-be-called",
                "auth": {"type": "bearer", "token_env_var": "MEMENTO_SMOKE_TOKEN"},
            }},
        }))
        # Do not inherit live credentials, worker handoffs, or config overrides.
        self.env = {key: value for key, value in os.environ.items()
                    if key in {"PATH", "TMPDIR", "LANG", "LC_ALL", "SYSTEMROOT"}}
        self.env.update({"AGENTDESK_ROOT_DIR": str(root),
                         "AGENTDESK_CONFIG": str(config_path),
                         "MEMENTO_SMOKE_TOKEN": "synthetic-consumer-test-identity"})

    def invoke(self, provider, event, payload, expected_deny, label, session="smoke-session"):
        with self.lock:
            self.calls += 1
        result = subprocess.run([
            str(self.binary), provider + "-hook-relay", "--endpoint", self.endpoint,
            "--provider", provider, "--event", event, "--session-id", session,
        ], input=json.dumps(payload), text=True, capture_output=True,
            cwd=self.root, env=self.env, timeout=15, check=False)
        if result.returncode != 0:
            raise AssertionError(f"{provider}/{label}: CLI exit {result.returncode}: {result.stderr}")
        try:
            output = json.loads(result.stdout)
        except ValueError as error:
            raise AssertionError(f"{provider}/{label}: invalid hook stdout {result.stdout!r}") from error
        specific = output.get("hookSpecificOutput", {})
        denied = specific.get("permissionDecision") == "deny"
        if denied != expected_deny:
            raise AssertionError(f"{provider}/{label}: expected deny={expected_deny}, got {output}")
        if denied and (specific.get("hookEventName") != "PreToolUse"
                       or not specific.get("permissionDecisionReason")):
            raise AssertionError(f"{provider}/{label}: incomplete deny contract: {output}")
        with self.lock:
            self.evidence["checks"].append({"provider": provider, "case": label,
                                             "event": event, "denied": denied})

    def exercise(self, provider):
        serial = 0

        def payload(content, **fields):
            nonlocal serial
            serial += 1
            return {"tool_name": "mcp__memento__remember", "tool_use_id": f"call-{serial}",
                    "tool_input": {"content": f"{provider}: {content}", "type": "fact",
                                   "topic": "family", "workspace": "family", **fields}}

        def pre(call, deny, label, session="smoke-session"):
            self.invoke(provider, "PreToolUse", call, deny, label, session)

        def post(call, label, response=None, session="smoke-session"):
            observed = copy.deepcopy(call)
            observed["tool_response"] = response if response is not None else {
                "content": [{"type": "text", "text": '{"fragmentId":"synthetic-fragment"}'}]}
            self.invoke(provider, "PostToolUse", observed, False, label, session)

        baseline = payload("Child now likes pears", assertionStatus="verified")
        pre(baseline, False, "first new fact")
        post(baseline, "correlated successful remember")
        duplicate = copy.deepcopy(baseline)
        duplicate["tool_use_id"] = "duplicate-other-invocation"
        pre(duplicate, True, "confirmed duplicate blocked across CLI processes")
        pre(payload("Child now likes apples", assertionStatus="verified"), False, "new family fact preserved")
        changed = copy.deepcopy(duplicate)
        changed["tool_input"]["assertionStatus"] = "rejected"
        pre(changed, False, "status change preserved")
        changed = copy.deepcopy(duplicate)
        changed["tool_input"]["scope"] = "session"
        pre(changed, False, "scope change preserved")
        post(changed, "session scoped success", session="smoke-session")
        pre(changed, True, "same session duplicate blocked")
        pre(changed, False, "distinct provider session preserved", session="other-session")
        changed = copy.deepcopy(duplicate)
        changed["tool_input"]["content"] = f"{provider}: Child now likes  pears"
        pre(changed, False, "changed content whitespace preserved conservatively")
        changed = copy.deepcopy(duplicate)
        changed["tool_input"]["source"] = "another source"
        pre(changed, True, "source only change blocked")

        for mutation in ("amend", "forget", "memory_consolidate", "session_rotate"):
            pre(duplicate, True, f"confirmed before {mutation}")
            mutation_call = payload("Real correction")
            mutation_call["tool_name"] = f"mcp__memento__{mutation}"
            if mutation == "amend":
                mutation_call["tool_input"] = {"id": "synthetic-fragment", "content": "Real correction"}
            elif mutation == "forget":
                mutation_call["tool_input"] = {"id": "synthetic-fragment"}
            elif mutation == "session_rotate":
                mutation_call["tool_input"] = {"reason": "synthetic smoke test"}
            else:
                mutation_call["tool_input"] = {}
            pre(mutation_call, False, f"{mutation} allowed")
            post(mutation_call, f"{mutation} post allowed")
            pre(baseline, False, f"remember permitted after {mutation}")
            post(baseline, f"remember success after {mutation}")

        missing = payload("New family fact with lost response")
        pre(missing, False, "missing post first attempt")
        missing["tool_use_id"] = "missing-post-retry"
        pre(missing, False, "missing post retry remains allowed")
        ambiguous = payload("New family fact with ambiguous response")
        pre(ambiguous, False, "ambiguous result first attempt")
        post(ambiguous, "ambiguous result observed", {"jsonrpc": "2.0", "id": "rpc-only"})
        ambiguous["tool_use_id"] = "ambiguous-retry"
        pre(ambiguous, False, "ambiguous result retry remains allowed")
        concurrent_call = payload("Concurrent first family fact")
        contenders = []
        for index in range(4):
            contender = copy.deepcopy(concurrent_call)
            contender["tool_use_id"] = f"concurrent-{index}"
            contenders.append(contender)
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
            futures = [executor.submit(pre, contender, False, f"concurrent first allowed {index}")
                       for index, contender in enumerate(contenders)]
            for future in futures:
                future.result()
        post(contenders[0], "concurrent owner success")
        pre(concurrent_call, True, "confirmed concurrent payload subsequently blocked")

        reflect = payload("Session lesson with new knowledge")
        reflect["tool_name"] = "memento.reflect" if provider == "codex" else "mcp__memento__reflect"
        reflect["tool_input"] = {"summary": f"{provider}: Session lesson with new knowledge", "workspace": "family"}
        pre(reflect, False, "new reflect allowed")
        post(reflect, "reflect successful response")
        reflect["tool_use_id"] = "reflect-retry"
        pre(reflect, True, "confirmed reflect duplicate blocked")
        unrelated = payload("Unrelated tool data")
        unrelated["tool_name"] = "mcp__other__remember"
        pre(unrelated, False, "unrelated MCP tool preserved")
        self.invoke(provider, "Notification", unrelated, False, "unrelated event preserved")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output-json", type=Path, help="Optional evidence file")
    options = parser.parse_args()
    if not options.binary.is_absolute() or not options.binary.is_file():
        parser.error("--binary must be an existing absolute file path")
    binary = options.binary.resolve()
    digest = hashlib.sha256()
    with binary.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    evidence = {"binary": str(binary), "binary_sha256": digest.hexdigest(), "checks": [],
                "boundaries": ["Synthetic observations only; no real MCP or DB mutations",
                               "Concurrent unconfirmed first calls remain allowed",
                               "Missing/ambiguous post result remains retryable",
                               "Provider enforcement and semantic paraphrase dedup are not proven"]}
    root = Path(tempfile.mkdtemp(prefix="adk-memento-consumption-"))
    server = ThreadingHTTPServer(("127.0.0.1", 0), MockRelay)
    server.observations = []
    server.observation_lock = threading.Lock()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    fixture = ConsumerFixture(binary, root, f"http://127.0.0.1:{server.server_port}", evidence)
    failure = None
    try:
        for provider in ("claude", "codex"):
            fixture.exercise(provider)
    except Exception as error:
        failure = str(error)
    finally:
        # Keep the mock online until detached workers have delivered and exited.
        deadline = time.monotonic() + 15
        idle_since = None
        while time.monotonic() < deadline:
            idle_since = (idle_since or time.monotonic()) if queues_idle(root) else None
            if idle_since is not None and time.monotonic() - idle_since >= 0.75:
                break
            time.sleep(0.05)
        idle = queues_idle(root)
        with server.observation_lock:
            observations = list(server.observations)
        evidence["relay_calls"] = len(observations)
        evidence["cli_calls"] = fixture.calls
        if any(not item["hook"] for item in observations):
            failure = failure or "Unexpected non-hook HTTP request attempted"
        if len(observations) != fixture.calls:
            failure = failure or "Not every CLI observation reached the isolated relay"
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
        if idle:
            shutil.rmtree(root)
        else:
            evidence["preserved_temporary_root"] = str(root)
            failure = failure or "Isolated relay workers did not quiesce; fixture retained, no process killed"
    evidence["passed"] = failure is None
    if failure:
        evidence["error"] = failure
    rendered = json.dumps(evidence, indent=2, ensure_ascii=False) + "\n"
    if options.output_json:
        options.output_json.write_text(rendered)
    print(rendered, end="")
    return 1 if failure else 0


if __name__ == "__main__":
    raise SystemExit(main())
