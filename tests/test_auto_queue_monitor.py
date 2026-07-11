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

    def test_pending_action_id_is_persisted_before_enqueue_and_reused(self) -> None:
        active = [condition()]
        first = state_helper.plan_actions(self.state_path, active, 1_000, 1_800)
        self.assertEqual([action["action"] for action in first], ["alert"])
        action_id = first[0]["action_id"]
        self.assertEqual(len(action_id), 32)
        persisted = json.loads(self.state_path.read_text(encoding="utf-8"))
        self.assertEqual(persisted["pending_action"]["action_id"], action_id)

        # A failed HTTP enqueue or a crash before commit retries the exact ID.
        retry = state_helper.plan_actions(self.state_path, active, 3_001, 1_800)
        self.assertEqual([action["action"] for action in retry], ["alert"])
        self.assertEqual(retry[0]["action_id"], action_id)
        self.assertEqual(retry[0]["condition"], first[0]["condition"])
        self.assertEqual(retry[0]["expected_last_alert_at"], None)
        self.assertEqual(retry[0]["now"], 3_001)

        self.assertTrue(state_helper.commit_action(self.state_path, retry[0]))
        persisted = json.loads(self.state_path.read_text(encoding="utf-8"))
        entry = persisted["conditions"][active[0]["key"]]
        self.assertEqual(entry["last_alert_at"], 3_001)
        self.assertIsNone(persisted["pending_action"])
        self.assertEqual(
            state_helper.plan_actions(self.state_path, active, 3_002, 1_800),
            [],
            "a long enqueue outage must not immediately consume the cooldown after commit",
        )

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
        self.assertTrue(state_helper.commit_action(self.state_path, boundary[0]))

        # A distinct condition instance is not suppressed by the first key.
        second_condition = condition(suffix="two")
        mixed = state_helper.plan_actions(
            self.state_path, [first_condition, second_condition], 11_801, 1
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
        # Failed recovery enqueue is retried with the same durable action ID.
        retry = state_helper.plan_actions(self.state_path, [], 2_011, 1_800)
        self.assertEqual(retry[0]["action_id"], recovery[0]["action_id"])
        self.assertEqual(retry[0]["now"], 2_011)
        self.assertTrue(state_helper.commit_action(self.state_path, retry[0]))
        self.assertEqual(state_helper.plan_actions(self.state_path, [], 2_012, 1_800), [])

    def test_malformed_state_realerts_without_consuming_incident(self) -> None:
        active = [condition(kind="REVIEW_LONG")]
        self.state_path.write_text("{not-json", encoding="utf-8")

        actions = state_helper.plan_actions(self.state_path, active, 3_000, 1_800)
        self.assertEqual([action["action"] for action in actions], ["alert"])
        quarantined = list(self.state_path.parent.glob("monitor-state.json.corrupt.*"))
        self.assertEqual(len(quarantined), 1)
        persisted = json.loads(self.state_path.read_text(encoding="utf-8"))
        self.assertEqual(persisted["pending_action"]["action_id"], actions[0]["action_id"])

        # A failed enqueue retains the action identity and retries it.
        self.assertEqual(
            state_helper.plan_actions(self.state_path, active, 3_001, 1_800)[0]["action_id"],
            actions[0]["action_id"],
        )

        # Only a successful delivery commit creates cooldown/recovery state.
        retry = state_helper.plan_actions(self.state_path, active, 3_001, 1_800)
        self.assertTrue(state_helper.commit_action(self.state_path, retry[0]))
        recovery = state_helper.plan_actions(self.state_path, [], 3_100, 1_800)
        self.assertEqual([action["action"] for action in recovery], ["recovery"])

    def test_unknown_detector_state_preserves_alert_without_false_recovery(self) -> None:
        active = [condition(kind="STUCK")]
        alert = state_helper.plan_actions(self.state_path, active, 4_000, 1_800)[0]
        self.assertTrue(state_helper.commit_action(self.state_path, alert))

        self.assertEqual(
            state_helper.plan_actions(
                self.state_path,
                [],
                4_100,
                1_800,
                [active[0]["key"]],
            ),
            [],
        )
        persisted = json.loads(self.state_path.read_text(encoding="utf-8"))
        self.assertIn(active[0]["key"], persisted["conditions"])

        recovery = state_helper.plan_actions(self.state_path, [], 4_101, 1_800)
        self.assertEqual([action["action"] for action in recovery], ["recovery"])

    def test_unknown_new_condition_neither_alerts_nor_creates_state(self) -> None:
        active = [condition(kind="ANOMALY")]
        self.assertEqual(
            state_helper.plan_actions(
                self.state_path,
                active,
                5_000,
                1_800,
                [active[0]["key"]],
            ),
            [],
        )
        self.assertFalse(self.state_path.exists())


if __name__ == "__main__":
    unittest.main()
