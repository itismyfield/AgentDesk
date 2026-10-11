#!/usr/bin/env python3
"""Fixed-fixture checks of the P11 M2 activation judge: a real pair passes; a fake-only run, a
skipped run, a missing witness, a binary of another hash, X1 without X2, and a write answered by a
server other than the one the test started (N-2) each fail on exactly their own reason."""
from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("activation", HERE / "test_p11_m2_activation.py")
activation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(activation)

BIN = "/scratch/x12/bin/herdr"
SHA = activation.PINNED_SHA256
TEST_PID = "500"


def sock(server: str) -> str:
    return f"/scratch/x12/r1/{server}/run/h.sock"


def log_path(server: str) -> str:
    return f"/scratch/x12/r1/{server}/home/xdg/herdr/herdr-server.log"


def server_log(server: str, pid: str, requests: list[tuple[str, str, str]]) -> str:
    lines = [f"2026-10-11T12:00:00Z  INFO herdr::api::server: api server listening path={sock(server)}",
             '2026-10-11T12:00:00Z  WARN herdr::logging: update check failed '
             'event="update.check.complete" subsystem="update" outcome="error" '
             'err="curl failed: No such file or directory (os error 2)"']
    for rid, method, outcome in requests:
        lines.append('2026-10-11T12:00:01Z  INFO herdr::logging: api request received '
                     f'event="api.request.start" subsystem="api" outcome="started" '
                     f'request_id="{rid}" method="{method}" changes_ui=true')
        lines.append('2026-10-11T12:00:01Z  INFO herdr::logging: api request completed '
                     f'event="api.request.complete" subsystem="api" outcome="{outcome}" '
                     f'request_id="{rid}" method="{method}"')
    lines.append('2026-10-11T12:00:09Z  INFO herdr::logging: herdr exiting '
                 f'event="app.shutdown" subsystem="server" outcome="completed" pid={pid}')
    return "\n".join(lines) + "\n"


def pane(pane_id: str, workspace: str, shell: int, child: int | None) -> dict:
    return {"pane_id": pane_id, "workspace_id": workspace, "terminal_id": f"term_{pane_id}",
            "shell_pid": shell, "foreground": [shell] + ([child] if child else [])}


def fixture(name: str) -> tuple[str, dict[str, str]]:
    """A passing run's output and the server logs it names."""
    test, result, outcome = activation.TESTS[name]
    target = {"x1": "x1-s1", "x2": "x2-s2"}[name]
    servers = {"x1": [("x1-s1", "600"), ("x1-s2", "601")], "x2": [("x2-s2", "610")]}[name]
    target_pid = dict(servers)[target]
    nonce, key = "0123abcd" * 4, f"claude/h/mac-mini:AgentDesk-claude-{name}"
    before = [pane("w1:p1", "w1", 700, 701), pane("w2:p1", "w2", 702, 703)]
    if name == "x2":
        before.append(pane("w3:p1", "w3", 704, None))
    after = before[1:] if name == "x1" else before
    lines = ["running 1 test",
             f"test {activation.MODULE}::{test} ... X_TEST name={name} pid={TEST_PID}"]
    for server, pid in servers:
        lines.append(f"X_SERVER name={server} pid={pid} pgid={pid} socket={sock(server)} "
                     f"start=darwin:1791714496.{pid}000 log={log_path(server)}")
    lines.append(f"X_EXECUTION server={target} session_key={key} nonce={nonce} pane=w1:p1 "
                 "workspace=w1 shell=700 provider=701")
    lines.append(f"X_SNAPSHOT server={target} phase=before panes={json.dumps(before, separators=(',', ':'))}")
    lines.append(f"X_CLI ok={name == 'x1'} detail=None")
    lines.append(f"X_WRITE id=adk-7 method=pane.close outcome={outcome} socket={sock(target)} "
                 f"pid={target_pid} start=darwin:1791714496.{target_pid}000")
    lines.append(f"X_EXE pid={target_pid} path={BIN} sha256={SHA}")
    lines.append(f'X_RESULT_LOG result={result} session_key={key} execution_nonce={nonce} '
                 'reason="operator test" detail=None')
    lines.append(f"X_SNAPSHOT server={target} phase=after panes={json.dumps(after, separators=(',', ':'))}")
    for server, _ in servers:
        lines.append(f"X_CURL server={server} children=0 update_check=spawn_failed")
    lines.append(f"X_EFFECT name={name} {activation.EFFECT[name]}")
    for server, pid in servers:
        lines.append(f"X_STOPPED name={server} pid={pid} forced=false hosted_left=[]")
    lines += ["ok", "",
              "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 13030 filtered out"]
    close_outcome = "ok" if name == "x1" else "error"
    logs = {}
    for server, pid in servers:
        requests = [("x-setup-1", "workspace.create", "ok"), ("x-setup-2", "pane.send_input", "ok")]
        if server == target:
            requests.append(("adk-7", "pane.close", close_outcome))
        logs[log_path(server)] = server_log(server, pid, requests)
    return "\n".join(lines) + "\n", logs


def judged(name: str, output: str, logs: dict[str, str]) -> list[str]:
    return activation.judge(name, output, bin_path=BIN, read_log=lambda path: logs.get(path, ""))


def replace(text: str, old: str, new: str) -> str:
    assert old in text, old
    return text.replace(old, new)


def main() -> int:
    checks = []

    def expect(label: str, failures: list[str], wanted: list[str]) -> None:
        ok = failures == wanted
        checks.append(ok)
        print(f"{'ok  ' if ok else 'FAIL'} {label}: {failures}")

    real = {name: judged(name, *fixture(name)) for name in activation.TESTS}
    expect("real pair passes", activation.judge_all(real), [])

    # The write answered inside the test process: an in-process fake server.
    output, logs = fixture("x1")
    output = replace(output, f"pid={TEST_PID}", "pid=600")
    expect("fake-only fails", judged("x1", output, logs),
           ["W3: the write was answered inside the test process"])

    # Built and listed but skipped: not a run.
    output, logs = fixture("x2")
    test = activation.TESTS["x2"][0]
    output = replace(output, f"{test} ... X_TEST", f"{test} ... ignored\nX_TEST")
    output = replace(output, "ok. 1 passed; 0 failed; 0 ignored", "ok. 0 passed; 0 failed; 1 ignored")
    expect("skip fails", judged("x2", output, logs),
           [f"W1: `{test}` did not run", "W2: not exactly one passed test"])

    output, logs = fixture("x1")
    output = "\n".join(line for line in output.splitlines() if not line.startswith("X_WRITE "))
    expect("missing witness fails", judged("x1", output, logs),
           ["W3: 0 write witnesses, expected 1"])

    output, logs = fixture("x2")
    output = replace(output, f"sha256={SHA}", "sha256=" + "0" * 64)
    expect("sha mismatch fails", judged("x2", output, logs),
           [f"W3: binary sha256 {'0' * 64} is not the pinned 0.9.3"])

    expect("x1 only fails", activation.judge_all({"x1": real["x1"]}), ["x2: not run"])

    # N-2: a fake server outside the test process answered on the socket while the real server
    # the test started ran beside it; its log never took the request.
    output, logs = fixture("x1")
    output = replace(output, "X_WRITE id=adk-7 method=pane.close outcome=ok socket="
                     f"{sock('x1-s1')} pid=600 start=darwin:1791714496.600000",
                     f"X_WRITE id=adk-7 method=pane.close outcome=ok socket={sock('x1-s1')} "
                     "pid=650 start=darwin:1791714497.000001")
    output = replace(output, f"X_EXE pid=600 path={BIN}", f"X_EXE pid=650 path={BIN}")
    logs[log_path("x1-s1")] = replace(logs[log_path("x1-s1")], 'request_id="adk-7"',
                                      'request_id="other"')
    expect("fake server beside the real one fails", judged("x1", output, logs),
           ["W3: the write's process is not the started server",
            "W3: the server log took request adk-7 as []"])

    print(f"SELFTEST {'PASS' if all(checks) else 'FAIL'} {sum(checks)}/{len(checks)}")
    return 0 if all(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
