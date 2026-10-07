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
        self.assertEqual(len(requests), 8)
        self.assertEqual(sleeps, [2.0] * 3)
        self.assertNotIn("known_gap_rechecks", record)
        self.assertEqual(record["completion_rechecks"][0]["refetches"], 3)
        self.assertEqual(record["completion_rechecks"][0]["outcome"], "EXHAUSTED")
        self.assertEqual(len(record["revalidated_after_recheck"]), 3)
        self.assertTrue(all(item["passed"] for item in record["revalidated_after_recheck"]))

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


class HerdrExpectedFailure(unittest.TestCase):
    def test_profiles_are_cell_and_scenario_scoped(self):
        from tui_relay import known_gap as gaps
        for cell in ('claude-herdr', 'codex-herdr'):
            self.assertEqual(gaps.herdr_profile('E-18', cell)['issue'], '#5340 HTTP HostOwned follow-up (not P10-3)')
            self.assertEqual(gaps.herdr_profile('E-12', cell)['issue'], '#5340 P11')
        for scenario in ('E-2', 'E-5', 'E-8', 'E-19', 'E-30', 'E-36', 'E-51'):
            self.assertEqual(gaps.herdr_profile(scenario, 'codex-herdr')['issue'], '#5340 P10-2')
            self.assertIsNone(gaps.herdr_profile(scenario, 'claude-herdr'))
        self.assertIsNone(gaps.herdr_profile('E-1', 'codex-herdr'))
        self.assertIsNone(gaps.herdr_profile('E-18', 'claude-tui'))

    def test_gap_and_unexpected_pass_never_become_pass(self):
        from tui_relay import known_gap as gaps
        for passed, status, label in [(False, 'known_gap', 'KNOWN_GAP'), (True, 'unexpected_pass', 'UNEXPECTED_PASS')]:
            result = {'reason': 'original observation'}
            gaps.apply_herdr_result(result, gaps.herdr_profile('E-18', 'claude-herdr'), passed=passed)
            self.assertEqual(result['status'], status)
            self.assertEqual(result['known_gaps'][0]['classification'], label)
            self.assertEqual(result['known_gaps'][0]['observed_reason'], 'original observation')

    def execute(self, scenario, *, cell='claude-herdr', cancel_error=None, missing=None, preflight_error=None):
        from unittest.mock import patch, Mock
        from argparse import Namespace
        import run_tui_relay as driver
        from tui_relay import herdr, assertions
        class NoTmux:
            def __getattr__(self, name):
                raise AssertionError('tmux used: ' + name)
        rows = []
        client = Mock(base_url='http://unused.test')
        client.send_control.return_value = {'id': '100'}
        client.send.return_value = {'id': '101'}
        def wait(channel, **kwargs):
            marker = 'OK' if not rows else 'NEXT'
            row = {'id': str(102 + len(rows)), 'content': marker, 'author': {'id': '42', 'bot': True}}
            rows.append(row)
            return (None if marker == missing else row), list(rows)
        client.wait_for_message.side_effect = wait
        client.fetch_messages.side_effect = lambda *a, **kw: list(rows)
        args = Namespace(cell=cell, channel_id='41', base_url='http://unused.test', dry_run=False,
                         hard_reset_session_each=False, reset_before_each=True, allow_destructive=True,
                         phase_deadline_s=None, final_refetches=1, herdr_endpoint='local')
        full = {'id': 'E-18', 'agent_mode': 'real_live', 'coverage_class': 'live', 'steps': [], 'assertions': [], **scenario}
        with patch.object(driver, 'tmux', NoTmux()), patch.object(driver, 'reset_channel_state', side_effect=AssertionError('reset')), \
             patch.object(herdr, 'observe', side_effect=preflight_error, return_value={'herdr': {}}), \
             patch.object(herdr.time, 'sleep'), patch.object(driver, 'cancel_turn', side_effect=cancel_error) as cancel, \
             patch.dict('os.environ', {'AGENTDESK_E2E_ALLOW_DESTRUCTIVE': '1'}):
            result = driver.run_scenario(full, args=args, client=client, run_id='test')
        return result, cancel, client

    def test_stop_and_forced_termination_errors_are_not_skipped_or_hidden(self):
        from tui_relay import assertions
        for scenario_id, operation, issue in [('E-18', {'cancel_turn': {'force': True}}, '#5340 HTTP HostOwned follow-up (not P10-3)'),
                                               ('E-12', {'kill_pane': {}}, '#5340 P11')]:
            result, cancel, client = self.execute({'id': scenario_id, 'steps': [
                {'send_prompt': 'OK'}, {'wait_for_discord_text': 'OK'}, operation]},
                cancel_error=assertions.AssertionError('unsupported Herdr cancellation'))
            self.assertEqual(result['status'], 'fail')
            self.assertIn(issue, result['expected_gap']['issue'])
            cancel.assert_called_once()
            client.send.assert_called_once()

    def test_unexpected_pass_is_explicit(self):
        result, cancel, _ = self.execute({'steps': [{'send_prompt': 'OK'}, {'wait_for_discord_text': 'OK'}, {'cancel_turn': {}}]})
        cancel.assert_called_once()
        self.assertEqual(result['status'], 'unexpected_pass')

    def test_preflight_failure_is_not_a_known_gap(self):
        from tui_relay import assertions
        result, cancel, client = self.execute({'steps': [{'cancel_turn': {}}]},
                                              preflight_error=assertions.AssertionError('admission stopped'))
        self.assertEqual(result['status'], 'fail')
        self.assertNotIn('known_gaps', result)
        cancel.assert_not_called()
        client.send.assert_not_called()

    def test_followup_runs_before_known_gap(self):
        scenario = {'id': 'E-2', 'steps': [{'send_prompt': 'OK'}, {'wait_for_discord_text': 'OK'},
                                         {'send_prompt': 'NEXT'}, {'wait_for_discord_text': 'NEXT'}]}
        result, _, client = self.execute(scenario, cell='codex-herdr', missing='NEXT')
        self.assertEqual(client.send.call_count, 2)
        self.assertEqual(result['status'], 'known_gap')
        self.assertIn('P10-2', result['reason'])
        result, _, _ = self.execute(scenario, cell='codex-herdr', missing='OK')
        self.assertEqual(result['status'], 'fail')
        self.assertNotIn('known_gaps', result)

    def test_partial_tmux_coverage_never_counts_as_pass(self):
        result, _, _ = self.execute({'id': 'E-19', 'steps': [{'capture_session_identity': {}}]})
        self.assertEqual(result['status'], 'not_applicable')
        self.assertTrue(result['not_applicable'])

    def test_successful_http_cancel_with_late_output_is_regression(self):
        result, cancel, _ = self.execute({'steps': [{'send_prompt': 'OK'}, {'wait_for_discord_text': 'OK'},
                {'cancel_turn': {}}], 'assertions': [{'marker_absent': {'marker': 'OK', 'surface': 'relay'}}]})
        cancel.assert_called_once_with(base_url='http://unused.test', channel_id='41', force=False)
        self.assertEqual(result['status'], 'fail')
        self.assertNotIn('known_gaps', result)

    def test_forced_cancel_missing_exit_witness_is_known_gap(self):
        result, cancel, _ = self.execute({'id': 'E-12', 'steps': [{'send_prompt': 'OK'},
                {'wait_for_discord_text': 'OK'}, {'kill_pane': {}}, {'wait_for_discord_text': 'NEXT'}]}, missing='NEXT')
        cancel.assert_called_once_with(base_url='http://unused.test', channel_id='41', force=True)
        self.assertEqual(result['status'], 'known_gap')
        self.assertIn('P11', result['reason'])

    def test_http_cancel_error_is_a_regression_not_known_gap(self):
        from tui_relay import assertions
        result, cancel, _ = self.execute({'steps': [{'send_prompt': 'OK'}, {'wait_for_discord_text': 'OK'}, {'cancel_turn': {}}]},
                cancel_error=assertions.AssertionError('cancel_turn HTTP 401'))
        cancel.assert_called_once()
        self.assertEqual(result['status'], 'fail')
        self.assertNotIn('known_gaps', result)

    def test_exact_http_host_guard_conflict_is_known_gap_after_real_attempt(self):
        import io
        import urllib.error
        from tui_relay import assertions
        for scenario_id, operation in [('E-18', {'cancel_turn': {}}), ('E-12', {'kill_pane': {}})]:
            for code, reason, expected in [(409, 'session host is not legacy tmux', 'known_gap'),
                                            (409, 'stop_unobserved', 'fail'), (403, 'session host is not legacy tmux', 'fail')]:
                error = assertions.AssertionError(f'cancel_turn HTTP {code}: {reason}')
                error.__cause__ = urllib.error.HTTPError('http://unused.test', code, reason, {}, io.BytesIO())
                result, cancel, _ = self.execute({'id': scenario_id, 'steps': [{'send_prompt': 'OK'},
                        {'wait_for_discord_text': 'OK'}, operation]}, cancel_error=error)
                cancel.assert_called_once()
                self.assertEqual(result['status'], expected)

    def test_text_stop_uses_discord_midturn_and_keeps_http_gap_separate(self):
        import io
        import urllib.error
        from unittest.mock import Mock
        from tui_relay import herdr
        for refused, duplicate, expected in [(False, False, 'unexpected_pass'), (True, False, 'known_gap'), (False, True, 'fail')]:
            scenario = driver.yaml.safe_load((Path(__file__).resolve().parents[3] / 'tests/e2e/tui_relay/scenarios/E-18-stop-mid-turn-cancel.yaml').read_text())
            args = Namespace(cell='claude-herdr', channel_id='41', base_url='http://unused.test', dry_run=False,
                             hard_reset_session_each=False, reset_before_each=False, allow_destructive=True, final_refetches=1)
            rows, sends = [], []
            client = Mock()
            client.send_control.side_effect = [{'id': '100'}, {'id': '200'}, {'id': '300'}, {'id': '400'}]
            def add(text):
                r = {'id': str(501 + len(rows)), 'content': text, 'author': {'id': '42', 'bot': True}}
                rows.append(r)
            def send(channel, text):
                sends.append(text)
                if text == '!stop':
                    add('이 세션의 호스트를 확인하지 못해 중지하지 않았어요. 턴은 계속 진행돼요.' if refused else '중지하고 있어요...')
                    if duplicate:
                        add('중지하고 있어요...')
                elif 'E18S:NEXT' in text:
                    add('[E2E:E18S:NEXT]')
                elif 'E18S:OK' in text:
                    rows.clear()
                    add('[E2E:E18S:OK]')
                else:
                    add('[E2E:E18:OK]')
                return {'id': '450'}
            client.send.side_effect = send
            client.wait_for_message.side_effect = lambda channel, **k: (next((r for r in rows if k['predicate'](r)), None), list(rows))
            client.fetch_messages.side_effect = lambda *a, **k: list(rows)
            error = assertions.AssertionError('cancel_turn HTTP 409: session host is not legacy tmux')
            error.__cause__ = urllib.error.HTTPError('http://unused.test', 409, 'conflict', {}, io.BytesIO())
            with patch.object(herdr, 'observe', return_value={'herdr': {}}), patch.object(herdr.time, 'sleep'), \
                 patch.object(driver, 'tmux', None), patch.object(driver, 'cancel_turn', side_effect=error), \
                 patch.dict('os.environ', {'AGENTDESK_E2E_ALLOW_DESTRUCTIVE': '1'}):
                result = driver.run_scenario(scenario, args=args, client=client, run_id='stop')
            self.assertEqual(result['subcases'][0]['status'], 'known_gap')
            self.assertIn('HTTP HostOwned follow-up (not P10-3)', result['subcases'][0]['reason'])
            self.assertEqual(result['subcases'][1]['status'], expected, result)
            self.assertEqual(sends.count('!stop'), 1)
            self.assertEqual(sum('E18S:OK' in x for x in sends), 1)
            if not refused:
                self.assertEqual(len(sends), 4)  # HTTP hold, command hold, !stop, one next prompt; no resend.
