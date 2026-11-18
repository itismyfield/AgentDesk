"""Persistent notification delivery using fixture transports only."""
import copy
import json
from pathlib import Path
import tempfile
import unittest

from scripts.e2e.continuous import outbox


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


if __name__ == "__main__":
    unittest.main()
