#!/usr/bin/env python3
"""Run and judge the P11 M2 activation gates X1 and X2 against real Herdr 0.9.3 servers.

Each test runs alone (`--ignored --exact`) with a scratch copy of the server binary, a run root
under the scratch directory and a pid ledger. Both must pass on one clean head:

  W1  the test really ran: `running 1 test` and its own line, not `ignored`
  W2  `test result: ok. 1 passed; 0 failed; 0 ignored`, and cargo exited 0 within its time
  W3  its one write went to a process other than the test, whose binary is the scratch copy with
      the pinned hash; that process is the server the test started on that socket (pid and
      start), and that server's own log took the request id once as a pane close (N-2)
  W4  one terminate result log naming the bound execution, with the expected result; one
      snapshot per server and phase, the target's after one exactly its before one less the
      target pane for X1 and unchanged for X2, every other server unchanged; one effect line;
      for X1 one O line: the sealed tail unread at the termination, the service writing none
      of O's state, then posted and recorded once; a restart posts its own new output once and
      reposts nothing
  W5  each run starts and ends on the PR head with no change, untracked files included

The run also requires each server offline (no curl under it, its update check unable to spawn
curl), every ledger pid of the run gone afterwards, and the production sockets unchanged.
Usage: pg_throwaway.sh python3 scripts/test_p11_m2_activation.py --scratch DIR --pids FILE
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

PINNED_SHA256 = "5173a3e0ae42d5d1ab7ebfa5d5e6329f7c3d23f8e1a3677c7ce3231da2884157"
TESTS = {
    "x1": (
        "services::discord::tui_prompt_relay::relay_e2e::herdr_terminate_o_e2e::real",
        "a_p11_m2_last_pane_close_and_exact_retire",
        "retired",
        "ok",
    ),
    "x2": (
        "services::session_host::herdr_terminate_real_tests",
        "a_p11_m2_confirmation_required_has_no_escalation",
        "confirmation_required",
        "error:confirmation_required",
    ),
}
EFFECT = {"x1": "row=retired hold=released turn=settled", "x2": "row=bound hold=kept turn=active"}
O_EVIDENCE = {
    "x1": "tail=unread service_o_writes=0 service_posts=0 tail_posts=1 tail_ledger_posted=1 "
          "restart_output=1 restart_reposts=0 halts=0",
}
WIDER_CLOSES = ("workspace.close", "tab.close", "group.close", "worktree_group.close",
                "server.stop")
RUN_TIMEOUT = 1800


def fields(line: str) -> dict[str, str]:
    """`key=value` pairs of one `X_` line; a value runs to the next ` key=`."""
    body = line.split(" ", 1)[1] if " " in line else ""
    keys = list(re.finditer(r"(?:^| )([a-z_0-9]+)=", body))
    found = {}
    for index, match in enumerate(keys):
        end = keys[index + 1].start() if index + 1 < len(keys) else len(body)
        found[match.group(1)] = body[match.end():end]
    return found


def lines_of(output: str, tag: str) -> list[dict[str, str]]:
    """Each `tag` line; the first may follow the harness's `test ... ` on the same line."""
    found = []
    for line in output.splitlines():
        at = line.find(tag + " ")
        if at == 0 or (at > 0 and line[at - 1] == " "):
            found.append(fields(line[at:]))
    return found


def log_requests(log: str) -> list[tuple[str, str]]:
    """`(request_id, method)` of each API request the server log took."""
    found = []
    for line in log.splitlines():
        if 'event="api.request.start"' in line:
            rid = re.search(r' request_id="([^"]*)"', line)
            method = re.search(r' method="([^"]*)"', line)
            found.append((rid.group(1) if rid else "", method.group(1) if method else ""))
    return found


def judge(name: str, output: str, *, bin_path: str, read_log, returncode: int | None) -> list[str]:
    """Why one test's run is not a PASS; empty when it is. `returncode` is None on a timeout."""
    module, test, result, write_outcome = TESTS[name]
    failures = []
    # W1, W2
    if "running 1 test" not in output.splitlines():
        failures.append("W1: no `running 1 test`")
    # With --nocapture the test's own lines come between its name and `ok`.
    started = re.search(rf"^test {re.escape(module)}::{test} \.\.\. (.*)$", output, re.M)
    if not started or started.group(1).strip() == "ignored":
        failures.append(f"W1: `{test}` did not run")
    if not re.search(r"^test result: ok\. 1 passed; 0 failed; 0 ignored;", output, re.M):
        failures.append("W2: not exactly one passed test")
    if returncode is None:
        failures.append(f"W2: the run did not end within {RUN_TIMEOUT}s")
    elif returncode != 0:
        failures.append(f"W2: cargo test exited {returncode}")
    # W3
    tests = lines_of(output, "X_TEST")
    test_pid = tests[0].get("pid") if tests else None
    if test_pid is None:
        failures.append("W3: the test did not name its own pid")
    writes = lines_of(output, "X_WRITE")
    if len(writes) != 1:
        failures.append(f"W3: {len(writes)} write witnesses, expected 1")
        return failures
    write = writes[0]
    if write.get("pid") == test_pid:
        failures.append("W3: the write was answered inside the test process")
    if write.get("outcome") != write_outcome:
        failures.append(f"W3: write outcome {write.get('outcome')}, expected {write_outcome}")
    exes = [exe for exe in lines_of(output, "X_EXE") if exe.get("pid") == write.get("pid")]
    if len(exes) != 1:
        failures.append("W3: no binary read for the write's process")
    else:
        exe = exes[0]
        if os.path.realpath(exe.get("path", "")) != os.path.realpath(bin_path):
            failures.append(f"W3: the write's process runs {exe.get('path')}, not {bin_path}")
        if exe.get("sha256") != PINNED_SHA256:
            failures.append(f"W3: binary sha256 {exe.get('sha256')} is not the pinned 0.9.3")
    failures += correlate(write, lines_of(output, "X_SERVER"), read_log)
    # W4
    executions = lines_of(output, "X_EXECUTION")
    results = lines_of(output, "X_RESULT_LOG")
    if len(executions) != 1:
        failures.append(f"W4: {len(executions)} bound executions, expected 1")
        return failures
    execution = executions[0]
    if len(results) != 1:
        failures.append(f"W4: {len(results)} terminate result logs, expected 1")
    else:
        logged = results[0]
        if logged.get("result") != result:
            failures.append(f"W4: result {logged.get('result')}, expected {result}")
        if (logged.get("execution_nonce"), logged.get("session_key")) != (
            execution.get("nonce"),
            execution.get("session_key"),
        ):
            failures.append("W4: the result log names another execution")
    failures += snapshots(name, output, execution)
    failures += exactly_one(name, output, "X_EFFECT", EFFECT[name])
    if name in O_EVIDENCE:
        failures += exactly_one(name, output, "X_O", O_EVIDENCE[name])
    failures += isolation(output)
    return failures


def exactly_one(name: str, output: str, tag: str, expected: str) -> list[str]:
    """The run's one `tag` line, which reads `expected` after its name."""
    found = [line for line in lines_of(output, tag) if line.get("name") == name]
    if len(found) != 1:
        return [f"W4: {len(found)} {tag} lines, expected 1"]
    text = " ".join(f"{key}={value}" for key, value in found[0].items() if key != "name")
    return [] if text == expected else [f"W4: {tag} `{text}`, expected `{expected}`"]


def correlate(write: dict, servers: list[dict], read_log) -> list[str]:
    """N-2: the write's connection belongs to the server the test started on that socket, and
    that server's own log took the request once, as a pane close and nothing wider."""
    targets = [server for server in servers if server.get("socket") == write.get("socket")]
    if len(targets) != 1:
        return ["W3: no started server owns the write's socket"]
    server = targets[0]
    failures = []
    if (write.get("pid"), write.get("start")) != (server.get("pid"), server.get("start")):
        failures.append("W3: the write's process is not the started server")
    log = read_log(server.get("log", ""))
    if f"api server listening path={server.get('socket')}" not in log:
        failures.append("W3: the server log does not own the socket")
    if not re.search(rf"herdr exiting .* pid={re.escape(server.get('pid', ''))}\b", log):
        failures.append("W3: the server log does not name the server's pid")
    requests = log_requests(log)
    taken = [method for rid, method in requests if rid == write.get("id")]
    if taken != ["pane.close"]:
        failures.append(f"W3: the server log took request {write.get('id')} as {taken}")
    closes = [rid for rid, method in requests if method == "pane.close"]
    if len(closes) != 1:
        failures.append(f"W3: the server log took {len(closes)} pane closes")
    wider = [method for _, method in requests if method in WIDER_CLOSES]
    if wider:
        failures.append(f"W3: wider closes reached the server: {wider}")
    return failures


def snapshots(name: str, output: str, execution: dict) -> list[str]:
    """One before and one after snapshot per started server; only X1's target pane went."""
    taken: dict[tuple[str, str], list] = {}
    for line in lines_of(output, "X_SNAPSHOT"):
        taken.setdefault((line.get("server", ""), line.get("phase", "")), []).append(
            json.loads(line.get("panes", "null")))
    failures = []
    for server in [line.get("name", "") for line in lines_of(output, "X_SERVER")]:
        phases = {phase: taken.get((server, phase), []) for phase in ("before", "after")}
        if any(len(found) != 1 for found in phases.values()):
            failures.append(f"W4: {server} has {len(phases['before'])} before and "
                            f"{len(phases['after'])} after snapshots, expected 1 each")
            continue
        before, after = phases["before"][0], phases["after"][0]
        expected = before
        if server == execution.get("server"):
            pane = execution.get("pane")
            if pane not in [entry.get("pane_id") for entry in before]:
                failures.append("W4: the target pane is not in the before snapshot")
                continue
            if name == "x1":
                expected = [entry for entry in before if entry.get("pane_id") != pane]
        if after != expected:
            failures.append(f"W4: {server}'s after snapshot is not its before one"
                            + (" less the target" if expected != before else ""))
    return failures


def isolation(output: str) -> list[str]:
    failures = []
    servers = lines_of(output, "X_SERVER")
    curls = {line.get("server"): line for line in lines_of(output, "X_CURL")}
    stopped = {line.get("name"): line for line in lines_of(output, "X_STOPPED")}
    for server in servers:
        name = server.get("name")
        curl = curls.get(name)
        if not curl or curl.get("children") != "0" or curl.get("update_check") != "spawn_failed":
            failures.append(f"offline: {name} not shown offline")
        stop = stopped.get(name)
        if not stop or stop.get("forced") != "false" or stop.get("hosted_left") != "[]":
            failures.append(f"cleanup: {name} not stopped cleanly")
    return failures


def judge_all(verdicts: dict[str, list[str]],
              heads: dict[str, list[tuple[str, bool]]], pr_head: str) -> list[str]:
    """W5 and the pair: both gates ran with no failure, each starting and ending on the clean
    PR head."""
    failures = []
    for name in TESTS:
        if name not in verdicts:
            failures.append(f"{name}: not run")
            continue
        failures += [f"{name}: {why}" for why in verdicts[name]]
        states = heads.get(name, [])
        if len(states) != 2:
            failures.append(f"{name}: W5: the head was not read before and after the run")
        for when, (head, clean) in zip(("before", "after"), states):
            if (head, clean) != (pr_head, True):
                failures.append(f"{name}: W5: {when} the run on {head} clean={clean}, "
                                f"not the PR head {pr_head}")
    return failures


def sha256(path: str) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def git(worktree: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(worktree), *args], capture_output=True, text=True, check=True
    ).stdout.strip()


def head_state(worktree: Path) -> tuple[str, bool]:
    clean = git(worktree, "status", "--porcelain", "--untracked-files=all") == ""
    return git(worktree, "rev-parse", "HEAD"), clean


def production_sockets() -> list[str]:
    """Read-only stat lines of the production Herdr sockets; nothing connects to them."""
    rows = []
    for directory in (Path.home() / ".adk/herdr", Path.home() / ".config/herdr"):
        if directory.is_dir():
            for path in sorted(directory.iterdir()):
                if ".sock" in path.name:
                    stat = path.lstat()
                    rows.append(f"{path} ino={stat.st_ino} mtime={stat.st_mtime_ns} "
                                f"ctime={stat.st_ctime_ns}")
    return rows


def ledger_pids(ledger: Path, offset: int) -> list[int]:
    """Pids the run recorded, from the ledger bytes it appended."""
    text = ledger.read_bytes()[offset:].decode(errors="replace") if ledger.exists() else ""
    pids = []
    for line in text.splitlines():
        parts = line.split()
        if len(parts) > 1 and parts[1].startswith("x12-") and parts[0].isdigit():
            pids.append(int(parts[0]))
    return sorted(set(pids))


def running(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--scratch", required=True, type=Path)
    parser.add_argument("--pids", required=True, type=Path)
    parser.add_argument("--herdr-src", default=str(Path.home() / ".local/bin/herdr"))
    parser.add_argument("--worktree", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--only", choices=sorted(TESTS), action="append")
    parser.add_argument("--pr-head", help="the PR head both runs must be on (default: HEAD)")
    args = parser.parse_args()
    if not os.environ.get("POSTGRES_TEST_DATABASE_URL_BASE"):
        print("P11_M2_ACTIVATION FAIL: no throwaway PostgreSQL (run under pg_throwaway.sh)")
        return 1
    stamp = datetime.datetime.now().strftime("%Y%m%dT%H%M%S")
    run_root = args.scratch / f"r{stamp}"
    out = run_root / "logs"
    out.mkdir(parents=True)
    report = []
    say = lambda line: (print(line), report.append(line))
    head, clean = head_state(args.worktree)
    say(f"head={head} clean={clean} worktree={args.worktree}")
    binary = args.scratch / "bin/herdr"
    binary.parent.mkdir(parents=True, exist_ok=True)
    if not binary.exists() or sha256(str(binary)) != sha256(args.herdr_src):
        shutil.copy2(args.herdr_src, binary)
    say(f"herdr source={args.herdr_src} sha256={sha256(args.herdr_src)}")
    say(f"herdr copy={binary} sha256={sha256(str(binary))} pinned={PINNED_SHA256}")
    if sha256(str(binary)) != PINNED_SHA256:
        say("P11_M2_ACTIVATION FAIL: the scratch binary is not the pinned 0.9.3")
        return 1
    before = production_sockets()
    say("production sockets (read only): " + json.dumps(before))
    production = {row.split(" ")[0] for row in before}
    for name in ("x1-s1", "x1-s2", "x2-s2"):
        socket = run_root / name / "run/h.sock"
        assert str(socket) not in production and len(str(socket)) < 104, socket
        say(f"isolated socket {socket} (not a production path)")
    offset = args.pids.stat().st_size if args.pids.exists() else 0
    env = dict(os.environ, ADK_TEST_HERDR_BIN=str(binary), ADK_TEST_HERDR_ROOT=str(run_root),
               ADK_TEST_HERDR_PIDS=str(args.pids))
    verdicts, heads = {}, {}
    pr_head = args.pr_head or head
    for name in args.only or sorted(TESTS):
        heads[name] = [head_state(args.worktree)]
        module, test = TESTS[name][:2]
        command = ["cargo", "test", "--lib", f"{module}::{test}", "--", "--ignored", "--exact",
                   "--nocapture", "--test-threads=1"]
        try:
            run = subprocess.run(command, cwd=args.worktree, env=env, capture_output=True,
                                 text=True, timeout=RUN_TIMEOUT)
            output, returncode = run.stdout + run.stderr, run.returncode
        except subprocess.TimeoutExpired as expired:
            stdout, stderr = expired.stdout or b"", expired.stderr or b""
            output = (stdout if isinstance(stdout, str) else stdout.decode(errors="replace")) + (
                stderr if isinstance(stderr, str) else stderr.decode(errors="replace"))
            output, returncode = output + "\nTIMEOUT", None
        heads[name].append(head_state(args.worktree))
        (out / f"{name}.log").write_text(output)
        verdicts[name] = judge(name, output, bin_path=str(binary), returncode=returncode,
                               read_log=lambda path: Path(path).read_text() if path else "")
        say(f"{name.upper()} {'PASS' if not verdicts[name] else 'FAIL'} head={head} "
            f"log={out / (name + '.log')} " + "; ".join(verdicts[name]))
    failures = judge_all(verdicts, heads, pr_head)
    left = [pid for pid in ledger_pids(args.pids, offset) if running(pid)]
    if left:
        failures.append(f"cleanup: recorded pids still running {left}")
        for pid in left:
            os.kill(pid, 15)
            with args.pids.open("a") as ledger:
                ledger.write(f"{pid} killed {datetime.datetime.now().isoformat()} "
                             "(test_p11_m2_activation: leftover recorded pid)\n")
    if production_sockets() != before:
        failures.append("isolation: production sockets changed")
    if head_state(args.worktree) != (pr_head, True):
        failures.append(f"W5: the worktree left the clean PR head {pr_head}")
    verdict = "PASS" if not failures else "FAIL"
    say(f"P11_M2_ACTIVATION {verdict} head={head} " + "; ".join(failures))
    (out / "summary.txt").write_text("\n".join(report) + "\n")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
