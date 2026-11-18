"""Continuous E2E runner through its CLI/tick entry with a fake driver; no Discord or provider contact."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO))
from scripts.e2e.continuous import __main__ as cli, channel_lock, runner  # noqa: E402

SHA = "a" * 40
DRIVER = f"""
import json, os, signal, sys
sys.path.insert(0, {str(REPO)!r})
from scripts.e2e.continuous import channel_lock, budget
witness = channel_lock.inherited()
args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
guard = budget.TurnGuard(json.loads(args["--budget"]))
for i in range(int(os.environ.get("FAKE_REQUEST_TURNS", "1"))):
    if not guard.start(1):
        break
    guard.finish(1)
with open(os.environ["FAKE_DRIVER_LOG"], "a") as log:
    log.write(json.dumps({{**args, "lease_run": witness["run_id"], "granted_turns": guard.started}}) + "\\n")
if os.environ.get("FAKE_DRIVER_GATE"):
    signal.alarm(120)  # A failed assertion must not leave this driver blocked on the fifo.
    os.kill(os.getppid(), 9)
    open(os.environ["FAKE_DRIVER_GATE"]).read()
    os.close(witness["fd"])
    open(os.environ["FAKE_DRIVER_GATE"] + ".done", "w").close()
print(json.dumps({{"schema_version": 2, "status": "PASS", "native_turns": json.loads(os.environ.get("FAKE_TURNS", str(guard.started))), "fixture": True, "tokens": guard.started, "usage": {{"native_turns": json.loads(os.environ.get("FAKE_TURNS", str(guard.started))), "input_tokens": guard.started, "output_tokens": 0, "cache_read_tokens": 0, "cache_write_tokens": 0}}, "budget_checked": guard.started}}))
"""


class Runner(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="continuous-runner-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.state = self.root / "state"
        (self.root / "driver.py").write_text(DRIVER)
        self.log = self.root / "driver.log"
        self.env = mock.patch.dict(os.environ, {"FAKE_DRIVER_LOG": str(self.log)})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.config = {"enabled": True, "state_dir": str(self.state), "rotation_interval_s": 3600,
                       "capabilities": ["usage:claude", "turn_budget:claude", "usage:codex", "turn_budget:codex"],
                       "resource": {"guild": "1", "parents": ["2", "3"], "bot_set": ["4", "5"]},
                       "driver": [sys.executable, str(self.root / "driver.py")],
                       "budget": {p: {"daily_turns": 10, "daily_tokens": 10**6, "daily_time_s": 600} for p in ("claude", "codex")},
                       "reservation": {b: {p: {"turns": 6, "tokens": 1000, "time_s": 300} for p in ("claude", "codex")}
                                       for b in ("P", "R1")}}

    def tick(self, now, **changes):
        path = self.root / "config.json"
        path.write_text(json.dumps({**self.config, **changes}))
        return runner.tick(path, now=now)

    def driven(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def submit(self, request_id, sha=SHA):
        (self.root / "config.json").write_text(json.dumps(self.config))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cli.main(["--config", str(self.root / "config.json"), "submit",
                                       "--request-id", request_id, "--deploy-sha", sha]), 0)

    def test_off_tick_takes_no_lock_and_writes_no_state(self):
        for config in ({**self.config, "enabled": False}, None):
            path = self.root / "config.json"
            path.unlink(missing_ok=True)
            if config:
                path.write_text(json.dumps(config))
            out = io.StringIO()
            with mock.patch.object(channel_lock, "acquire", side_effect=AssertionError("lock taken")), \
                    contextlib.redirect_stdout(out):
                self.assertEqual(cli.main(["--config", str(path), "tick"]), 0)
            self.assertEqual(json.loads(out.getvalue()), {"status": "DISABLED"})
            self.assertFalse(self.state.exists())

    def test_concurrent_acquires_admit_exactly_one_holder_per_parent_pair(self):
        alias = {"guild": "1", "parents": ["3", "2"], "bot_set": ["5", "4", "4"]}
        barrier, held, busy = threading.Barrier(8), [], []

        def take(resource):
            barrier.wait()
            try:
                held.append(channel_lock.acquire(self.state, resource))
            except channel_lock.LeaseBusy:
                busy.append(resource)
        threads = [threading.Thread(target=take, args=(self.config["resource"] if i % 2 else alias,)) for i in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual((len(held), len(busy)), (1, 7))
        other = channel_lock.acquire(self.state, {**alias, "parents": ["2", "9"]})
        other.close()
        held[0].close()
        channel_lock.acquire(self.state, alias).close()

    def test_due_boundary_and_cursor_advance(self):
        self.assertEqual(self.tick(1000.0)["status"], "UNEVALUABLE")
        self.assertEqual(self.tick(4599.0)["status"], "NOT_DUE")
        self.assertEqual(self.tick(4600.0)["status"], "UNEVALUABLE")
        self.assertEqual(self.tick(99999.0, rotation_interval_s=None)["status"], "NOT_DUE")
        cells = [row["--cell"] for row in self.driven()]
        self.assertEqual(sorted(set(cells[:5])), ["claude-tui"])
        self.assertEqual(sorted(set(cells[5:])), ["codex-tui"])

    def test_crashed_runner_keeps_lease_while_driver_lives_then_is_reclaimed(self):
        gate = self.root / "gate"
        os.mkfifo(gate)
        os.mkfifo(f"{gate}.done")
        path = self.root / "config.json"
        path.write_text(json.dumps(self.config))
        crashed = subprocess.Popen(
            [sys.executable, "-m", "scripts.e2e.continuous", "--config", str(path), "tick"], cwd=REPO,
            env={**os.environ, "FAKE_DRIVER_GATE": str(gate)}, stdout=subprocess.DEVNULL)
        self.assertEqual(crashed.wait(timeout=60), -9)
        self.assertEqual(self.tick(4e9)["status"], "DEFERRED_LEASE")
        with open(gate, "w"):
            pass
        Path(f"{gate}.done").read_text()
        released = self.tick(4e9 + 1)
        self.assertEqual(released["status"], "DEFERRED_BUDGET")
        records = [json.loads(p.read_text()) for p in (self.state / "runs").glob("*.json")]
        orphan = next(r for r in records if r["run_id"] != released["run_id"])
        self.assertEqual((orphan["result"], orphan["abandoned_by"]), ("ABANDONED", released["run_id"]))
        self.assertEqual(len(records), 1)
        self.assertEqual(orphan["cursor_index"], 0)
        ledger = json.loads((self.state / "budget" / f"{orphan['budget_day']}.json").read_text())
        self.assertEqual(ledger[orphan["run_id"]]["claude"]["turns"], 6)

    def test_budget_reservation_refuses_before_any_driver_start(self):
        self.submit("deploy-1")
        self.assertEqual(self.tick(5000.0, rotation_interval_s=None)["status"], "UNEVALUABLE")
        self.submit("deploy-2", "b" * 40)
        self.assertEqual(self.tick(5001.0, rotation_interval_s=None)["status"], "UNEVALUABLE")
        self.assertEqual(len(self.driven()), 4)
        with mock.patch.dict(os.environ, {"FAKE_TURNS": "1"}):
            self.submit("deploy-3", "c" * 40)
            self.assertEqual(self.tick(90000.0, rotation_interval_s=None)["status"], "UNEVALUABLE")
            self.submit("deploy-4", "d" * 40)
            self.assertEqual(self.tick(90001.0, rotation_interval_s=None)["status"], "UNEVALUABLE")
        self.submit("deploy-5", "e" * 40)
        unset = {p: {"daily_turns": 10, "daily_tokens": None, "daily_time_s": 600} for p in ("claude", "codex")}
        self.assertEqual(self.tick(200000.0, rotation_interval_s=None, budget=unset)["status"], "DEFERRED_BUDGET")
        self.assertEqual(len(self.driven()), 8)
        self.setUp()
        tight = {"R1": {"claude": {"turns": 6, "tokens": 1000, "time_s": 0.001}}}
        self.assertEqual(self.tick(300000.0, reservation=tight)["status"], "UNEVALUABLE")
        with mock.patch.dict(os.environ, {"FAKE_TURNS": "1"}):
            one_turn = {"R1": {"codex": {"turns": 1, "tokens": 1000, "time_s": 300}}}
            self.assertEqual(self.tick(400000.0, reservation=one_turn)["status"], "UNEVALUABLE")
        self.assertEqual(len(self.driven()), 2)

    def test_deploy_inbox_runs_newest_once_and_blocks_missing_capabilities(self):
        self.submit("old")
        self.submit("new", "b" * 40)
        self.submit("new", "c" * 40)
        (self.state / "inbox" / "bad.json").write_text(json.dumps({"request_id": "bad", "token": "x"}))
        self.assertEqual(self.tick(5000.0, rotation_interval_s=None)["status"], "UNEVALUABLE")
        self.assertEqual(self.tick(5001.0, rotation_interval_s=None)["status"], "NOT_DUE")
        self.assertEqual([(r["--scenario"], r["--deploy-sha"]) for r in self.driven()],
                         [("E-50", "b" * 40), ("E-51", "b" * 40)])
        requests = json.loads((self.state / "requests.json").read_text())
        self.assertEqual(requests, {"old": "SUPERSEDED", "new": "UNEVALUABLE", "bad": "REJECTED"})
        record = json.loads(next((self.state / "runs").glob("*.json")).read_text())
        blocked = {r["scenario"]: r["status"] for r in record["occurrences"] if r["status"] != "PASS"}
        self.assertEqual(blocked, dict.fromkeys(["R-marker", "R-inject", "R-fresh", "R-host"], "BLOCKED_CAPABILITY"))


def load_tests(loader, tests, pattern):
    # The required CI entry for this module also selects its independent contract children.
    for name in ("recovery", "summary", "outbox", "publication"):
        tests.addTests(loader.loadTestsFromName(f"scripts.e2e.tests.test_continuous_{name}"))
    return tests


if __name__ == "__main__":
    unittest.main()
