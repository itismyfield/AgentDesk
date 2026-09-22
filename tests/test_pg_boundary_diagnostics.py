"""Executable boundary-diagnostics contracts and regressions for the N1 review findings."""
from __future__ import annotations

import contextlib
import copy
import io
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest import mock

import yaml
from tests.test_nightly_repair import ROOT, NIGHTLY, PG_STEP, PG_CARGO, FILTER_SOURCE
from tests.test_nightly_repair import load_module, step_of, workflow_triggers

HELPER = ROOT / "scripts/ci/pg-resource-diagnostics.py"
M = load_module("pg_boundary_diagnostics", HELPER)
GATE = "github.event_name == 'workflow_dispatch' && inputs.resource_diagnostics == true"
VERIFY = "Verify PostgreSQL resource diagnostics"
ARGV = "test --all-targets -- _pg pg_ postgres --nocapture --test-threads=1"
MEM = dict(zip(M.MEMORY_KEYS, (16000, 9000, 4000, 3900)))
META = {"run_id": "9001", "run_attempt": "1", "commit_sha": "c" * 40, "container": "agentdesk-postgres"}
IDENTITY = ["--run-id", "9001", "--run-attempt", "1", "--commit-sha", "c" * 40]
STATE = {"Status": "running", "ExitCode": 0, "OOMKilled": False, "Error": "", "scope": "postgres-container-only"}


def valid_records(status=0):
    records = []
    for index, phase in enumerate(("before", "exit"), 1):
        disk = {"requested_path": "/w/target", "measured_path": "/w", "total_mib": 20000, "free_mib": 10000}
        records.append({"kind": "snapshot", "coverage": "boundary-only", "phase": phase,
                        "sequence": index, "owner_status": None if index == 1 else status,
                        "metadata": copy.deepcopy(META), "memory": dict(MEM),
                        "filesystems": {"workspace_target": dict(disk), "runner_temp": dict(disk)},
                        "pg_container": {"name": META["container"], "mem_usage": "64MiB / 15.6GiB;0.40%"},
                        "pg_container_state": dict(STATE) if index == 2 and status not in (0, 130, 143) else None,
                        "monotonic": float(index), "utc": f"2026-09-22T00:00:0{index}+00:00"})
    return records


def scalar():
    return step_of(yaml.safe_load(NIGHTLY.read_text())["jobs"], "postgres_full", PG_STEP)["run"]


def wiring_problems(text):
    document = yaml.safe_load(text)
    jobs, problems = document["jobs"], []
    def need(condition, message):
        if not condition:
            problems.append(message)
    declared = ((workflow_triggers(document).get("workflow_dispatch") or {}).get("inputs") or {}).get("resource_diagnostics", {})
    need(declared.get("type") == "boolean", "boolean input")
    need(declared.get("default") is False, "default off")
    need(declared.get("required") is False, "optional input")
    pg = step_of(jobs, "postgres_full", PG_STEP) or {}
    check = step_of(jobs, "postgres_full", VERIFY) or {}
    need(pg.get("id") == "pg_cargo_test", "output producer")
    need((pg.get("env") or {}).get("PG_DIAG_ENABLED") == "${{ " + GATE + " }}", "exact enable gate")
    need(not pg.get("continue-on-error"), "non-advisory cargo")
    need(check.get("if") == "always() && " + GATE, "exact verification gate")
    need(check.get("timeout-minutes") == 1, "bounded verification")
    need(check.get("shell") == "bash", "verification shell")
    need(not check.get("continue-on-error"), "non-advisory verification")
    need((check.get("env") or {}).get("PG_DIAG_DIR") == "${{ steps.pg_cargo_test.outputs.diagnostics-dir }}", "output consumer")
    expected = ["python3", "-B", "scripts/ci/pg-resource-diagnostics.py", "verify", "--directory", "${PG_DIAG_DIR:-}",
                "--run-id", "${GITHUB_RUN_ID:-}", "--run-attempt", "${GITHUB_RUN_ATTEMPT:-}", "--commit-sha", "${GITHUB_SHA:-}"]
    need(shlex.split(check.get("run", "").replace("\\\n", "")) == expected, "executable verifier")
    names = [s.get("name") for s in jobs["postgres_full"]["steps"]]
    need(VERIFY in names and names.index(PG_STEP) < names.index(VERIFY) < names.index("Stop PostgreSQL service"), "verification order")
    return problems


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def await_true(predicate, seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("bounded fixture wait expired")


def read_lines(path):
    return path.read_text().splitlines() if path.is_file() else []


@contextlib.contextmanager
def launch(script=None, *, enabled=True, mode="ok", broken=None, container=""):
    with tempfile.TemporaryDirectory() as raw:
        d = Path(raw)
        (d / "bin").mkdir()
        (d / "temp").mkdir()
        (d / "not-directory").touch()
        bodies = {
            "cargo": '#!/bin/bash\nprintf "%s\\n" "$*" >> "$CASE/argv"\ntouch "$CASE/running"\n'
                     'case "$MODE" in\n fail) exit 101;;\n term) kill -TERM $$;;\n int) kill -INT $$;;\n'
                     ' hang) while [ ! -f "$CASE/release" ]; do sleep 0.05; done;;\nesac\n',
            "docker": '#!/bin/bash\nprintf "%s\\n" "$*" >> "$CASE/docker"\n'
                      'if [ "$BROKEN" = docker ]; then sleep 30; exit 1; fi\n'
                      'if [ "$BROKEN" = state ] && [ "$1" = inspect ]; then echo \'{"Status":"not-a-container-state","ExitCode":0,"OOMKilled":false,"Error":""}\'; exit 0; fi\n'
                      'if [ "$1" = inspect ]; then echo \'{"Status":"running","ExitCode":0,"OOMKilled":false,"Error":""}\'; '
                      'else echo "64MiB / 15.6GiB;0.40%"; fi\n',
            "python3": f'#!{sys.executable}\nimport importlib.util,json,os,sys\nfrom pathlib import Path\n'
                       'args=[a for a in sys.argv[1:] if a not in ("-u","-B")]\n'
                       'with (Path(os.environ["CASE"])/"python").open("a") as f: f.write(str(os.getpid())+" "+" ".join(args)+"\\n")\n'
                       'if os.environ.get("BROKEN") == "python": sys.exit(7)\n'
                       'spec=importlib.util.spec_from_file_location("fixture_snapshot",args.pop(0))\n'
                       'm=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)\n'
                       f'm.memory=lambda: {MEM!r}\nsys.exit(m.main(args))\n'}
        for name, body in bodies.items():
            path = d / "bin" / name
            path.write_text(body)
            path.chmod(0o755)
        env = {**os.environ, "PATH": str(d / "bin") + os.pathsep + os.environ["PATH"], "CASE": str(d),
               "MODE": mode, "BROKEN": broken or "", "PG_DIAG_ENABLED": "true" if enabled else "false",
               "GITHUB_OUTPUT": str(d if broken == "output" else d / "output"),
               "RUNNER_TEMP": str(d / ("not-directory" if broken == "temp" else "temp")),
               "GITHUB_WORKSPACE": str(ROOT), "GITHUB_RUN_ID": "9001", "GITHUB_RUN_ATTEMPT": "1",
               "GITHUB_SHA": "c" * 40, "POSTGRES_SERVICE_CONTAINER": container}
        (d / "step.sh").write_text(script or scalar())
        with (d / "stdout").open("w") as out, (d / "stderr").open("w") as err:
            p = subprocess.Popen(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", str(d / "step.sh")],
                                 cwd=ROOT, env=env, stdout=out, stderr=err, start_new_session=True)
            try:
                yield p, d
            finally:
                if p.poll() is None:
                    os.killpg(p.pid, signal.SIGTERM)
                    try:
                        p.wait(timeout=6)
                    except subprocess.TimeoutExpired:
                        os.killpg(p.pid, signal.SIGKILL)
                        p.wait(timeout=3)


def outcome(p, d):
    p.wait(timeout=15)
    paths = [Path(line.split("=", 1)[1]) for line in read_lines(d / "output") if line.startswith("diagnostics-dir=")]
    records = [json.loads(line) for line in read_lines(paths[0] / "samples.jsonl")] if paths else []
    return SimpleNamespace(status=p.returncode, directory=paths[0] if paths else None,
                           argv=read_lines(d / "argv"), records=records,
                           python=read_lines(d / "python"), docker=read_lines(d / "docker"), stderr=(d / "stderr").read_text())


def runtime_problems(o, expected, enabled=True):
    problems = []
    if o.status != expected:
        problems.append("status changed")
    if o.argv != [ARGV]:
        problems.append("Cargo argv/count changed")
    if enabled and [r["phase"] for r in o.records] != ["before", "exit"]:
        problems.append("missing boundaries")
    if not enabled and (o.directory or o.records or o.python or o.docker):
        problems.append("disabled resource work")
    return problems


class BoundaryWiringTests(unittest.TestCase):
    def test_actual_wiring_and_structural_mutants(self):
        text = NIGHTLY.read_text()
        self.assertEqual(wiring_problems(text), [])
        changes = (("default: false", "default: true"), ("type: boolean", "type: string"),
                   ("required: false", "required: true"), (GATE, GATE.replace(" && ", " || ")),
                   ("${{ " + GATE + " }}", '"true"'), ("always() && " + GATE, "always()"),
                   ("timeout-minutes: 1\n", "timeout-minutes: 1\n        continue-on-error: true\n"),
                   ("id: pg_cargo_test", "continue-on-error: true\n        id: pg_cargo_test"),
                   ("steps.pg_cargo_test.outputs", "steps.wrong.outputs"),
                   ("python3 -B scripts/ci/pg-resource-diagnostics.py verify", "echo skipped"),
                   ('--directory "${PG_DIAG_DIR:-}"', '--directory "${{ steps.pg_cargo_test.outputs.diagnostics-dir }}"'))
        for old, new in changes:
            with self.subTest(mutant=old):
                mutated = text.replace(old, new)
                self.assertNotEqual(mutated, text)
                self.assertIsInstance(yaml.safe_load(mutated), dict)
                self.assertNotEqual(wiring_problems(mutated), [])
        self.assertIn('"$PYTHON" -m unittest tests.test_pg_boundary_diagnostics', (ROOT / "scripts/ci-script-checks.sh").read_text())


class BoundaryRuntimeTests(unittest.TestCase):
    def test_enabled_run_keeps_one_exact_cargo_invocation(self):
        with launch() as (p, d):
            o = outcome(p, d)
            self.assertEqual(runtime_problems(o, 0), [])
            self.assertEqual(o.records[-1]["phase"], "exit")
            self.assertEqual(o.records[-1]["owner_status"], 0)
            self.assertEqual([r["sequence"] for r in o.records], [1, 2])

    def test_cargo_status_is_preserved(self):
        for mode, status in (("ok", 0), ("fail", 101), ("int", 130), ("term", 143)):
            with self.subTest(mode=mode), launch(mode=mode) as (p, d):
                o = outcome(p, d)
                self.assertEqual(runtime_problems(o, status), [])
                self.assertEqual(o.records[-1]["owner_status"], status)
                self.assertEqual(len(o.python), 2)
                self.assertEqual(BoundaryVerifierTests().check(o.records), 0)
                self.assertEqual(sum(line.startswith("inspect ") for line in o.docker), int(status == 101))

    def test_disabled_run_starts_no_sampler_or_probe(self):
        with launch(enabled=False) as (p, d):
            o = outcome(p, d)
            self.assertEqual(runtime_problems(o, 0, False), [])
            self.assertEqual(o.records, [])
            self.assertIsNone(o.directory)
            self.assertEqual(o.docker, [])
            self.assertEqual(o.python, [])

    def test_diagnostic_failures_do_not_suppress_cargo(self):
        for broken in ("output", "temp", "python"):
            with self.subTest(broken=broken), launch(mode="fail", broken=broken) as (p, d):
                o = outcome(p, d)
                self.assertEqual(o.status, 101)
                self.assertEqual(o.argv, [ARGV])
                self.assertEqual(o.records, [])
                self.assertIn("unavailable", o.stderr)

    def test_unsupported_container_state_only_fails_verification(self):
        with launch(mode="fail", broken="state") as (p, d):
            o = outcome(p, d)
            self.assertEqual(runtime_problems(o, 101), [])
            self.assertEqual(o.records[-1]["pg_container_state"]["Status"], "not-a-container-state")
            self.assertEqual(BoundaryVerifierTests().check(o.records), 1)

    def test_runtime_mutants_are_rejected(self):
        base = scalar()
        changes = (("exit \"$status\"", "exit 0", "fail", 101, True),
                   ('local status=$?', 'true\nlocal status=$?', "fail", 101, True),
                   ('exit "$status"', 'exit "${snapshot_status:-0}"', "fail", 101, True),
                   (FILTER_SOURCE, 'exit 0\n' + FILTER_SOURCE, "ok", 0, True),
                   (PG_CARGO, "true", "ok", 0, True), (PG_CARGO, PG_CARGO + '\n' + PG_CARGO, "ok", 0, True),
                   (PG_CARGO, 'if false; then\n' + PG_CARGO + '\nfi', "ok", 0, True),
                   ("--all-targets", "--lib", "ok", 0, True),
                   ('if [ "${PG_DIAG_ENABLED:-false}" = "true" ]; then', 'if true; then', "ok", 0, False))
        for old, new, mode, status, enabled in changes:
            with self.subTest(mutant=old), launch(base.replace(old, new), mode=mode, enabled=enabled) as (p, d):
                self.assertNotEqual(base.replace(old, new), base)
                self.assertNotEqual(runtime_problems(outcome(p, d), status, enabled), [])

    def test_samples_are_visible_while_cargo_is_still_running(self):
        with launch(mode="hang") as (p, d):
            await_true(lambda: (d / "running").exists())
            streamed = [json.loads(line) for line in read_lines(d / "stdout") if line.startswith("{")]
            self.assertTrue(streamed)
            self.assertEqual(streamed[0]["phase"], "before")
            self.assertTrue((d / "running").is_file())
            self.assertIsNone(p.poll())
            (d / "release").touch()
            self.assertEqual(outcome(p, d).status, 0)
            self.assertTrue(all(not alive(int(row.split()[0])) for row in read_lines(d / "python")))

    def test_group_signals_preserve_status_and_leave_no_sampler(self):
        for received, status in ((signal.SIGINT, 130), (signal.SIGTERM, 143)):
            with self.subTest(signal=received), launch(mode="hang") as (p, d):
                sentinel = subprocess.Popen(["sleep", "20"])
                try:
                    await_true(lambda: (d / "running").exists())
                    self.assertIn('"phase": "before"', (d / "stdout").read_text())
                    self.assertIsNone(p.poll())
                    first = read_lines(d / "python")
                    self.assertEqual(len(first), 1)
                    self.assertFalse(alive(int(first[0].split()[0])), "no persistent snapshot child during Cargo")
                    os.killpg(p.pid, received)
                    o = outcome(p, d)
                    self.assertEqual(runtime_problems(o, status), [])
                    self.assertEqual(o.records[-1]["owner_status"], status)
                    self.assertIsNone(sentinel.poll())
                finally:
                    sentinel.terminate()
                    sentinel.wait(timeout=3)

    def test_owner_sigkill_leaves_incomplete_evidence(self):
        with launch(mode="hang") as (p, d):
            await_true(lambda: (d / "running").exists())
            directory = read_lines(d / "output")[0].split("=", 1)[1]
            p.kill()
            p.wait(timeout=3)
            try:
                check = subprocess.run([sys.executable, "-B", str(HELPER), "verify", "--directory", directory, *IDENTITY],
                                       capture_output=True, text=True, timeout=5)
                self.assertEqual(check.returncode, 1)
                self.assertEqual(len(read_lines(Path(directory, "samples.jsonl"))), 1)
            finally:
                os.killpg(p.pid, signal.SIGKILL)

    def test_runtime_probe_hangs_are_bounded_without_changing_cargo(self):
        start = time.monotonic()
        with launch(mode="fail", broken="docker") as (p, d):
            o = outcome(p, d)
            self.assertEqual(runtime_problems(o, 101), [])
            self.assertIn("error", o.records[0]["pg_container"])
            self.assertIn("error", o.records[-1]["pg_container_state"])
            self.assertEqual(BoundaryVerifierTests().check(o.records), 1)
        self.assertLess(time.monotonic() - start, 12)

    def test_container_override_and_empty_fallback(self):
        for override in ("", "custom-pg"):
            with self.subTest(container=override), launch(mode="fail", container=override) as (p, d):
                o = outcome(p, d)
                self.assertEqual(o.status, 101)
                self.assertTrue(all(line.endswith(override or "agentdesk-postgres") for line in o.docker))
                self.assertEqual(sum(line.startswith("inspect ") for line in o.docker), 1)
                self.assertTrue(all("--no-stream --format" in line for line in o.docker if line.startswith("stats ")))
                self.assertNotIn("{{json .State}}", "\n".join(o.docker))


class BoundaryVerifierTests(unittest.TestCase):
    def check(self, records=None, *, raw=None, absent=False):
        with tempfile.TemporaryDirectory() as directory:
            if not absent:
                Path(directory, "samples.jsonl").write_text(raw if raw is not None else ''.join(json.dumps(r)+'\n' for r in records))
            target = str(Path(directory, "absent")) if absent == "directory" else directory
            p = subprocess.run([sys.executable, "-B", str(HELPER), "verify", "--directory", target, *IDENTITY],
                               capture_output=True, text=True, timeout=5)
            self.assertNotIn("Traceback", p.stderr)
            return p.returncode

    def test_valid_boundaries_and_failure_postmortem(self):
        for status in (0, 101, 130, 143):
            self.assertEqual(self.check(valid_records(status)), 0)
        paths = (("/w/target", "/w"), ("/w/target", "/w/target"), ("./target", "."), (".", "."), ("/w/a/b/c", "/w"), ("/w", "/w"), ("/", "/"))
        for state, (requested, measured) in zip(M.SUPPORTED_STATES, paths):
            rows = valid_records(101)
            rows[-1]["pg_container_state"].update(Status=state, Error="OCI runtime error")
            for record in rows:
                record["filesystems"]["runner_temp"].update(requested_path=requested, measured_path=measured)
            self.assertEqual(self.check(rows), 0, (state, requested, measured))

    def test_missing_partial_stale_invalid_and_error_evidence(self):
        self.assertEqual(self.check(absent=True), 1)
        self.assertEqual(self.check(absent="directory"), 1)
        self.assertEqual(self.check(raw="{not json}\n"), 1)
        self.assertEqual(self.check(raw="x" * (M.FILE_CAP + 1)), 1)
        for records in ([], valid_records()[:1], valid_records()[1:], valid_records()[::-1], valid_records() * 2):
            self.assertEqual(self.check(records), 1)
        changes = [((1, "owner_status"), None), ((1, "owner_status"), -1), ((1, "owner_status"), True),
                   ((1, "phase"), "deadline"), ((1, "sequence"), 9), ((1, "kind"), "error"), ((1, "error"), "write-failed"),
                   ((1, "monotonic"), float("nan")), ((1, "monotonic"), -1), ((1, "monotonic"), True),
                   ((1, "utc"), "invalid"), ((1, "utc"), "2026-09-22T00:00:02"),
                   ((1, "monotonic"), 1799), ((1, "pg_container", "error"), "timeout"),
                   ((1, "pg_container", "mem_usage"), ""), ((1, "pg_container", "name"), "wrong"),
                   ((1, "filesystems"), {}), ((1, "memory", "error"), "unreadable"),
                   ((1, "pg_container_state"), {"error": "unexpected"})]
        for index in (0, 1):
            for key in ("run_id", "run_attempt", "commit_sha"):
                changes.append(((index, "metadata", key), "stale"))
            for key in M.MEMORY_KEYS:
                for invalid in (None, True, -1, "9000"):
                    changes.append(((index, "memory", key), invalid))
            for label in ("workspace_target", "runner_temp"):
                for key in ("requested_path", "measured_path", "free_mib", "total_mib"):
                    changes.append(((index, "filesystems", label, key), None))
                for key in ("requested_path", "measured_path"):
                    changes.append(((index, "filesystems", label, key), "/unrelated"))
        for path, value in changes:
            with self.subTest(field=path, value=value):
                rows = valid_records()
                target = rows
                for key in path[:-1]:
                    target = target[key]
                target[path[-1]] = value
                self.assertEqual(self.check(rows), 1)
        failed = valid_records(101)
        failed[-1]["pg_container_state"] = {"error": "timeout"}
        self.assertEqual(self.check(failed), 1)
        for bad in ("not-a-container-state", "", "Running", None):
            failed[-1]["pg_container_state"] = {**STATE, "Status": bad}
            self.assertEqual(self.check(failed), 1, bad)


class BoundaryProbeTests(unittest.TestCase):
    def test_probe_hang_descendants_and_output_allocation_bound(self):
        with tempfile.TemporaryDirectory() as raw:
            d = Path(raw)
            for early in (False, True):
                pid = None
                (d / "child").unlink(missing_ok=True)
                child = 'import os,signal,time;from pathlib import Path;signal.signal(signal.SIGTERM,signal.SIG_IGN);Path(%r).write_text(str(os.getpid()));time.sleep(20)' % str(d / "child")
                leader = 'import subprocess,sys,time;subprocess.Popen([sys.executable,"-c",%r]);time.sleep(%s)' % (child, '.1' if early else '20')
                sentinel = subprocess.Popen(["sleep", "20"])
                try:
                    start = time.monotonic()
                    result = M.bounded_probe([sys.executable, "-c", leader], timeout=.5)
                    self.assertEqual(result, {"error": "timeout"})
                    self.assertLess(time.monotonic() - start, 4)
                    pid = int((d / "child").read_text())
                    await_true(lambda: not alive(pid))
                    self.assertIsNone(sentinel.poll())
                finally:
                    if pid is not None and alive(pid):
                        os.kill(pid, signal.SIGKILL)
                        await_true(lambda: not alive(pid))
                    sentinel.terminate()
                    sentinel.wait(timeout=3)
            result = M.bounded_probe([sys.executable, "-c", 'import sys;sys.stdout.write("x"*(4*1024*1024))'])
            self.assertEqual(result, {"error": "output-limit"})
            self.assertIn("error", M.bounded_probe([str(d / "absent")]))

    def test_independent_sinks_and_resource_read_errors(self):
        class BrokenSink:
            def write(self, data):
                raise OSError("broken stdout")
            def flush(self):
                raise OSError("broken stdout")
        with tempfile.TemporaryDirectory() as raw:
            with contextlib.redirect_stdout(BrokenSink()):
                self.assertEqual(M.emit(valid_records()[0], raw), 1)
            self.assertEqual(len(read_lines(Path(raw, "samples.jsonl"))), 1)
            Path(raw, "samples.jsonl").unlink()
            Path(raw, "samples.jsonl").mkdir()
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(M.emit(valid_records()[0], raw), 1)
            self.assertEqual(json.loads(output.getvalue())["phase"], "before")
            meminfo = "\n".join(f"{key}: {value * 1024} kB" for key, value in zip(M.MEMINFO_KEYS, MEM.values()))
            with mock.patch.object(Path, "read_text", return_value=meminfo):
                self.assertEqual(M.memory(), MEM)
            with mock.patch.object(Path, "read_text", return_value="MemTotal: 1024 kB\n"):
                self.assertIn("error", M.memory())
            with mock.patch.object(Path, "read_text", side_effect=OSError("unreadable")):
                self.assertIn("error", M.memory())
            with mock.patch.dict(os.environ, {"GITHUB_WORKSPACE": raw, "RUNNER_TEMP": raw, "CARGO_TARGET_DIR": ""}):
                fields = M.filesystems()
                self.assertEqual(fields["workspace_target"]["measured_path"], raw)
                self.assertEqual(set(fields), {"workspace_target", "runner_temp"})
                with mock.patch.object(M.shutil, "disk_usage", side_effect=OSError("unreadable")):
                    self.assertIn("error", M.filesystems()["workspace_target"])


if __name__ == "__main__":
    unittest.main()
