"""Offline consumer/fetch tests: synthetic capture clocks, never live proof."""

import copy
from datetime import datetime, timezone
from argparse import Namespace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_tui_relay as driver
from tui_relay import assertions, discord, known_gap as gap
from tui_relay.test_discord_client import _Response

RUN = "offline-c5"
BASE = datetime(2026, 1, 1, tzinfo=timezone.utc).timestamp()
SPEC = {"no_duplicate_marker_with_known_gap": {"marker": gap.PRE, "known_gap": gap.PROFILE}}
BINDING = dict(scenario="E-22", cell="claude-tui", channel_id=gap.CHANNEL_ID,
               bot_id=gap.BOT_ID, after_id="99")
YAML = Path(__file__).resolve().parents[3] / "tests/e2e/tui_relay/scenarios/E-22-tool-use-text-completeness.yaml"


def row(mid, content, *, edited=None, author=gap.BOT_ID):
    return {"id": str(mid), "content": content, "channel_id": gap.CHANNEL_ID,
            "author": {"id": author, "bot": True}, "type": 0,
            "timestamp": datetime.fromtimestamp(BASE + mid - 100, timezone.utc).isoformat(),
            "edited_timestamp": edited, "attachments": [], "embeds": [], "components": []}


def fixture():
    setup = row(100, f"### E2E SETUP E-22 cell=claude-tui run={RUN}", author=gap.SETUP_BOT_ID)
    before = row(101, gap.PRE + "\n\n⠋ ⚙ Bash: Sleep 20 seconds then print marker: " + gap.COMMAND + "\n• [Bash] 실행")
    after = row(101, gap.PRE, edited=datetime.fromtimestamp(BASE + 4, timezone.utc).isoformat())
    body, chrome = row(103, gap.BODY), row(104, "-# ✅ 완료")
    return [{"pages": [{"channel_id": gap.CHANNEL_ID, "after_id": "99", "limit": 100,
                        "observed_at": BASE + 10 + index, "messages": messages}]}
            for index, messages in enumerate(([setup], [setup, before], [setup, after, body, chrome]))]


def pending_fixture():
    captures = fixture()
    for capture, observed in zip(captures, (BASE + 0.5, BASE + 2, BASE + 4.1)):
        capture["pages"][0]["observed_at"] = observed
    captures[-1]["pages"][0]["messages"][1] = copy.deepcopy(captures[1]["pages"][0]["messages"][1])
    return captures


def consumer(captures, *, binding=None, spec=None, final=None):
    window = assertions.Window(setup_marker_id="100")
    for capture in captures or []:
        for message in capture.get("pages", [{}])[0].get("messages", []):
            if int(message["id"]) > 100:
                window.add(copy.deepcopy(message))
    if final is not None:
        window = assertions.Window(setup_marker_id="100")
        for message in final:
            window.add(copy.deepcopy(message))
    record = {"_known_gap_captures": captures, "_known_gap_binding": BINDING if binding is None else binding}
    driver.run_assertion(SPEC if spec is None else spec, window=window, record=record, run_id=RUN)
    return window, record


class E22KnownGapContract(unittest.TestCase):
    def setUp(self):
        for target in ("socket.socket", "subprocess.Popen"):
            blocker = patch(target, side_effect=AssertionError("offline test attempted external I/O"))
            blocker.start()
            self.addCleanup(blocker.stop)

    def test_unique_marker_needs_no_exception_or_binding(self):
        window, record = consumer(None, final=[row(101, gap.BODY)])
        self.assertNotIn("known_gaps", record)
        self.assertEqual(len(window.messages), 1)

    def test_pending_is_bound_original_preview_transition(self):
        decision = gap.evaluate_e22_known_gap(pending_fixture(), run_id=RUN, **BINDING)
        self.assertEqual(decision["classification"], "PENDING")
        self.assertEqual(decision["message_ids"], ["101", "103"])
        self.assertEqual(decision["deadline_at"], BASE + 8)

    def test_pending_deadline_is_not_extended_by_late_first_capture(self):
        captures = pending_fixture()
        captures[-1]["pages"][0]["observed_at"] = BASE + 7.9
        self.assertEqual(gap.evaluate_e22_known_gap(captures, run_id=RUN, **BINDING)["classification"], "PENDING")
        captures[-1]["pages"][0]["observed_at"] = BASE + 8
        self.assertEqual(gap.evaluate_e22_known_gap(captures, run_id=RUN, **BINDING)["classification"], "FAIL")

    def _pending_pipeline(self, *, resolve_on=None, initial=4.1, late=False, third=False,
                          restored=False, completion=True, recheck_change=None, change_on=1):
        pending = pending_fixture()
        final = copy.deepcopy(fixture()[-1]["pages"][0]["messages"])
        final[1]["edited_timestamp"] = datetime.fromtimestamp(BASE + 4.5, timezone.utc).isoformat()
        pages = [c["pages"][0]["messages"] for c in pending]
        clock, requests, sleeps = [BASE + initial], [], []
        def fetch(request, **_kwargs):
            index = len(requests)
            requests.append(clock[0])
            self.assertIn("after=99", request.full_url)
            if index >= 5 and late:
                clock[0] += 3
            messages = copy.deepcopy(pages[min(index, 2)])
            if (resolve_on is not None and index >= 4 + resolve_on) or (restored and index == 3):
                messages = copy.deepcopy(final)
                if restored:
                    messages[1]["edited_timestamp"] = datetime.fromtimestamp(BASE + 4, timezone.utc).isoformat()
            if restored and index >= 4:
                messages[1]["edited_timestamp"] = datetime.fromtimestamp(BASE + 4.05, timezone.utc).isoformat()
            if third and index >= 5:
                messages.append(row(102, gap.PRE))
            if not completion:
                messages = [m for m in messages if m["id"] != "104"]
            if index >= 4 + change_on and recheck_change == "completion":
                chrome = next(m for m in messages if m["id"] == "104")
                chrome.update(content="ordinary status text", edited_timestamp=
                    datetime.fromtimestamp(BASE + 4.7, timezone.utc).isoformat())
            if index >= 4 + change_on and recheck_change == "raw_count":
                for mid in range(105, 141):
                    extra = row(mid, f"unique ordinary filler {mid}")
                    extra["timestamp"] = datetime.fromtimestamp(BASE + 4.9, timezone.utc).isoformat()
                    messages.append(extra)
            return _Response(messages)
        def sleep(seconds):
            if len(requests) >= 5:
                sleeps.append(seconds)
                clock[0] += seconds
        original_mark = assertions.Window.mark_prompt_sent
        def mark(window):
            original_mark(window, datetime.fromtimestamp(BASE + 0.5, timezone.utc))
        with patch("urllib.request.urlopen", side_effect=fetch), patch.object(driver.time, "sleep", side_effect=sleep), \
             patch.object(driver.time, "time", side_effect=lambda: clock[0]), \
             patch.object(driver.time, "monotonic", side_effect=lambda: clock[0] - BASE), \
             patch.object(discord.DiscordClient, "send_control", return_value={"id": "100"}), \
             patch.object(discord.DiscordClient, "send_prompt", return_value={"message_id": "102"}), \
             patch.object(driver, "wait_for_provider_hold_state", return_value={"ok_marker": gap.PRE, "ok_marker_seen": True}), \
             patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}), \
             patch.object(assertions.Window, "mark_prompt_sent", mark):
            try:
                record = driver.run_one_cell(scenario=yaml.safe_load(YAML.read_text()), cell="claude-tui",
                    channel_id=gap.CHANNEL_ID, client=discord.DiscordClient("http://offline.invalid"),
                    run_id=RUN, dry_run=False, args=Namespace(queue_runtime_root="/offline-denied"))
            except driver.ScenarioStepAssertionError as error:
                return error.record, error, requests, sleeps
        return record, None, requests, sleeps

    def test_pending_resolves_with_one_or_two_existing_client_refetches(self):
        for attempt in (1, 2):
            with self.subTest(attempt=attempt):
                record, error, requests, sleeps = self._pending_pipeline(resolve_on=attempt)
                self.assertIsNone(error, str(error))
                trace = record["known_gap_rechecks"][0]
                self.assertEqual(trace["refetches"], attempt)
                self.assertEqual(trace["outcome"], "KNOWN_GAP")
                self.assertEqual([d["classification"] for d in trace["decisions"]], ["PENDING"] * attempt + ["KNOWN_GAP"])
                self.assertEqual(len(requests), 5 + attempt)
                self.assertEqual(sleeps, [1.0] * attempt)
                self.assertEqual([round(t - (BASE + 4.1), 2) for t in requests[5:]], list(range(1, attempt + 1)))
                result = {"assertions": []}
                driver._merge_record_into_result(result, record)
                self.assertEqual(result["known_gap_rechecks"], record["known_gap_rechecks"])

    def _assert_recheck_invalidates_prior_assertion(self, change, name, reason):
        for resolve_on, change_on in ((1, 1), (2, 1), (2, 2), (None, 1), (None, 2)):
            with self.subTest(resolve_on=resolve_on, change_on=change_on):
                record, error, requests, _ = self._pending_pipeline(
                    resolve_on=resolve_on, recheck_change=change, change_on=change_on)
                self.assertIsInstance(error, driver.ScenarioStepAssertionError)
                self.assertIn(reason, str(error))
                self.assertEqual(len(requests), 5 + change_on)
                trace = record["known_gap_rechecks"][0]
                self.assertEqual((trace["refetches"], trace["outcome"]), (change_on, "FAIL"))
                self.assertNotIn("known_gaps", record)
                self.assertTrue(any(name in item["spec"] and item["passed"] for item in record["assertions"]))
                revalidated = record["revalidated_after_recheck"]
                self.assertEqual(len(revalidated), change_on)
                self.assertTrue(all(item["passed"] for item in revalidated[:-1]))
                self.assertFalse(revalidated[-1]["passed"])
                self.assertEqual(revalidated[-1]["failed_assertion"], name)
                if change == "raw_count":
                    self.assertEqual(record["raw_count"], 39)
                result = {"assertions": []}
                driver._merge_record_into_result(result, record)
                self.assertEqual(result["revalidated_after_recheck"], revalidated)

    def test_recheck_overwritten_completion_fails_scenario(self):
        self._assert_recheck_invalidates_prior_assertion(
            "completion", "completion_chrome_after_body", "completion chrome not found")

    def test_recheck_raw_count_overflow_fails_scenario(self):
        self._assert_recheck_invalidates_prior_assertion(
            "raw_count", "raw_message_count_between_markers", "raw message count 39 outside [1, 36]")

    def test_normal_rechecks_revalidate_every_prior_assertion_and_report(self):
        prefix = []
        for spec in yaml.safe_load(YAML.read_text())["assertions"]:
            if "no_duplicate_marker_with_known_gap" in spec:
                break
            prefix.append(spec)
        self.assertEqual(len(prefix), 10)
        for attempt in (1, 2):
            with self.subTest(attempt=attempt):
                record, error, requests, _ = self._pending_pipeline(resolve_on=attempt)
                self.assertIsNone(error, str(error))
                self.assertEqual(len(requests), 5 + attempt)
                self.assertEqual(record.get("revalidated_after_recheck"),
                                 [{"assertions": prefix, "passed": True}] * attempt)
                result = {"assertions": []}
                driver._merge_record_into_result(result, record)
                self.assertEqual(result["revalidated_after_recheck"], record["revalidated_after_recheck"])

    def test_pending_refetch_budget_and_deadline_never_extend(self):
        for options, expected in (({}, 2), ({"initial": 7.5}, 0), ({"resolve_on": 1, "late": True}, 1)):
            with self.subTest(options=options):
                record, error, requests, _ = self._pending_pipeline(**options)
                self.assertIsNotNone(error)
                trace = record["known_gap_rechecks"][0]
                self.assertEqual((trace["refetches"], trace["outcome"]), (expected, "FAIL"))
                self.assertEqual(trace["deadline_at"], BASE + 8)
                self.assertEqual(len(requests), 5 + expected)
                self.assertNotIn("known_gaps", record)
                if options:
                    self.assertEqual(trace["decisions"][-1]["reason"], "pending_expired")

    def test_restored_preview_or_third_id_cannot_use_pending_grace(self):
        for options, count in (({"restored": True}, 0), ({"third": True}, 1)):
            with self.subTest(options=options):
                record, error, requests, _ = self._pending_pipeline(**options)
                self.assertIsNotNone(error)
                self.assertEqual(len(requests), 5 + count)
                self.assertNotIn("known_gaps", record)

    def test_required_completion_failure_precedes_pending_refetch(self):
        record, error, requests, sleeps = self._pending_pipeline(resolve_on=1, completion=False)
        self.assertIn("completion chrome not found", str(error))
        self.assertEqual(len(requests), 5)
        self.assertEqual(sleeps, [])
        self.assertNotIn("known_gap_rechecks", record)
        self.assertNotIn("revalidated_after_recheck", record)

    def test_pending_still_requires_all_identity_and_raw_guards(self):
        for name in ("author", "channel", "setup", "history_third", "same_body", "new_edit", "missing"):
            captures = pending_fixture()
            rows = captures[-1]["pages"][0]["messages"]
            if name == "author": rows[1]["author"]["id"] = "999"
            if name == "channel": rows[1]["channel_id"] = "999"
            if name == "setup": rows[0]["content"] += "-wrong"
            if name == "history_third": captures[1]["pages"][0]["messages"].append(row(102, gap.PRE))
            if name == "same_body": rows[1]["content"] = gap.BODY
            if name == "new_edit": rows[2]["edited_timestamp"] = rows[3]["timestamp"]
            if name == "missing": captures = None
            with self.subTest(name=name):
                self.assertIn(gap.evaluate_e22_known_gap(captures, run_id=RUN, **BINDING)["classification"], {"FAIL", "NOT_EVALUABLE"})

    def test_observed_edit_is_reported_and_original_guards_remain(self):
        window, record = consumer(fixture())
        decision = record["known_gaps"][0]
        self.assertEqual((decision["classification"], decision["known_gap"]), ("KNOWN_GAP", "#5731"))
        self.assertEqual(decision["message_ids"], ["101", "103"])
        self.assertEqual(decision["witness"]["after"], gap.PRE)
        self.assertEqual(len(window.message_updates), 1)
        with self.assertRaises(assertions.AssertionError):
            assertions.no_duplicate_marker(window, marker=gap.PRE)
        record["provider_hold_states"] = [{"ok_marker": gap.PRE, "ok_marker_seen": True}]
        for spec in yaml.safe_load(YAML.read_text())["assertions"]:
            driver.run_assertion(spec, window=window, record=record, run_id=RUN)
        result = {"assertions": []}
        driver._merge_record_into_result(result, record)
        self.assertEqual(result["known_gaps"], record["known_gaps"])
        self.assertNotIn("_known_gap_captures", result)
        window.raw_messages = [m for m in window.raw_messages if m["id"] != "104"]
        with self.assertRaises(assertions.AssertionError):
            assertions.completion_chrome_after_body(window, body_marker=gap.MARKERS[-1], required=True)

    def test_closed_dynamic_preview_alphabet(self):
        for spinner in "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏":
            for caption in ("", "Wait for E22: ", "도구 완료 기다리기: ", "a" * 45 + ": "):
                captures = fixture()
                preview = gap.PRE + "\n\n" + spinner + " ⚙ Bash: " + caption + gap.COMMAND + "\n• [Bash] 실행 · 2회"
                with self.subTest(preview=preview):
                    captures[1]["pages"][0]["messages"][1]["content"] = preview
                    consumer(captures)
        for caption in ("a" * 46, "가" * 16, "line\nbreak", "`injected`"):
            captures = fixture()
            captures[1]["pages"][0]["messages"][1]["content"] = gap.PRE + "\n\n⠋ ⚙ Bash: " + caption + ": " + gap.COMMAND + "\n• [Bash] 실행"
            with self.subTest(caption=caption), self.assertRaises(assertions.AssertionError):
                consumer(captures)

    def test_duplicate_counterexamples_refuse_in_actual_consumer(self):
        cases = {}
        for name in ("unedited", "missing_before", "retained_preview", "third_current", "third_history",
                     "same_body", "wrong_author", "wrong_channel", "unsupported_preview", "footer",
                     "double_token", "backward_edit", "future_edit", "changed_creation", "wrong_setup"):
            captures = copy.deepcopy(fixture())
            before = captures[1]["pages"][0]["messages"][1]
            final = captures[-1]["pages"][0]["messages"]
            if name == "unedited": final[1]["edited_timestamp"] = None
            if name == "missing_before": captures.pop(1)
            if name == "retained_preview": final[1]["content"] = before["content"]
            if name == "third_current": final.append(row(105, gap.PRE))
            if name == "third_history": captures[1]["pages"][0]["messages"].append(row(105, gap.PRE))
            if name == "same_body": final[1]["content"] = gap.BODY
            if name == "wrong_author": before["author"]["id"] = "999"
            if name == "wrong_channel": before["channel_id"] = "999"
            if name == "unsupported_preview": before["content"] = gap.PRE + " arbitrary progress"
            if name == "footer": final[2]["content"] += "\n\n-# ✅ 완료"
            if name == "double_token": final[2]["content"] += gap.PRE
            if name == "backward_edit": before["edited_timestamp"] = final[1]["edited_timestamp"]
            if name == "future_edit": final[1]["edited_timestamp"] = "2099-01-01T00:00:00+00:00"
            if name == "changed_creation": before["timestamp"] = "2026-01-01T00:00:00+00:00"
            if name == "wrong_setup": final[0]["content"] += "-other"
            cases[name] = captures
        for name, captures in cases.items():
            with self.subTest(case=name), self.assertRaises(assertions.AssertionError):
                consumer(captures)

    def test_missing_binding_capture_and_wrong_window_refuse(self):
        final = fixture()[-1]["pages"][0]["messages"][1:]
        for captures, binding in ((None, {}), ([], {}), (fixture(), {})):
            with self.subTest(captures=captures, binding=binding), self.assertRaises(assertions.AssertionError):
                consumer(captures, binding=binding, final=final)
        for key, value in (("cell", "claude-pipe"), ("scenario", "E-1"), ("channel_id", "999"),
                           ("bot_id", "999"), ("after_id", "98")):
            with self.subTest(key=key), self.assertRaises(assertions.AssertionError):
                consumer(fixture(), binding={**BINDING, key: value})
        final[0]["content"] += " "
        with self.assertRaisesRegex(assertions.AssertionError, "capture/window mismatch"):
            consumer(fixture(), final=final)

    def test_unknown_key_profile_or_options_never_ignored(self):
        for spec in ({"new_unknown_assertion": True}, {**SPEC, "extra": True},
                     {**SPEC, "requires_feature": "skip-me"},
                     {"no_duplicate_marker_with_known_gap": {"marker": gap.PRE, "known_gap": "unknown"}},
                     {"no_duplicate_marker_with_known_gap": {**SPEC["no_duplicate_marker_with_known_gap"], "extra": True}}):
            with self.subTest(spec=spec), self.assertRaises(assertions.AssertionError):
                consumer(None, spec=spec, final=[row(101, gap.BODY)])

    def test_capture_completeness_and_malformed_evidence_fail_closed(self):
        for captures in (None, [], [{"pages": []}], [{"pages": [{}, {}]}]):
            self.assertEqual(gap.evaluate_e22_known_gap(captures, run_id=RUN, **BINDING)["classification"], "NOT_EVALUABLE")
        for field, value in (("limit", True), ("observed_at", float("nan")), ("messages", {}),
                             ("after_id", "98"), ("channel_id", "999")):
            captures = fixture()
            captures[-1]["pages"][0][field] = value
            self.assertEqual(gap.evaluate_e22_known_gap(captures, run_id=RUN, **BINDING)["classification"], "FAIL")
        captures = fixture()
        captures[-1]["pages"][0]["messages"] *= 25
        self.assertEqual(gap.evaluate_e22_known_gap(captures, run_id=RUN, **BINDING)["classification"], "NOT_EVALUABLE")
        captures = fixture()
        captures.insert(0, {"pages": [{**captures[0]["pages"][0], "messages": [], "observed_at": BASE + 9}]})
        consumer(captures)

    def test_fetch_captures_unfiltered_response_without_extra_get_or_mutation(self):
        captures, requests = [], []
        def fetch(request, **_kwargs):
            requests.append(request.full_url)
            return _Response({"messages": fixture()[-1]["pages"][0]["messages"]})
        client = discord.DiscordClient("http://offline.invalid", captures=captures, capture_after_id="99")
        with patch("urllib.request.urlopen", side_effect=fetch):
            returned = client.fetch_messages(gap.CHANNEL_ID, after_id="100", limit=100)
        self.assertEqual(len(requests), 1)
        self.assertIn("after=99", requests[0])
        self.assertEqual(len(returned), 3)
        self.assertEqual(len(captures[0]["pages"][0]["messages"]), 4)
        returned[0]["content"] = "mutated by consumer"
        self.assertEqual(captures[0]["pages"][0]["messages"][1]["content"], gap.PRE)

    def test_run_one_cell_real_fetch_wait_consumer_and_report_pipeline(self):
        captures = fixture()
        pages = [c["pages"][0]["messages"] for c in captures]
        requests = []
        def fetch(request, **_kwargs):
            requests.append(request.full_url)
            return _Response(pages[min(len(requests) - 1, 2)])
        scenario = yaml.safe_load(YAML.read_text())
        original_mark = assertions.Window.mark_prompt_sent
        def mark(window):
            original_mark(window, datetime.fromtimestamp(BASE + 0.5, timezone.utc))
        client = discord.DiscordClient("http://offline.invalid")
        with patch("urllib.request.urlopen", side_effect=fetch), patch.object(driver.time, "sleep"), \
             patch.object(discord.DiscordClient, "send_control", return_value={"id": "100"}), \
             patch.object(discord.DiscordClient, "send_prompt", return_value={"message_id": "102"}), \
             patch.object(driver, "wait_for_provider_hold_state", return_value={"ok_marker": gap.PRE, "ok_marker_seen": True}), \
             patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}), \
             patch.object(assertions.Window, "mark_prompt_sent", mark):
            record = driver.run_one_cell(scenario=scenario, cell="claude-tui", channel_id=gap.CHANNEL_ID,
                client=client, run_id=RUN, dry_run=False, args=Namespace(queue_runtime_root="/offline-denied"))
        self.assertEqual(len(requests), 5)  # setup echo + two existing wait polls + two final refetches
        self.assertTrue(all("after=99" in url for url in requests))
        self.assertEqual(record["known_gaps"][0]["message_ids"], ["101", "103"])
        self.assertEqual(record["message_updates"], 1)
        self.assertNotIn("revalidated_after_recheck", record)
        self.assertIsNone(client.captures)  # per-scenario replacement never contaminates the next scenario
        self.assertEqual(record["coverage_class_actual"], "live")  # declared fixture, not actual live execution


if __name__ == "__main__":
    unittest.main()
