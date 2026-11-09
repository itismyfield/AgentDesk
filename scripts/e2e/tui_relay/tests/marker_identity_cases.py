"""Marker namespacing and wrong-run diagnostics through the real YAML driver."""

from __future__ import annotations

import copy
import re
import sys
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import post_deploy_turn_smoke as smoke
import run_tui_relay as driver
from tui_relay import assertions, wait_timeout
from tui_relay import test_post_deploy_turn_smoke as fixtures


class MarkerIdentity(unittest.TestCase):
    RUNS = ("20261109T133911Z-72704-E-50-claude-tui", "20261109T153810Z-25439-E-50-claude-tui")

    def test_similar_run_ids_have_distinct_front_tokens(self):
        markers = [smoke.expected_markers(fixtures.SCENARIOS, "E-50", run)[0] for run in self.RUNS]
        self.assertNotEqual(*(marker.split(":")[2][:12] for marker in markers))
        for marker, run in zip(markers, self.RUNS):
            self.assertRegex(marker, rf"^\[E2E:E50:[0-9a-f]{{12}}-{re.escape(run)}:T1\]$")
            self.assertEqual(wait_timeout.marker_identity(run), wait_timeout.marker_identity(run))

    def test_real_yaml_prompt_wait_assertions_counts_and_judge_agree(self):
        for scenario_id, cell in (("E-50", "claude-tui"), ("E-51", "codex-tui")):
            for run in self.RUNS:
                with self.subTest(scenario=scenario_id, run=run):
                    original = next(s for s in driver.load_scenarios(fixtures.SCENARIOS, cell=cell)
                                    if s["id"] == scenario_id)
                    preserved = copy.deepcopy(original)
                    client = fixtures.LiveShapedDiscord(provider=driver.cell_provider(cell))
                    expected = smoke.expected_markers(fixtures.SCENARIOS, scenario_id, run)
                    accepted = []
                    real_wait = driver.wait_for_discord_text_with_tui_idle_draft_guard

                    def wait(**kwargs):
                        accepted.append(kwargs["needle"])
                        return real_wait(**kwargs)

                    with patch.object(fixtures, "RUN_ID", run), \
                         patch.object(fixtures, "LiveShapedDiscord", return_value=client), \
                         patch.object(driver, "wait_for_discord_text_with_tui_idle_draft_guard", side_effect=wait):
                        result = fixtures.run_live_shaped(scenario_id, cell)
                    self.assertEqual(result["status"], "pass", result.get("reason"))
                    self.assertEqual(result["run_id"], run)
                    self.assertEqual(result["marker_counts"], dict.fromkeys(expected, 1))
                    for marker in expected:
                        self.assertRegex(marker, rf"^\[E2E:E5[01]:[0-9a-f]{{12}}-{re.escape(run)}:")
                        prompt = next(m for m in client.messages
                                      if assertions.is_our_send(m) and marker in m["content"])
                        self.assertIn(marker, prompt["content"])
                        self.assertTrue(any(marker in str(item["spec"]) for item in result["assertions"]))
                    self.assertEqual(accepted, expected)
                    report = {"run_id": run, "cell": cell, "channel_id": fixtures.CHANNELS[cell],
                              "coverage_class_violations": [], "scenarios": [result]}
                    outcome = smoke.classify(report=report, scenario_id=scenario_id, cell=cell,
                                             channel_id=fixtures.CHANNELS[cell], run_id=run, driver_rc=0,
                                             markers=expected, log_counts={"cell_lines": 1})
                    self.assertEqual(outcome["classification"], "PASS", outcome)
                    self.assertEqual(original, preserved)

    def test_expansion_preserves_non_marker_ids_and_does_not_mutate_templates(self):
        value = {"origin": "raw-{run_id}", "steps": ["[E2E:X:{run_id}:T1]"], "flag": True}
        original = copy.deepcopy(value)
        expanded = wait_timeout.expand_markers(value, self.RUNS[0])
        self.assertEqual(value, original)
        self.assertEqual(expanded["origin"], "raw-{run_id}")
        self.assertIs(expanded["flag"], True)
        self.assertEqual(wait_timeout.expand_markers(expanded, self.RUNS[0]), expanded)


class MarkerMismatch(unittest.TestCase):
    EXPECTED = "[E2E:E50:1a2b3c4d5e6f-20261109T153810Z-25439-E-50-claude-tui:T1]"
    ACTUAL = "[E2E:E50:20261109T153810Z-72704-E-50-claude-tui:T1]"

    def diagnostic(self, content=None, *, message_id="11", author=None, health=None, notice=False):
        window = assertions.Window(setup_marker_id="1")
        window.add({"id": "10", "content": self.EXPECTED,
                    "author": {"id": assertions.OUR_BOT_ID, "bot": True}})
        if notice:
            window.add({"id": "12", "content": "-# ⌨️ prompt sent", "author": {"id": "notify-bot", "bot": True}})
        if content is not None:
            window.add({"id": message_id, "content": content,
                        "author": {"id": author or assertions.provider_bot_id("claude"), "bot": True}})
        with patch.object(driver, "_target_mailbox_debug_snapshot", return_value=health or {
                "target_mailboxes": [{"busy_reasons": [], "queue_depth": 0}]}), \
             patch.object(driver.tmux, "capture_pane", return_value=""):
            return driver._collect_wait_timeout_diagnostics(
                base_url="http://offline.invalid", channel_id="42", cell="claude-tui",
                scenario={"id": "E-50"}, thread_channel_id=None, after_id="10",
                wait_kind="relay", needle=self.EXPECTED, prompt=None, window=window)

    def test_old_run_response_records_expected_and_actual_markers(self):
        diagnostic = self.diagnostic(self.ACTUAL)
        self.assertEqual(diagnostic["classification"], "marker_mismatch")
        self.assertEqual(diagnostic["expected_marker"], self.EXPECTED)
        self.assertEqual(diagnostic["actual_markers"], [self.ACTUAL])
        attribution = driver._failure_attribution("assertion", "timeout", record={"wait_timeouts": [diagnostic]})
        self.assertEqual(attribution["wait_timeout_classifications"], ["marker_mismatch"])

    def test_no_response_keeps_existing_classification(self):
        diagnostic = self.diagnostic()
        self.assertEqual(diagnostic["classification"], "no_direct_input_or_relay_evidence")
        self.assertEqual(diagnostic["actual_markers"], [])

    def test_direct_input_notice_does_not_hide_wrong_run_body(self):
        self.assertEqual(self.diagnostic(self.ACTUAL, notice=True)["classification"], "marker_mismatch")
        self.assertEqual(self.diagnostic(self.ACTUAL, notice=True, health={"target_mailboxes": [
            {"busy_reasons": ["relay active"]}]} )["classification"], "direct_input_notified_provider_or_relay_still_busy")

    def test_wrong_run_response_survives_driver_report_collection(self):
        scenario = next(s for s in driver.load_scenarios(fixtures.SCENARIOS, cell="claude-tui") if s["id"] == "E-50")

        class WrongRunDiscord(fixtures.LiveShapedDiscord):
            def _post(self, content, *, author):
                if author == self.provider_bot:
                    content = re.sub(r"(\[E2E:E50:)[^:\]]+(:T1\])", r"\g<1>previous-run\2", content)
                return super()._post(content, author=author)

        args = Namespace(cell="claude-tui", channel_id="42", dry_run=False, reset_before_each=False,
                         hard_reset_session_each=False, allow_destructive=False, final_refetches=1,
                         final_refetch_interval_s=0, required_agent_mode="real_live", required_coverage_class="live")
        clock = [0.0]
        with patch.object(driver.time, "sleep", side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)), \
             patch.object(driver.time, "monotonic", side_effect=lambda: clock[0]), \
             patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}), \
             patch.object(driver, "_raise_if_tui_prompt_stuck_while_idle"), \
             patch.object(driver, "_target_mailbox_debug_snapshot", return_value={"target_mailboxes": [{"queue_depth": 0}]}), \
             patch.object(driver.tmux, "capture_pane", return_value=""):
            result = driver.run_scenario(scenario, args=args, client=WrongRunDiscord(), run_id=fixtures.RUN_ID)
        self.assertEqual(result["status"], "fail", result)
        self.assertEqual(result["run_id"], fixtures.RUN_ID)
        diagnostic = result["wait_timeouts"][0]
        self.assertEqual(diagnostic["classification"], "marker_mismatch")
        self.assertEqual(diagnostic["expected_marker"], smoke.expected_markers(fixtures.SCENARIOS, "E-50", fixtures.RUN_ID)[0])
        self.assertEqual(diagnostic["actual_markers"], ["[E2E:E50:previous-run:T1]"])
        self.assertIn("marker_mismatch", result["failure_attribution"]["wait_timeout_classifications"])
        self.assertIn("previous-run", result["reason"])

    def test_other_scenario_step_or_expected_run_is_not_mismatch(self):
        for content in (self.ACTUAL.replace(":E50:", ":E51:"),
                        self.ACTUAL.replace(":T1]", ":T2]"), self.EXPECTED, "[E2E:E50:broken]"):
            with self.subTest(content=content):
                diagnostic = self.diagnostic(content)
                self.assertEqual(diagnostic["classification"], "no_direct_input_or_relay_evidence")
                self.assertEqual(diagnostic["actual_markers"], [])

    def test_messages_at_or_before_after_id_are_not_mismatch(self):
        for message_id in ("2", "9", "10", "not-a-snowflake"):
            with self.subTest(message_id=message_id):
                diagnostic = self.diagnostic(self.ACTUAL, message_id=message_id)
                self.assertEqual(diagnostic["classification"], "no_direct_input_or_relay_evidence")
                self.assertEqual(diagnostic["actual_markers"], [])

    def test_prompt_echo_chrome_and_other_authors_are_not_mismatch(self):
        cases = [(self.ACTUAL, assertions.OUR_BOT_ID), (self.ACTUAL, "notify-bot"),
                 (self.ACTUAL, assertions.provider_bot_id("codex")),
                 ("-# ⌨️ " + self.ACTUAL, None), ("✅ " + self.ACTUAL, None)]
        for content, author in cases:
            with self.subTest(content=content, author=author):
                diagnostic = self.diagnostic(content, author=author)
                self.assertNotEqual(diagnostic["classification"], "marker_mismatch")
                self.assertEqual(diagnostic["actual_markers"], [])

    def test_queue_block_retains_its_classification(self):
        diagnostic = self.diagnostic(self.ACTUAL, health={"target_mailboxes": [{"relay_stall_state": "queue_blocked"}]})
        self.assertEqual(diagnostic["classification"], "turn_not_started_queue_blocked")

    def test_actual_markers_are_complete_and_unique(self):
        second = self.ACTUAL.replace("-72704-", "-99999-")
        diagnostic = self.diagnostic(f"{second}\n{self.ACTUAL}\n{second}")
        self.assertEqual(diagnostic["classification"], "marker_mismatch")
        self.assertEqual(diagnostic["actual_markers"], sorted([self.ACTUAL, second]))

    def test_mismatch_remains_a_smoke_failure(self):
        result = fixtures.run_live_shaped("E-50", "claude-tui")
        result.update(status="fail", reason="marker wait timed out", wait_timeouts=[self.diagnostic(self.ACTUAL)])
        result["marker_counts"] = dict.fromkeys(result["marker_counts"], 0)
        passed, detail = fixtures.verdict(result, "E-50", "claude-tui", "turns-clean.dcserver.log")
        self.assertFalse(passed)
        self.assertIn("T1 body not delivered", detail)


if __name__ == "__main__":
    unittest.main()
