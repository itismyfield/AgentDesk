"""Behavior tests for the restart-safe auto-queue monitor incident state."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
HELPER_PATH = REPO_ROOT / "scripts" / "auto_queue_monitor_state.py"
SPEC = importlib.util.spec_from_file_location("auto_queue_monitor_state", HELPER_PATH)
assert SPEC is not None and SPEC.loader is not None
state_helper = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(state_helper)


def condition(kind: str = "STUCK", suffix: str = "one") -> dict[str, str]:
    return {
        "kind": kind,
        "key": f"{kind}|run-1|entry-{suffix}|stage-1",
        "alert": f"{kind} alert {suffix}",
        "recovery": f"{kind} recovered {suffix}",
    }


class AutoQueueMonitorStateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.state_path = Path(self.tempdir.name) / "monitor-state.json"

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_state_advances_only_after_success_commit(self) -> None:
        active = [condition()]
        first = state_helper.plan_actions(self.state_path, active, 1_000, 1_800)
        self.assertEqual([action["action"] for action in first], ["alert"])
        self.assertFalse(self.state_path.exists())

        # A failed HTTP send means no commit. The next process/run must retry.
        retry = state_helper.plan_actions(self.state_path, active, 1_001, 1_800)
        self.assertEqual([action["action"] for action in retry], ["alert"])

        self.assertTrue(state_helper.commit_action(self.state_path, retry[0]))
        persisted = json.loads(self.state_path.read_text(encoding="utf-8"))
        entry = persisted["conditions"][active[0]["key"]]
        self.assertEqual(entry["last_alert_at"], 1_001)

    def test_cooldown_is_at_least_thirty_minutes_and_per_instance(self) -> None:
        first_condition = condition(suffix="one")
        first = state_helper.plan_actions(
            self.state_path, [first_condition], 10_000, 1
        )
        self.assertTrue(state_helper.commit_action(self.state_path, first[0]))

        self.assertEqual(
            state_helper.plan_actions(
                self.state_path, [first_condition], 11_799, 1
            ),
            [],
        )
        boundary = state_helper.plan_actions(
            self.state_path, [first_condition], 11_800, 1
        )
        self.assertEqual([action["action"] for action in boundary], ["alert"])

        # A distinct condition instance is not suppressed by the first key.
        second_condition = condition(suffix="two")
        mixed = state_helper.plan_actions(
            self.state_path, [first_condition, second_condition], 10_100, 1
        )
        self.assertEqual(
            [action["condition"]["key"] for action in mixed],
            [second_condition["key"]],
        )

    def test_resolution_emits_exactly_one_recovery_after_success(self) -> None:
        active = [condition(kind="ANOMALY")]
        alert = state_helper.plan_actions(self.state_path, active, 2_000, 1_800)[0]
        self.assertTrue(state_helper.commit_action(self.state_path, alert))

        recovery = state_helper.plan_actions(self.state_path, [], 2_010, 1_800)
        self.assertEqual([action["action"] for action in recovery], ["recovery"])
        # Failed recovery send is retried because the state was not committed.
        self.assertEqual(
            [
                action["action"]
                for action in state_helper.plan_actions(
                    self.state_path, [], 2_011, 1_800
                )
            ],
            ["recovery"],
        )
        self.assertTrue(state_helper.commit_action(self.state_path, recovery[0]))
        self.assertEqual(state_helper.plan_actions(self.state_path, [], 2_012, 1_800), [])

    def test_malformed_state_fails_closed_without_fake_recovery(self) -> None:
        active = [condition(kind="REVIEW_LONG")]
        self.state_path.write_text("{not-json", encoding="utf-8")

        self.assertEqual(
            state_helper.plan_actions(self.state_path, active, 3_000, 1_800), []
        )
        quarantined = list(self.state_path.parent.glob("monitor-state.json.corrupt.*"))
        self.assertEqual(len(quarantined), 1)
        persisted = json.loads(self.state_path.read_text(encoding="utf-8"))
        entry = persisted["conditions"][active[0]["key"]]
        self.assertIsNone(entry["last_alert_at"])
        self.assertEqual(entry["suppress_until"], 4_800)

        # It was never announced, so disappearance must not claim recovery.
        self.assertEqual(state_helper.plan_actions(self.state_path, [], 3_100, 1_800), [])

        # If still active, it becomes alertable only at the fail-closed window.
        self.state_path.write_text("{bad-again", encoding="utf-8")
        state_helper.plan_actions(self.state_path, active, 5_000, 1_800)
        self.assertEqual(
            state_helper.plan_actions(self.state_path, active, 6_799, 1_800), []
        )
        actions = state_helper.plan_actions(self.state_path, active, 6_800, 1_800)
        self.assertEqual([action["action"] for action in actions], ["alert"])


if __name__ == "__main__":
    unittest.main()
