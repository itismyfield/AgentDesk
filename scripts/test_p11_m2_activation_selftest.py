#!/usr/bin/env python3
"""Fixed-fixture checks of the P11 M2 activation judge: a real pair passes; each of these fails on
exactly its own reason: a fake-only run, a skipped run, a missing witness, a binary of another
hash, X1 without X2, a pair run on another head, a write answered by a server other than the one
the test started (N-2), a run that exited non-zero or timed out, an untracked file or a head
move during a run, a second effect line, a control pane swapped in, a group close in the server
log, and a missing or contrary O line."""
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
HEAD = "c0ffee" + "0" * 34


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
    module, test, result, outcome = activation.TESTS[name]
    target = {"x1": "x1-s1", "x2": "x2-s2"}[name]
    servers = {"x1": [("x1-s1", "600"), ("x1-s2", "601")], "x2": [("x2-s2", "610")]}[name]
    target_pid = dict(servers)[target]
    nonce, key = "0123abcd" * 4, f"claude/h/mac-mini:AgentDesk-claude-{name}"
    before = [pane("w1:p1", "w1", 700, 701), pane("w2:p1", "w2", 702, 703)]
    if name == "x2":
        before.append(pane("w3:p1", "w3", 704, None))
    after = before[1:] if name == "x1" else before
    lines = ["running 1 test",
             f"test {module}::{test} ... X_TEST name={name} pid={TEST_PID}"]
    for server, pid in servers:
        lines.append(f"X_SERVER name={server} pid={pid} pgid={pid} socket={sock(server)} "
                     f"start=darwin:1791714496.{pid}000 log={log_path(server)}")
    lines.append(f"X_EXECUTION server={target} session_key={key} nonce={nonce} pane=w1:p1 "
                 "workspace=w1 shell=700 provider=701")
    others = {"x1": [pane("w9:p1", "w9", 720, 721)], "x2": []}[name]
    for server, _ in servers:
        panes = before if server == target else others
        lines.append(f"X_SNAPSHOT server={server} phase=before panes={compact(panes)}")
    lines.append(f"X_CLI ok={name == 'x1'} detail=None")
    lines.append(f"X_WRITE id=adk-7 method=pane.close outcome={outcome} socket={sock(target)} "
                 f"pid={target_pid} start=darwin:1791714496.{target_pid}000")
    lines.append(f"X_EXE pid={target_pid} path={BIN} sha256={SHA}")
    lines.append(f'X_RESULT_LOG result={result} session_key={key} execution_nonce={nonce} '
                 'reason="operator test" detail=None')
    for server, _ in servers:
        panes = after if server == target else others
        lines.append(f"X_SNAPSHOT server={server} phase=after panes={compact(panes)}")
    for server, _ in servers:
        lines.append(f"X_CURL server={server} children=0 update_check=spawn_failed")
    if name in activation.O_EVIDENCE:
        lines.append(f"X_O name={name} {activation.O_EVIDENCE[name]}")
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


def compact(panes: list[dict]) -> str:
    return json.dumps(panes, separators=(",", ":"))


def judged(name: str, output: str, logs: dict[str, str], returncode: int | None = 0) -> list[str]:
    return activation.judge(name, output, bin_path=BIN, returncode=returncode,
                            read_log=lambda path: logs.get(path, ""))


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
    heads = {name: [(HEAD, True), (HEAD, True)] for name in activation.TESTS}
    expect("real pair passes", activation.judge_all(real, heads, HEAD), [])

    # The write answered inside the test process: an in-process fake server.
    output, logs = fixture("x1")
    output = replace(output, f"pid={TEST_PID}", "pid=600")
    expect("fake-only fails", judged("x1", output, logs),
           ["W3: the write was answered inside the test process"])

    # Built and listed but skipped: not a run.
    output, logs = fixture("x2")
    test = activation.TESTS["x2"][1]
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
    expect("binary sha mismatch fails", judged("x2", output, logs),
           [f"W3: binary sha256 {'0' * 64} is not the pinned 0.9.3"])

    expect("x1 only fails", activation.judge_all({"x1": real["x1"]}, heads, HEAD), ["x2: not run"])

    # X2 ran on another head than X1 and the PR.
    other = dict(heads, x2=[("f" * 40, True), ("f" * 40, True)])
    expect("head sha mismatch fails", activation.judge_all(real, other, HEAD),
           [f"x2: W5: before the run on {'f' * 40} clean=True, not the PR head {HEAD}",
            f"x2: W5: after the run on {'f' * 40} clean=True, not the PR head {HEAD}"])

    # An untracked source appeared, or the head moved, while X1 ran.
    untracked = dict(heads, x1=[(HEAD, True), (HEAD, False)])
    expect("untracked file fails", activation.judge_all(real, untracked, HEAD),
           [f"x1: W5: after the run on {HEAD} clean=False, not the PR head {HEAD}"])
    moved = dict(heads, x1=[(HEAD, True), ("e" * 40, True)])
    expect("head move during a run fails", activation.judge_all(real, moved, HEAD),
           [f"x1: W5: after the run on {'e' * 40} clean=True, not the PR head {HEAD}"])

    # The output reads as a pass but cargo did not exit 0, or did not end in time.
    output, logs = fixture("x1")
    expect("non-zero exit fails", judged("x1", output, logs, 101),
           ["W2: cargo test exited 101"])
    expect("timeout fails", judged("x1", output, logs, None),
           [f"W2: the run did not end within {activation.RUN_TIMEOUT}s"])

    # A second, contrary effect line.
    output, logs = fixture("x2")
    output = output.replace("X_EFFECT name=x2", "X_EFFECT name=x2 row=retired hold=released "
                            "turn=settled\nX_EFFECT name=x2", 1)
    expect("duplicate effect fails", judged("x2", output, logs), ["W4: 2 X_EFFECT lines, expected 1"])

    # The control pane kept on the target server was swapped for another with the same count.
    output, logs = fixture("x1")
    after = [pane("w2:p9", "w2", 702, 703)]
    output = replace(output, f"X_SNAPSHOT server=x1-s1 phase=after panes="
                     f"{compact([pane('w2:p1', 'w2', 702, 703)])}",
                     f"X_SNAPSHOT server=x1-s1 phase=after panes={compact(after)}")
    expect("control pane swap fails", judged("x1", output, logs),
           ["W4: x1-s1's after snapshot is not its before one less the target"])

    # The target server also took a group close.
    output, logs = fixture("x1")
    logs[log_path("x1-s1")] = server_log("x1-s1", "600", [
        ("x-setup-1", "workspace.create", "ok"), ("adk-7", "pane.close", "ok"),
        ("adk-8", "group.close", "ok")])
    expect("group close fails", judged("x1", output, logs),
           ["W3: wider closes reached the server: ['group.close']"])

    # X1 without its O line, or with the tail posted twice.
    output, logs = fixture("x1")
    without = "\n".join(line for line in output.splitlines() if not line.startswith("X_O "))
    expect("missing O line fails", judged("x1", without, logs), ["W4: 0 X_O lines, expected 1"])
    twice = replace(output, "tail_posts=1", "tail_posts=2")
    expect("tail posted twice fails", judged("x1", twice, logs),
           ["W4: X_O `" + activation.O_EVIDENCE["x1"].replace("tail_posts=1", "tail_posts=2")
            + "`, expected `" + activation.O_EVIDENCE["x1"] + "`"])

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
