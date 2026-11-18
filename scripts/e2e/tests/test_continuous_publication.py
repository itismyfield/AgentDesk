"""Scheduled fixture publication and delivery evidence, without operator/provider contact."""
import json
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from scripts.e2e.tests.test_continuous_runner import Runner
from scripts.e2e.continuous import outbox, publication, runner, summary
from scripts.e2e.continuous.store import read_json
from scripts.e2e.tests.test_continuous_summary import report


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
                self.assertTrue(observed.wait(timeout=5))
        self.assertGreaterEqual(len(calls), 3)
        self.assertFalse(any(t.name == "e2e-heartbeat" for t in threading.enumerate()))


del Runner
