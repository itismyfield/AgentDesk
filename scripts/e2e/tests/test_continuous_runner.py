"""Continuous E2E runner through its CLI/tick entry with a fake driver; no Discord or provider contact."""
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO))
from scripts.e2e.continuous import __main__ as cli, budget, channel_lock, companion, consumer, inbox, outbox, plan, publication, runner, summary  # noqa: E402
from scripts.e2e.continuous.store import read_json, write_json  # noqa: E402

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
        self.now = 1_700_000_000.0
        self.enterContext(mock.patch.object(time, "time", side_effect=lambda: self.now))
        self.config = {"enabled": True, "state_dir": str(self.state), "rotation_interval_s": 3600,
                       "capabilities": ["usage:claude", "turn_budget:claude", "usage:codex", "turn_budget:codex", "E-51/herdr"],
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
        self.now += 1
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
        try:
            self.assertEqual(crashed.wait(timeout=120), -9)
        except subprocess.TimeoutExpired:
            crashed.kill()
            crashed.wait()
            raise
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
        self.setUp()
        token_cap = copy.deepcopy(self.config["budget"])
        token_cap["claude"]["daily_tokens"] = 999
        self.assertEqual(self.tick(500000.0, budget=token_cap)["status"], "DEFERRED_BUDGET")
        self.assertEqual(self.driven(), [])

    def test_deploy_inbox_runs_newest_once_and_blocks_missing_capabilities(self):
        self.submit("old")
        self.submit("new", "b" * 40)
        self.submit("new", "c" * 40)
        (self.state / "inbox" / "bad.json").write_text(json.dumps(
            {"schema_version": 1, "request_id": "bad", "deploy_sha": SHA, "submitted_at": 5000, "token": "x"}))
        self.assertEqual(self.tick(5000.0, rotation_interval_s=None)["status"], "UNEVALUABLE")
        self.assertEqual(self.tick(5001.0, rotation_interval_s=None)["status"], "NOT_DUE")
        self.assertEqual([(r["--scenario"], r["--deploy-sha"]) for r in self.driven()],
                         [("E-50", "b" * 40), ("E-51", "b" * 40)])
        requests = json.loads((self.state / "requests.json").read_text())
        self.assertEqual(requests, {"old": "SUPERSEDED", "new": "UNEVALUABLE", "bad": "REJECTED"})
        record = json.loads(next((self.state / "runs").glob("*.json")).read_text())
        blocked = {r["scenario"]: r["status"] for r in record["occurrences"] if r["status"] != "PASS"}
        self.assertEqual(blocked, dict.fromkeys(["R-marker", "R-inject", "R-fresh", "R-host"], "BLOCKED_CAPABILITY"))



class Recovery(Runner):
    # Reuse the fixture, without collecting the inherited S1 tests a second time.
    def test_unknown_usage_stops_following_occurrences_and_keeps_reservation(self):
        with mock.patch.dict("os.environ", {"FAKE_TURNS": "null"}):
            self.assertEqual(self.tick(1000)["status"], "UNEVALUABLE")
        self.assertEqual(len(self.driven()), 1)
        ledger = read_json(self.state / "budget" / f"{budget.day(1000)}.json", {})
        self.assertEqual(next(iter(ledger.values()))["claude"]["turns"], 6)
        self.tick(100000)
        self.assertEqual(sum(row["--cell"].startswith("claude-") for row in self.driven()), 1)

    def test_missing_usage_capability_never_starts_driver(self):
        self.assertEqual(self.tick(1000, capabilities=[])["status"], "DEFERRED_BUDGET")
        self.assertEqual(self.driven(), [])
        self.assertFalse(self.state.exists())

    def test_null_limits_and_reservations_change_no_files_or_lock(self):
        for section, fields in (("budget", ["daily_turns", "daily_tokens", "daily_time_s"]),
                                ("reservation", ["turns", "tokens", "time_s"])):
            for field in fields:
                config = copy.deepcopy(self.config)
                target = config[section]["claude"] if section == "budget" else config[section]["R1"]["claude"]
                target[field] = None
                with self.subTest(section=section, field=field), mock.patch.object(
                        channel_lock, "acquire", side_effect=AssertionError("lock taken")):
                    self.assertEqual(self.tick(1000, **config)["status"], "DEFERRED_BUDGET")
                    self.assertFalse(self.state.exists())
        self.submit("waiting")
        before = {p.relative_to(self.state): p.read_bytes() for p in self.state.rglob("*") if p.is_file()}
        config = copy.deepcopy(self.config["budget"])
        config["claude"]["daily_turns"] = None
        self.tick(1000, budget=config)
        self.assertEqual(before, {p.relative_to(self.state): p.read_bytes() for p in self.state.rglob("*") if p.is_file()})
        self.assertEqual(self.tick(1001)["status"], "UNEVALUABLE")
        self.submit("waiting-with-status")
        before = {p.relative_to(self.state): p.read_bytes() for p in self.state.rglob("*") if p.is_file()}
        with mock.patch.object(channel_lock, "acquire", side_effect=AssertionError("lock taken")):
            self.assertEqual(self.tick(1002, budget=config)["status"], "DEFERRED_BUDGET")
        self.assertEqual(before, {p.relative_to(self.state): p.read_bytes() for p in self.state.rglob("*") if p.is_file()})

    def test_budget_deferred_request_retries_next_day_once_and_can_be_superseded(self):
        self.submit("waiting")
        caps = {p: {"daily_turns": 1, "daily_tokens": 1000, "daily_time_s": 600} for p in ("claude", "codex")}
        self.assertEqual(self.tick(1000, budget=caps)["status"], "DEFERRED_BUDGET")
        self.assertEqual(self.driven(), [])
        self.assertEqual(self.tick(90000, rotation_interval_s=None)["status"], "UNEVALUABLE")
        count = len(self.driven())
        self.tick(90001, rotation_interval_s=None)
        self.assertEqual(len(self.driven()), count)
        self.submit("old")
        self.tick(90002, budget=caps)
        self.submit("new")
        self.tick(180000)
        self.assertEqual(inbox.ledger(self.state)["old"], "SUPERSEDED")

    def test_native_turn_guard_refuses_second_turn_in_one_occurrence(self):
        with mock.patch.dict("os.environ", {"FAKE_REQUEST_TURNS": "3"}):
            one = {"R1": {"claude": {"turns": 1, "tokens": 1000, "time_s": 300}}}
            self.tick(1000, reservation=one)
        self.assertEqual(len(self.driven()), 1)
        self.assertEqual(self.driven()[0]["granted_turns"], 1)

    def test_native_guard_counts_startup_and_blocks_unsettled_tokens(self):
        guard = budget.TurnGuard({"turns": 2, "tokens": 10, "time_s": 100, "deadline": time.monotonic() - 1})
        self.assertFalse(guard.start(1))
        guard = budget.TurnGuard({"turns": 2, "tokens": 10, "time_s": 100})
        self.assertTrue(guard.start(5))
        guard.finish(None)
        self.assertFalse(guard.start(1))

    def test_functional_fail_survives_missing_budget_witness(self):
        lease = mock.Mock()
        lease.spawn.return_value = subprocess.CompletedProcess([], 1, json.dumps({"schema_version": 2, "status": "FAIL", "native_turns": 1}))
        row = {"scenario": "E-1", "cell": "claude-tui"}
        runner._execute(lease, self.config, {"profile": "default", "run_id": "test", "request": {}}, row,
                        {"turns": 1, "tokens": 10, "time_s": 10})
        self.assertEqual(row["status"], "FAIL")

    def test_unknown_usage_components_do_not_become_zero(self):
        lease = mock.Mock()
        lease.spawn.return_value = subprocess.CompletedProcess([], 0, json.dumps({"schema_version": 2, "status": "PASS", "native_turns": 1,
                                                                               "tokens": 1, "budget_checked": 1}))
        row = {"scenario": "E-1", "cell": "claude-tui"}
        runner._execute(lease, self.config, {"profile": "default", "run_id": "test", "request": {}}, row,
                        {"turns": 1, "tokens": 10, "time_s": 10})
        self.assertIsNone(row["tokens"])

    def test_crash_before_reservation_or_deferral_does_not_consume_request(self):
        for boundary in ("reservation", "deferral"):
            with self.subTest(boundary=boundary):
                self.setUp()
                self.submit("waiting")
                original = inbox.record

                def record(state, updates):
                    if "DEFERRED_BUDGET" in updates.values():
                        raise RuntimeError("deferral crash")
                    return original(state, updates)

                patch = mock.patch.object(budget, "reserve", side_effect=RuntimeError("reservation crash")) if boundary == "reservation" else mock.patch.object(inbox, "record", side_effect=record)
                caps = {p: {"daily_turns": 1, "daily_tokens": 1000, "daily_time_s": 600} for p in ("claude", "codex")}
                with patch, self.assertRaises(RuntimeError):
                    self.tick(1000, budget=caps)
                self.assertEqual(self.driven(), [])
                self.assertEqual(self.tick(90000, rotation_interval_s=None)["status"], "UNEVALUABLE")
                self.assertEqual(len(self.driven()), 2)

    def test_crash_after_driver_leaves_unknown_provider_paused(self):
        original = runner._execute

        def execute(*args):
            original(*args)
            raise RuntimeError("crash before recording usage")

        with mock.patch.object(runner, "_execute", side_effect=execute), self.assertRaises(RuntimeError):
            self.tick(1000)
        self.assertEqual(len(self.driven()), 1)
        self.assertEqual(self.tick(90000)["status"], "DEFERRED_BUDGET")
        self.assertEqual(len(self.driven()), 1)

    def test_completion_replay_is_idempotent_at_each_file_boundary(self):
        for kind in ("scheduled", "deploy"):
            for boundary in ("settle", "record", "projection", "mark_applied"):
                with self.subTest(kind=kind, boundary=boundary):
                    self.setUp()
                    if kind == "deploy":
                        self.submit("request")
                    original = runner._record

                    def crash_record(state, record, **changes):
                        if boundary == "mark_applied" and changes.get("applied"):
                            raise RuntimeError("crash before projection acknowledgment")
                        value = original(state, record, **changes)
                        if boundary == "record" and "completed_at" in changes:
                            raise RuntimeError("crash after durable completion")
                        return value

                    projection_owner = plan if kind == "scheduled" else inbox
                    projection_name = "save_cursor" if kind == "scheduled" else "record"
                    projection_fn = getattr(projection_owner, projection_name)

                    def crash_projection(state, value):
                        if (kind == "scheduled" and value["index"] == 1) or (kind == "deploy" and value.get("request") not in ("RUNNING", "DEFERRED_BUDGET")):
                            raise RuntimeError("projection crash")
                        return projection_fn(state, value)

                    patch = mock.patch.object(budget, "settle", side_effect=RuntimeError("settle crash")) if boundary == "settle" else (
                        mock.patch.object(runner, "_record", side_effect=crash_record) if boundary in ("record", "mark_applied") else
                        mock.patch.object(projection_owner, projection_name, side_effect=crash_projection))
                    with patch:
                        with self.assertRaises(RuntimeError):
                            self.tick(1000)
                    before = len(self.driven())
                    # Recovery precedes deciding whether another run is due.
                    self.tick(1001, rotation_interval_s=None)
                    records = [read_json(p, {}) for p in (self.state / "runs").glob("*.json")]
                    terminal = next(r for r in records if r.get("completed_at"))
                    self.assertNotEqual(terminal["result"], "ABANDONED")
                    self.assertEqual(len(self.driven()), before)
                    if kind == "scheduled":
                        self.assertEqual(plan.cursor(self.state)["index"], 1)
                    else:
                        self.assertEqual(inbox.ledger(self.state)["request"], terminal["result"])
                    runner._abandon_orphans(self.state, "recovery-again")
                    self.assertEqual(plan.cursor(self.state)["index"], int(kind == "scheduled"))


# unittest otherwise collects imported/inherited tests as well as the new boundary tests.
for _name in list(Runner.__dict__):
    if _name.startswith("test_"):
        setattr(Recovery, _name, None)


IDENTITY = {"instance_id": "instance", "boot_id": "boot", "head": SHA,
            "binary_sha256": "b" * 64, "profile_hash": "c" * 64}


KEY = "E-1/claude/tmux/channel/sequential/announce_bot/none/none/default/delivery/1"


def report():
    return {"schema_version": 1, **IDENTITY, "run_id": "run", "suite": "R1", "profile": "default",
            "started_at": 100, "completed_at": 110, "expected": [KEY],
            "occurrences": [{"key": KEY, "actual_occurrence_key": KEY, "status": "PASS", "scenario": "E-1", "provider": "claude", "host": "tmux",
                             "surface": "channel", "state": "sequential", "origin": "announce_bot",
                             "busy_basis": None, "placement": None, "output_profile": "default",
                             "operation": "delivery", "occurrence": 1,
                             "assertions_expected": ["delivery"],
                             "assertions": [{"key": "delivery", "status": "PASS", "complete": True}],
                             "real_provider_contacted": True, "evidence_paths": ["raw.json"], **IDENTITY}],
            "result": "PASS", "usage_complete": True,
            "usage": {"claude": {"native_turns": 1, "input_tokens": 0, "output_tokens": 1,
                                 "cache_read_tokens": 0, "cache_write_tokens": 0}}, "attempts": []}


def status():
    return {"schema_version": 1, "enabled": True, "heartbeat_at": 115, "tick_interval_s": 10,
            "rotation_interval_s": 100, "max_run_s": 30, "tick_grace_s": 5,
            "last_completed_run": "run", "outbox_pending": 0, "next_due": 200}


class Summary(unittest.TestCase):
    def test_complete_live_report_is_pass_and_formats_counts(self):
        view = summary.consume(report(), status(), head=SHA, now=120)
        self.assertEqual(view["check"]["result"], "PASS")
        self.assertEqual((view["check"]["live"], view["check"]["expected"],
                          view["check"]["assertions"], view["check"]["assertions_expected"]), (1, 1, 1, 1))
        check, monitor = summary.format_lines(view)
        self.assertEqual(check, f"E2E_CHECK suite=R1 head={SHA} result=PASS live=1/1 assertions=1/1 age_s=10 stale=0 reason=none")
        self.assertIn("E2E_MONITOR enabled=1 heartbeat_age_s=5 last_run=run pending_alerts=0 next_due=200", monitor)

    def test_zero_skip_fixture_missing_assertion_timeout_and_unknown_usage_are_not_pass(self):
        variants = []
        zero = report(); zero.update(expected=[], occurrences=[]); variants.append((zero, "NO_LIVE_OCCURRENCES"))
        for field, value, reason in [("status", "SKIPPED", "SKIPPED"), ("fixture", True, "FIXTURE"),
                                     ("dry_run", True, "FIXTURE"), ("timeout", True, "TIMEOUT"),
                                     ("dirty_residue", True, "DIRTY_RESIDUE"),
                                     ("real_provider_contacted", False, "NO_PROVIDER_CONTACT"),
                                     ("evidence_paths", [], "MISSING_EVIDENCE")]:
            value_report = report(); value_report["occurrences"][0][field] = value
            variants.append((value_report, reason))
        missing = report(); missing["occurrences"][0]["assertions"] = []; variants.append((missing, "MISSING_ASSERTIONS"))
        measured = summary.evaluate(missing)
        self.assertEqual((measured["live"], measured["assertions"], measured["assertions_expected"]), (1, 0, 1))
        no_required = report(); no_required["occurrences"][0]["assertions_expected"] = []; variants.append((no_required, "MISSING_ASSERTIONS"))
        incomplete = report(); incomplete["occurrences"][0]["assertions"][0]["complete"] = False
        variants.append((incomplete, "MISSING_ASSERTIONS"))
        usage = report(); usage["usage"]["claude"]["native_turns"] = None; variants.append((usage, "USAGE_UNKNOWN"))
        flag = report(); flag["usage_complete"] = False; variants.append((flag, "USAGE_UNKNOWN"))
        for counter in (True, -1, 0.5):
            invalid = report(); invalid["usage"]["claude"]["native_turns"] = counter
            variants.append((invalid, "USAGE_UNKNOWN"))
        no_provider = report(); no_provider["usage"] = {}; variants.append((no_provider, "USAGE_UNKNOWN"))
        for value_report, reason in variants:
            with self.subTest(reason=reason):
                result = summary.evaluate(value_report, head=SHA)
                self.assertNotEqual(result["result"], "PASS")
                self.assertEqual(result["reason"], reason)

    def test_expected_occurrence_identity_and_duplicate_assertions_cannot_be_forged(self):
        changed = report(); changed["expected"].append("missing")
        self.assertEqual(summary.evaluate(changed)["result"], "PARTIAL")
        for field in IDENTITY:
            changed = report(); changed["occurrences"][0][field] = "wrong"
            self.assertEqual(summary.evaluate(changed)["reason"], "IDENTITY_MISMATCH")
        changed = report(); changed["occurrences"].append(copy.deepcopy(changed["occurrences"][0]))
        self.assertEqual(summary.evaluate(changed)["reason"], "DUPLICATE_OCCURRENCE")
        changed = report(); changed["occurrences"][0]["assertions"].append(copy.deepcopy(changed["occurrences"][0]["assertions"][0]))
        self.assertEqual(summary.evaluate(changed)["reason"], "MISSING_ASSERTIONS")
        changed = report(); changed["binary_sha256"] = None
        self.assertEqual(summary.evaluate(changed)["reason"], "MISSING_IDENTITY")
        changed = report(); changed["head"] = "z" * 40
        self.assertEqual(summary.evaluate(changed)["reason"], "MISSING_IDENTITY")

    def test_old_head_schema_future_time_stale_and_no_data_are_explicit(self):
        self.assertEqual(summary.consume(None, status(), head=SHA, now=120)["check"]["result"], "NO_DATA")
        value = report(); value["schema_version"] = 2
        self.assertEqual(summary.consume(value, status(), head=SHA, now=120)["check"]["reason"], "UNSUPPORTED_SCHEMA")
        self.assertEqual(summary.consume(report(), status(), head="d" * 40, now=120)["check"]["reason"], "STALE_HEAD")
        value = report(); value["completed_at"] = 121
        self.assertEqual(summary.consume(value, status(), head=SHA, now=120)["check"]["reason"], "FUTURE_TIMESTAMP")
        value = status(); value["heartbeat_at"] = 121
        self.assertEqual(summary.consume(report(), value, head=SHA, now=120)["check"]["reason"], "FUTURE_TIMESTAMP")
        value = status(); value["heartbeat_at"] = 300
        self.assertEqual(summary.consume(report(), value, head=SHA, now=300)["check"]["reason"], "STALE")
        value = status(); value["heartbeat_at"] = 100
        self.assertEqual(summary.consume(report(), value, head=SHA, now=120)["check"]["reason"], "HEARTBEAT_STALE")
        value = status(); value["enabled"] = False
        disabled = summary.consume(report(), value, head=SHA, now=120)
        self.assertEqual(disabled["check"]["result"], "DISABLED")
        self.assertEqual(disabled["monitor"]["heartbeat_age_s"], 5)

    def test_failures_and_failed_attempts_are_preserved(self):
        value = report(); value["occurrences"][0]["assertions"][0]["status"] = "FAIL"
        self.assertEqual(summary.evaluate(value)["result"], "FAIL")
        value = report(); value["attempts"] = [{"result": "FAIL", "classification": "ENV_SUSPECT"}]
        self.assertEqual(summary.evaluate(value)["result"], "RECOVERED_ENV")
        value["attempts"] = [{"result": "FAIL"}]
        self.assertEqual(summary.evaluate(value)["result"], "FLAKY")
        value = report(); value["result"] = "FAIL"
        self.assertEqual(summary.evaluate(value)["result"], "FAIL")

    def test_builder_preserves_input_report_and_unknowns_without_mutation(self):
        value = report(); before = copy.deepcopy(value)
        value["bundle"] = value.pop("suite")
        built = summary.build(value, IDENTITY)
        self.assertEqual(built["schema_version"], 1)
        self.assertEqual(built["suite"], "R1")
        self.assertEqual(summary.evaluate(built)["result"], "PASS")
        self.assertEqual(value["occurrences"], before["occurrences"])
        self.assertNotIn("suite", value)
        value["usage"]["claude"]["native_turns"] = None
        self.assertIsNone(summary.build(value, IDENTITY)["usage"]["claude"]["native_turns"])
        value["occurrences"][0]["origin"] = "controlled_ingress"
        built = summary.build(value, IDENTITY)
        self.assertEqual(built["result"], "PARTIAL")

    def test_planned_keys_and_required_assertions_cannot_be_replaced_by_driver(self):
        manifest = json.loads((Path(__file__).resolve().parents[3] / "docs/e2e-continuous/bundles.schema1.json").read_text())
        self.assertEqual(list(summary.KEY_FIELDS), manifest["execution_contract"]["occurrence_key_fields"])
        row = report()["occurrences"][0]
        row["key"] = "forged/driver/key"
        self.assertEqual(summary.occurrence_key(row), KEY)
        for field in ("state", "origin", "busy_basis", "placement", "output_profile", "operation", "occurrence"):
            changed = copy.deepcopy(row); changed[field] = "different"
            self.assertNotEqual(summary.occurrence_key(changed), KEY)
        built = summary.build(report(), {**IDENTITY, "required_assertions": {"E-1": ["delivery", "ordering"]}})
        self.assertEqual(built["result"], "UNEVALUABLE")
        self.assertEqual(built["occurrences"][0]["assertions_expected"], ["delivery", "ordering"])
        altered = report(); altered["occurrences"][0]["key"] = "forged/driver/key"; altered["expected"] = ["forged/driver/key"]
        self.assertEqual(summary.evaluate(altered)["reason"], "OCCURRENCE_KEY_MISMATCH")
        unknown = report(); unknown["occurrences"][0].pop("origin")
        unknown["occurrences"][0]["key"] = summary.occurrence_key(unknown["occurrences"][0])
        unknown["expected"] = [unknown["occurrences"][0]["key"]]
        self.assertEqual(summary.evaluate(unknown)["reason"], "MISSING_OCCURRENCE_CONTRACT")
        base = report()["occurrences"][0]
        base.pop("provider"); base.pop("host"); base.pop("output_profile"); base["cell"] = "claude-tui"
        self.assertEqual(summary.occurrence_key(base), KEY)

    def test_actual_occurrence_witness_must_match_the_planned_qualifiers(self):
        value = report()
        self.assertEqual(summary.evaluate(value)["result"], "PASS")
        for actual in (None, "E-1/claude/tmux/channel/sequential/controlled_ingress/none/none/default/delivery/1"):
            value["occurrences"][0]["actual_occurrence_key"] = actual
            evaluated = summary.evaluate(value)
            self.assertEqual(evaluated["result"], "UNEVALUABLE")
            self.assertEqual(evaluated["reason"], "OCCURRENCE_WITNESS_MISMATCH")
            self.assertEqual((evaluated["live"], evaluated["assertions"]), (0, 0))
            self.assertEqual(value["occurrences"][0]["key"], KEY)

    def test_malformed_input_is_unknown_without_exceptions_or_claimed_pass(self):
        malformed = report(); malformed["occurrences"][0]["assertions"][0]["key"] = []
        for value in ([], malformed, {**report(), "schema_version": True}, {"schema_version": 1}, {**report(), "occurrences": [None]},
                      {**report(), "expected": [None]}, {**report(), "usage": {"claude": None}}):
            self.assertNotEqual(summary.evaluate(value)["result"], "PASS")
        for value in (None, [], {"schema_version": 2}, {**status(), "schema_version": True}, {**status(), "heartbeat_at": "broken"}):
            self.assertEqual(summary.consume(report(), value, head=SHA, now=120)["check"]["result"], "UNKNOWN")


class ReviewS2(unittest.TestCase):
    setUp, tick, submit = Runner.setUp, Runner.tick, Runner.submit

    def live(self, change=lambda body: None, at="E-15"):
        scenarios = ("E-1", "E-2", "E-3", "E-5", "E-15")
        contract = {"surface": "channel", "state": "sequential", "origin": "announce_bot", "busy_basis": None,
                    "placement": None, "output_profile": "default", "operation": "delivery", "occurrence": 1}
        self.config.update(**IDENTITY, occurrence_contracts={s: contract for s in scenarios},
                           required_assertions={s: ["delivery"] for s in scenarios})

        def spawn(argv, run_id, **kwargs):
            args = dict(zip(argv[len(self.config["driver"])::2], argv[len(self.config["driver"]) + 1::2]))
            row = {**contract, "scenario": args["--scenario"], "cell": args["--cell"]}
            body = {"schema_version": 2, "status": "PASS", **IDENTITY, "native_turns": 1, "tokens": 1,
                    "budget_checked": 1, "usage": {field: int(field in ("native_turns", "input_tokens")) for field in summary.USAGE},
                    "actual_occurrence_key": summary.occurrence_key(row), "real_provider_contacted": True,
                    "assertions": [{"key": "delivery", "status": "PASS", "complete": True}], "evidence_paths": ["synthetic.json"]}
            if row["scenario"] == at:
                change(body)
            return subprocess.CompletedProcess(argv, 0, json.dumps(body))
        return mock.patch.object(channel_lock.ChannelLease, "spawn", side_effect=spawn)

    def completed(self, result):
        return read_json(self.state / "runs" / f"{result['run_id']}.json", {})

    def test_last_occurrence_unknown_cannot_publish_pass(self):
        for field, value in (("tokens", 2), ("native_turns", None), ("native_turns", "missing")):
            with self.subTest(field=field, value=value):
                self.setUp()
                def change(body):
                    body.pop(field) if value == "missing" else body.update({field: value})
                with self.live(change):
                    result = self.tick(time.time())
                record = self.completed(result)
                self.assertEqual(result["status"], "UNEVALUABLE")
                self.assertFalse(record["usage_complete"])
                self.assertIn("claude", read_json(self.state / "budget/paused.json", {}))
                self.assertEqual(read_json(self.state / f"budget/{record['budget_day']}.json", {})[record["run_id"]]["claude"]["tokens"], 1000)
                self.assertEqual(outbox.pending(self.state), 1)
                view = publication.view({**self.config, "state_dir": self.state}, head=SHA, now=time.time())
                self.assertNotEqual(view["check"]["result"], "PASS")

    def test_driver_schema_is_checked_before_evidence_is_adopted(self):
        for version in (2, 999, None, True, "missing"):
            with self.subTest(version=version):
                self.setUp()
                def change(body):
                    body.pop("schema_version") if version == "missing" else body.update(schema_version=version)
                with self.live(change):
                    result = self.tick(time.time())
                self.assertEqual(result["status"], "PASS" if type(version) is int and version == 2 else "UNEVALUABLE")
                row = self.completed(result)["occurrences"][-1]
                raw = read_json(row["driver_report_path"], {})
                self.assertEqual(raw.get("schema_version", "missing"), version)
                if version != 2:
                    self.assertNotIn("real_provider_contacted", row)

    def test_required_qualifiers_reject_absent_null_empty_and_wrong_types(self):
        for field in set(summary.KEY_FIELDS) - {"busy_basis", "placement"}:
            for invalid in (None, "", " ", [], "missing"):
                with self.subTest(field=field, invalid=invalid):
                    value = report(); row = value["occurrences"][0]
                    row.pop(field) if invalid == "missing" else row.update({field: invalid})
                    if field == "output_profile" and invalid == "missing":
                        value["profile"] = None
                    row["key"] = row["actual_occurrence_key"] = summary.occurrence_key(row, value.get("profile", "default"))
                    value["expected"] = [row["key"]]
                    verdict = summary.evaluate(value)
                    self.assertEqual(verdict["result"], "UNEVALUABLE")
                    self.assertEqual(verdict["live"], 0)
        self.assertEqual(summary.evaluate(report())["result"], "PASS")

    def test_malformed_usage_completes_and_keeps_fail_without_recovery(self):
        for usage in (None, []):
            for outcome in ("PASS", "FAIL"):
                with self.subTest(usage=usage, outcome=outcome):
                    self.setUp()
                    with self.live(lambda body: body.update(usage=usage, status=outcome), at="E-1") as spawned:
                        result = self.tick(time.time())
                    record = self.completed(result)
                    self.assertEqual(result["status"], "FAIL" if outcome == "FAIL" else "UNEVALUABLE")
                    self.assertEqual(spawned.call_count, 1)
                    self.assertTrue(record["completed_at"] and record["applied"])
                    self.assertEqual(plan.cursor(self.state)["index"], 1)
                    self.assertIn("claude", read_json(self.state / "budget/paused.json", {}))
                    self.assertEqual(outbox.pending(self.state), 1)
                    self.assertEqual(read_json(record["occurrences"][0]["driver_report_path"], {})["usage"], usage)

    def test_daily_budget_deferral_keeps_supervisor_heartbeat(self):
        now = time.time()
        with self.live():
            self.assertEqual(self.tick(now)["status"], "PASS")
        self.submit("budget-wait")
        write_json(self.state / f"budget/{budget.day(now)}.json", {"spent": {p: {"turns": 10, "tokens": 10**6, "time_s": 600} for p in ("claude", "codex")}})
        before = (self.state / f"budget/{budget.day(now)}.json").read_bytes()
        with self.live() as spawned:
            self.assertEqual(self.tick(now + 180)["status"], "DEFERRED_BUDGET")
        self.assertEqual(spawned.call_count, 0)
        self.assertEqual((self.state / f"budget/{budget.day(now)}.json").read_bytes(), before)
        self.assertEqual(inbox.ledger(self.state)["budget-wait"], "DEFERRED_BUDGET")
        self.assertEqual(read_json(self.state / "status.json", {})["heartbeat_at"], now + 180)
        view = publication.view({**self.config, "state_dir": self.state}, head=SHA, now=now + 180)
        self.assertNotEqual(view["check"]["reason"], "HEARTBEAT_STALE")







class Outbox(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="continuous-outbox-")
        self.addCleanup(temp.cleanup)
        self.state = Path(temp.name)
        self.summary = {"schema_version": 1, "run_id": "run-1", "head": "a" * 40,
                        "suite": "P", "profile_hash": "profile", "result": "FAIL",
                        "occurrences": [{"key": "one", "status": "FAIL", "reason": "lost_output"}]}

    def test_failed_notification_stays_pending_and_retries_once(self):
        event_id = outbox.enqueue(self.state, self.summary, 100)
        calls = []

        def send(event):
            calls.append(event["event_id"])
            if len(calls) == 1:
                raise OSError("fixture failure")
            return True

        self.assertEqual(outbox.dispatch(self.state, send, 100), {"attempted": 1, "sent": 0, "pending": 1})
        self.assertEqual(outbox.dispatch(self.state, send, 159)["attempted"], 0)
        self.assertEqual(outbox.dispatch(self.state, send, 160), {"attempted": 1, "sent": 1, "pending": 0})
        self.assertEqual(calls, [event_id, event_id])
        self.assertEqual(outbox.dispatch(self.state, send, 9999)["attempted"], 0)

    def test_unknown_ack_and_exhaustion_preserve_pending(self):
        outbox.enqueue(self.state, self.summary, 100)
        for now in (100, 160, 220):
            self.assertEqual(outbox.dispatch(self.state, lambda event: None, now)["sent"], 0)
        self.assertEqual(outbox.dispatch(self.state, lambda event: True, 280)["attempted"], 0)
        self.assertEqual(outbox.pending(self.state), 1)
        row = next(iter(json.loads((self.state / "outbox.json").read_text())["events"].values()))
        self.assertEqual(row["attempts"], 3)
        self.assertTrue(row["exhausted"])

    def test_identical_run_and_known_gap_do_not_repeat_but_state_changes_do(self):
        first = outbox.enqueue(self.state, self.summary, 100)
        same = {**self.summary, "run_id": "run-2"}
        self.assertIsNone(outbox.enqueue(self.state, same, 101))
        passed = {**same, "result": "PASS", "occurrences": [{"key": "one", "status": "PASS"}]}
        recovered = outbox.enqueue(self.state, passed, 102)
        self.assertIsNotNone(recovered)
        failed_again = outbox.enqueue(self.state, same, 103)
        self.assertEqual(len({first, recovered, failed_again}), 3)
        gap = {**same, "result": "PARTIAL", "occurrences": [
            {"key": "human", "status": "UNEVALUABLE", "reason": "NOT_RUN_HUMAN"}]}
        self.assertIsNotNone(outbox.enqueue(self.state, gap, 104))
        self.assertIsNone(outbox.enqueue(self.state, {**gap, "run_id": "run-3"}, 105))
        self.assertIsNotNone(outbox.enqueue(self.state, {**gap, "profile_hash": "other"}, 106))
        changed = copy.deepcopy(gap)
        changed["occurrences"][0]["reason"] = "RUNNER_DIED"
        self.assertIsNotNone(outbox.enqueue(self.state, changed, 107))

    def test_provenance_and_changed_failed_occurrence_notify_separately(self):
        claude = {**self.summary, "expected": ["scenario/claude/tmux/human_gateway"], "occurrences": [
            {"key": "scenario/claude/tmux/human_gateway", "status": "FAIL", "reason": "lost_output"}]}
        codex = {**claude, "expected": ["scenario/codex/tmux/controlled_ingress"], "occurrences": [
            {"key": "scenario/codex/tmux/controlled_ingress", "status": "FAIL", "reason": "lost_output"}]}
        self.assertIsNotNone(outbox.enqueue(self.state, claude, 100))
        self.assertIsNone(outbox.enqueue(self.state, {**codex, "result": "PASS", "occurrences": [
            {"key": codex["expected"][0], "status": "PASS"}]}, 100.5))
        self.assertIsNotNone(outbox.enqueue(self.state, codex, 101))
        first = {**self.summary, "expected": ["a", "b"], "occurrences": [
            {"key": "a", "status": "FAIL", "reason": "lost_output"}, {"key": "b", "status": "PASS"}]}
        second = {**first, "expected": ["b", "a"], "occurrences": [
            {"key": "a", "status": "PASS"}, {"key": "b", "status": "FAIL", "reason": "lost_output"}]}
        self.assertIsNotNone(outbox.enqueue(self.state, first, 102))
        self.assertIsNotNone(outbox.enqueue(self.state, second, 103))
        self.assertIsNone(outbox.enqueue(self.state, {**second, "expected": ["a", "b"]}, 104))

    def test_top_level_failure_reason_and_assertion_changes_notify(self):
        value = {**self.summary, "result": "UNEVALUABLE", "reason": "USAGE_UNKNOWN", "occurrences": [
            {"key": "one", "status": "PASS"}]}
        self.assertIsNotNone(outbox.enqueue(self.state, value, 100))
        self.assertIsNotNone(outbox.enqueue(self.state, {**value, "reason": "MISSING_ASSERTIONS"}, 101))
        first = {**self.summary, "occurrences": [{"key": "one", "status": "PASS", "assertions": [
            {"key": "delivery", "status": "FAIL"}]}]}
        second = {**first, "occurrences": [{"key": "one", "status": "PASS", "assertions": [
            {"key": "host", "status": "FAIL"}]}]}
        self.assertIsNotNone(outbox.enqueue(self.state, first, 102))
        self.assertIsNotNone(outbox.enqueue(self.state, second, 103))

    def test_malformed_assertion_evidence_still_queues_an_alert(self):
        value = {**self.summary, "result": "UNEVALUABLE", "reason": "MISSING_ASSERTIONS", "occurrences": [
            {"key": "one", "status": "PASS", "assertions": None}]}
        self.assertIsNotNone(outbox.enqueue(self.state, value, 100))
        self.assertEqual(outbox.pending(self.state), 1)

    def test_timeout_death_and_partial_summaries_are_deliverable(self):
        events = []
        for index, reason in enumerate(("TIMEOUT", "RUNNER_DIED", "USAGE_UNKNOWN")):
            summary = {**self.summary, "result": "UNEVALUABLE", "occurrences": [
                {"key": "one", "status": "UNEVALUABLE", "reason": reason}]}
            self.assertIsNotNone(outbox.enqueue(self.state, summary, 100 + index))
        result = outbox.dispatch(self.state, lambda event: events.append(event) or True, 200, limit=3)
        self.assertEqual(result, {"attempted": 3, "sent": 3, "pending": 0})
        self.assertEqual({e["reasons"][0] for e in events}, {"TIMEOUT", "RUNNER_DIED", "USAGE_UNKNOWN"})

    def test_crashed_sender_charges_attempt_before_transport(self):
        outbox.enqueue(self.state, self.summary, 100)

        def crash(event):
            raise KeyboardInterrupt("fixture interruption")

        with self.assertRaises(KeyboardInterrupt):
            outbox.dispatch(self.state, crash, 100)
        row = next(iter(json.loads((self.state / "outbox.json").read_text())["events"].values()))
        self.assertEqual((row["attempts"], row["next_attempt_at"], row["status"]), (1, 160, "pending"))
        self.assertEqual(outbox.dispatch(self.state, lambda event: True, 159)["attempted"], 0)

    def test_schema_mismatch_does_not_create_state(self):
        with self.assertRaises(ValueError):
            outbox.enqueue(self.state, {**self.summary, "schema_version": 2}, 100)
        self.assertFalse((self.state / "outbox.json").exists())

    def test_initial_pass_and_disabled_budget_tick_never_notify(self):
        self.assertEqual(outbox.pending(self.state), 0)
        self.assertFalse((self.state / "outbox.json").exists())
        for result in ("PASS", "DISABLED", "DEFERRED_BUDGET", "DEFERRED_LEASE", "NOT_DUE"):
            self.assertIsNone(outbox.enqueue(self.state, {**self.summary, "result": result}, 100))
        self.assertEqual(outbox.pending(self.state), 0)


class Publication(unittest.TestCase):
    setUp, tick, driven, submit = Runner.setUp, Runner.tick, Runner.driven, Runner.submit

    def test_scheduled_fixture_and_failed_then_acknowledged_notification(self):
        now = time.time()
        self.config.update(head="a" * 40, instance_id="fixture-instance", boot_id="fixture-boot",
                           binary_sha256="b" * 64, profile_hash="c" * 64)
        result = self.tick(now)
        self.assertEqual(result["status"], "UNEVALUABLE")
        self.assertEqual(len(self.driven()), 5)
        value = read_json(self.state / "summary.json", {})
        self.assertEqual(value["run_id"], result["run_id"])
        self.assertEqual(value["schema_version"], 1)
        self.assertTrue(all(row.get("fixture") for row in value["occurrences"]))
        for row in value["occurrences"]:
            self.assertEqual(read_json(row["driver_report_path"], {})["schema_version"], 2)
        self.assertEqual(summary.evaluate(value)["live"], 0)
        delivered = []
        self.assertEqual(outbox.dispatch(self.state, lambda event: False, now + 1)["pending"], 1)
        self.assertEqual(outbox.dispatch(self.state, lambda event: delivered.append(event) or True, now + 61)["sent"], 1)
        self.assertEqual(len(delivered), 1)
        self.assertEqual(delivered[0]["run_id"], result["run_id"])
        publication.heartbeat(self.state, self.config, now + 61)
        config = {**self.config, "state_dir": self.state}
        view = publication.view(config, head=self.config["head"], now=now + 61)
        self.assertEqual(view["monitor"]["pending_alerts"], 0)
        self.assertEqual(view["check"]["result"], "UNEVALUABLE")
        record = read_json(self.state / "runs" / f"{result['run_id']}.json", {})
        publication.finish(self.state, record, self.config)
        self.assertEqual(outbox.pending(self.state), 0)

    def test_crash_between_summary_outbox_status_writes_replays_without_driver(self):
        for boundary in ("outbox", "summary", "status"):
            with self.subTest(boundary=boundary):
                self.setUp()
                original = publication.write_json

                def write(path, value):
                    if path.name == boundary + ".json" and (boundary != "status" or value.get("running") is None):
                        raise RuntimeError("publication crash")
                    return original(path, value)

                patch = mock.patch.object(outbox, "enqueue", side_effect=RuntimeError("outbox crash")) if boundary == "outbox" else mock.patch.object(publication, "write_json", side_effect=write)
                with patch, self.assertRaises(RuntimeError):
                    self.tick(1000)
                count = len(self.driven())
                self.assertEqual(self.tick(1001)["status"], "NOT_DUE")
                self.assertEqual(len(self.driven()), count)
                self.assertEqual(read_json(self.state / "summary.json", {})["result"], "UNEVALUABLE")
                self.assertEqual(outbox.pending(self.state), 1)
                self.assertTrue(read_json(self.state / "status.json", {})["last_completed_run"])

    def test_off_observation_reads_no_state_and_does_not_create_it(self):
        with mock.patch.object(publication, "read_json", side_effect=AssertionError("state read")):
            view = publication.view({"enabled": False}, head="a" * 40, now=1000)
        self.assertEqual(view["check"]["result"], "DISABLED")
        self.assertEqual(view["monitor"]["heartbeat_age_s"], 0)
        self.assertFalse(self.state.exists())

    def test_not_due_tick_refreshes_heartbeat_without_starting_a_driver(self):
        now = time.time()
        self.tick(now)
        count = len(self.driven())
        self.assertEqual(self.tick(now + 180)["status"], "NOT_DUE")
        self.assertEqual(len(self.driven()), count)
        state = read_json(self.state / "status.json", {})
        self.assertEqual(state["heartbeat_at"], now + 180)

    def test_usage_paused_tick_refreshes_heartbeat_without_new_turns(self):
        now = time.time()
        with mock.patch.dict("os.environ", {"FAKE_TURNS": "null"}):
            self.tick(now)
        self.assertEqual(len(self.driven()), 1)
        self.submit("while-paused")
        self.assertEqual(self.tick(now + 180)["status"], "DEFERRED_BUDGET")
        self.assertEqual(len(self.driven()), 1)
        self.assertEqual(read_json(self.state / "status.json", {})["heartbeat_at"], now + 180)

    def test_completed_assertion_contract_is_frozen_during_republication(self):
        value = report()
        value["assertion_contract"] = {"E-1": ["delivery"]}
        publication.finish(self.state, value, self.config)
        changed = {**self.config, "required_assertions": {"E-1": ["delivery", "ordering"]}}
        publication.finish(self.state, value, changed)
        self.assertEqual(read_json(self.state / "summary.json", {})["result"], "PASS")

    def test_blocking_driver_keeps_running_heartbeat_and_joins_the_writer(self):
        observed, calls = threading.Event(), []

        def heartbeat(state, config, now, running):
            calls.append(running)
            if len(calls) == 3:
                observed.set()

        with mock.patch.object(publication, "heartbeat", side_effect=heartbeat):
            with publication.keepalive(self.state, {"tick_interval_s": 0.001}, "running"):
                self.assertTrue(observed.wait(timeout=60))
        self.assertGreaterEqual(len(calls), 3)
        self.assertFalse(any(t.name == "e2e-heartbeat" for t in threading.enumerate()))




class ConsumerAcceptance(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='continuous-consumer-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.file = self.root/'status.json'
        self.report_file = self.root/'summary.json'
        self.file.write_text(json.dumps(status()))
        self.report_file.write_text(json.dumps(report()))

    def view(self, head=SHA, now=120):
        return consumer.view(self.file, head=head, now=now)

    def test_local_mirror_and_explicit_summary_reuse_the_same_pure_decision(self):
        expected=summary.consume(report(),status(),head=SHA,now=120)
        self.assertEqual(self.view(),expected)
        other=self.root/'other.json';self.report_file.rename(other)
        self.assertEqual(consumer.view(self.file,head=SHA,now=120,summary_file=other),expected)
        self.file.write_text(json.dumps({'schema_version':1,'status':status(),'summary':report()}))
        self.assertEqual(self.view(),expected)

    def test_no_data_and_disabled_are_distinct(self):
        self.report_file.unlink()
        self.assertEqual(self.view()['check']['result'],'NO_DATA')
        self.file.unlink()
        self.assertEqual(self.view()['check']['result'],'NO_DATA')
        disabled=status();disabled['enabled']=False
        self.file.write_text(json.dumps(disabled))
        self.assertEqual(self.view()['check']['result'],'DISABLED')

    def test_head_stale_future_schema_errors_cannot_turn_green(self):
        self.assertEqual(self.view(head='b'*40)['check']['reason'],'STALE_HEAD')
        stale=status();stale['heartbeat_at']=300
        self.file.write_text(json.dumps(stale))
        self.assertEqual(self.view(now=300)['check']['reason'],'STALE')
        future=status();future['heartbeat_at']=121
        self.file.write_text(json.dumps(future))
        self.assertEqual(self.view()['check']['reason'],'FUTURE_TIMESTAMP')
        self.file.write_text(json.dumps({'schema_version':2,'status':status(),'summary':report()}))
        self.assertEqual(self.view()['check']['result'],'UNKNOWN')

    def test_missing_head_host_failure_null_and_malformed_are_explicit(self):
        self.assertEqual(self.view(head='')['check']['reason'],'HEAD_REQUIRED')
        with mock.patch.object(consumer,'read_json',side_effect=OSError('host offline')):
            self.assertEqual(self.view()['check']['reason'],'HOST_UNAVAILABLE')
        self.report_file.unlink()
        for raw in ('null','{broken'):
            self.file.write_text(raw)
            self.assertEqual(self.view()['check']['result'],'UNKNOWN')

    def test_env_cli_prints_two_lines_and_keeps_read_failures_fail_open(self):
        with mock.patch.dict(os.environ,{'BC_E2E_STATUS_FILE':str(self.file),'BC_E2E_HEAD':SHA}), \
             mock.patch.object(consumer.time,'time',return_value=120):
            output=io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(consumer.main([]),0)
            lines=output.getvalue().splitlines()
            self.assertEqual(len(lines),2)
            self.assertTrue(lines[0].startswith('E2E_CHECK '))
            self.assertIn('result=PASS',lines[0])
            self.assertTrue(lines[1].startswith('E2E_MONITOR '))
            with mock.patch.object(consumer,'read_json',side_effect=PermissionError('no access')), \
                 contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(consumer.main([]),0)

    def test_companion_writes_only_the_secret_free_idempotent_inbox_request(self):
        state=self.root/'state'
        submitted=companion.submit(state,'deploy',SHA,now=120)
        self.assertEqual(submitted['status'],'SUBMITTED')
        self.assertEqual(companion.submit(state,'deploy',SHA,now=121),submitted)
        stored=json.loads((state/'inbox/deploy.json').read_text())
        self.assertEqual(set(stored),{'schema_version','request_id','deploy_sha','submitted_at'})
        self.assertEqual(stored['submitted_at'],120)
        self.assertFalse((state/'runs').exists())
        self.assertFalse((state/'status.json').exists())

    def test_companion_refusal_and_io_failure_leave_deployment_success_unchanged(self):
        self.assertEqual(companion.submit('relative','deploy',SHA)['status'],'REJECTED')
        self.assertEqual(companion.submit(self.root/'state','../unsafe',SHA)['status'],'REJECTED')
        with mock.patch.object(companion.inbox,'submit',side_effect=PermissionError('no access')):
            self.assertEqual(companion.submit(self.root/'state','deploy',SHA)['status'],'REJECTED')
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(companion.main(['--state-dir','relative','--request-id','bad','--deploy-sha','invalid']),0)

    def cli(self, main, argv):
        output=io.StringIO()
        with contextlib.redirect_stdout(output), mock.patch.object(consumer.time,'time',return_value=120):
            self.assertEqual(main(argv),0)
        return output.getvalue().splitlines()

    def test_companion_reports_a_non_private_inbox_as_rejected_with_exit_zero(self):
        state=self.root/'state';inbox_dir=state/'inbox'
        state.mkdir(mode=0o700);inbox_dir.mkdir();inbox_dir.chmod(0o755)
        argv=['--state-dir',str(state),'--request-id','deploy','--deploy-sha',SHA]
        self.assertEqual(json.loads(self.cli(companion.main,argv)[0])['status'],'REJECTED')
        self.assertEqual(sorted(path.name for path in state.rglob('*')),['inbox'])
        inbox_dir.chmod(0o700)
        self.assertEqual(json.loads(self.cli(companion.main,argv)[0])['status'],'SUBMITTED')

    def test_expired_result_of_another_head_still_reports_the_head_mismatch(self):
        late=status();late['heartbeat_at']=300
        self.file.write_text(json.dumps(late))
        same,other=self.view(now=300)['check'],self.view(head='b'*40,now=300)['check']
        self.assertEqual((same['result'],same['reason'],same['stale']),('UNKNOWN','STALE',1))
        self.assertEqual((other['result'],other['reason'],other['stale']),('UNKNOWN','STALE_HEAD',1))
        named=report();named['occurrences'][0].update(status='UNEVALUABLE',reason='STALE_HEAD')
        self.report_file.write_text(json.dumps(named))
        self.assertEqual(self.view(now=300)['check']['reason'],'STALE')

    def test_wrongly_typed_summary_result_prints_unknown_instead_of_a_traceback(self):
        argv=['--status-file',str(self.file),'--head',SHA]
        self.assertIn('result=PASS',self.cli(consumer.main,argv)[0])
        for wrong in ([],{}):
            broken=report();broken['result']=wrong
            for mirror in (False,True):
                with self.subTest(wrong=wrong,mirror=mirror):
                    self.report_file.write_text(json.dumps(broken))
                    self.file.write_text(json.dumps({'schema_version':1,'status':status(),'summary':broken} if mirror else status()))
                    lines=self.cli(consumer.main,argv)
                    self.assertEqual([line.split()[0] for line in lines],['E2E_CHECK','E2E_MONITOR'])
                    self.assertIn('result=UNKNOWN',lines[0]);self.assertIn('reason=MALFORMED_SUMMARY',lines[0])


if __name__ == "__main__":
    unittest.main()
