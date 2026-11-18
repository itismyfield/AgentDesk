"""Budget and durable completion boundaries, with no live provider contact."""
import copy
import json
import subprocess
import time
from unittest import mock

from scripts.e2e.tests.test_continuous_runner import Runner
from scripts.e2e.continuous import budget, channel_lock, inbox, plan, runner
from scripts.e2e.continuous.store import read_json, write_json


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
        lease.spawn.return_value = subprocess.CompletedProcess([], 1, json.dumps({"status": "FAIL", "native_turns": 1}))
        row = {"scenario": "E-1", "cell": "claude-tui"}
        runner._execute(lease, self.config, {"profile": "default", "run_id": "test", "request": {}}, row,
                        {"turns": 1, "tokens": 10, "time_s": 10})
        self.assertEqual(row["status"], "FAIL")

    def test_unknown_usage_components_do_not_become_zero(self):
        lease = mock.Mock()
        lease.spawn.return_value = subprocess.CompletedProcess([], 0, json.dumps({"status": "PASS", "native_turns": 1,
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
del Runner
