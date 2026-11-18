"""Offline judgments for reaction-history and session-host assertions; no Discord or host contact."""

import json
import subprocess
import sys
import threading
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_tui_relay as driver  # noqa: E402
from tui_relay import assertions, surface_assertions as surface  # noqa: E402

MID = "900000000000000002"
PARENT, THREAD = "900000000000000011", "900000000000000019"
HISTORY = {"reaction_history": {"send": 2, "final": ["➕"], "forbid": ["📬"]}}
INJECT = {"busy_injection_within": {"send": 2, "max_seconds": 30, "before_marker": "[E2E:E63:{run_id}:DONE]"}}


def read(t, *marks, end=None):
    poll = {"t0": round(t - 0.2, 3), "t": t, "reactions": list(marks)}
    return poll if end is None else {**poll, "turn_end": end}


def failed(t):
    return {"t0": round(t - 0.2, 3), "t": t, "error": "fetch_messages URL error"}


def watched(*polls, done_at=None):
    bodies = {"[E2E:E63:r1:DONE]": 100.0 + done_at} if done_at is not None else {}
    return {"reaction_watch": {"2": {"message_id": MID, "acked_at": 100.0, "polls": list(polls),
                                     "turn_end_marker": "[E2E:E63:r1:DONE]"}},
            "_body_observations": bodies}


def judge(spec, record):
    return driver.run_assertion(spec, window=assertions.Window(setup_marker_id="1"), record=record, run_id="r1")


class ReactionHistory(unittest.TestCase):
    def test_single_final_mark_with_dense_reads_passes(self):
        outcome = judge(HISTORY, watched(read(0.5), read(1.0, "➕"), read(6.0, "➕")))
        self.assertEqual(outcome["status"], "pass")
        self.assertEqual(outcome["details"]["changes"], [[0.5, []], [1.0, ["➕"]]])

    def test_transient_queue_mark_or_extra_final_mark_fails(self):
        for polls, reason in (((read(0.5, "📬"), read(1.0, "➕")), "forbidden reaction"),
                              ((read(0.5), read(9.0, "➕", "✅")), "final reaction")):
            with self.subTest(reason=reason), self.assertRaisesRegex(assertions.AssertionError, reason) as caught:
                judge(HISTORY, watched(*polls))
            self.assertNotIsInstance(caught.exception, driver.HarnessEvidenceError)

    def test_failed_or_sparse_observation_is_unevaluable_not_pass(self):
        for record in (watched(read(0.5), read(1.0, "➕"), failed(6.0)),
                       watched(read(0.4), failed(0.9), failed(1.4), failed(1.9), read(3.0, "➕")),
                       {"reaction_watch": {}}):
            with self.subTest(record=record), self.assertRaises(driver.HarnessEvidenceError):
                judge(HISTORY, record)


class BusyInjection(unittest.TestCase):
    def test_injection_within_bound_and_before_turn_end_passes(self):
        outcome = judge(INJECT, watched(read(0.5, end=False), read(1.2, "➕", end=False), done_at=80.0))
        self.assertEqual(outcome["details"]["injected_between_s"], [0.3, 1.2])

    def test_missing_late_or_post_turn_injection_fails(self):
        cases = ((INJECT, (read(0.5), read(90.0)), "never shown"),
                 (INJECT, (read(0.5), read(40.2), read(41.0, "➕")), "later than 30"),
                 ({"busy_injection_within": {**INJECT["busy_injection_within"], "max_seconds": 300}},
                  (read(0.5, end=False), read(25.2, end=True), read(26.0, "➕", end=True)), "after the turn-end"))
        for spec, polls, reason in cases:
            with self.subTest(reason=reason), self.assertRaisesRegex(assertions.AssertionError, reason) as caught:
                judge(spec, watched(*polls, done_at=20.0))
            self.assertNotIsInstance(caught.exception, driver.HarnessEvidenceError)

    def test_straddling_gap_or_unseen_turn_end_is_unevaluable(self):
        for record in (watched(read(20.2), failed(28.0), read(35.0, "➕"), done_at=80.0),
                       watched(read(0.5), read(1.2, "➕"))):
            with self.subTest(record=record), self.assertRaises(driver.HarnessEvidenceError):
                judge(INJECT, record)


class Watch(unittest.TestCase):
    def test_watch_reads_the_sent_message_and_keeps_read_failures(self):
        calls, third = [], threading.Event()
        message = {"id": MID, "reactions": [{"emoji": {"name": "⚠️"}, "count": 1},
                                            {"emoji": {"name": "📬"}, "count": 0}]}

        def fetch(channel_id, **kwargs):
            calls.append((channel_id, kwargs))
            if len(calls) >= 3:
                third.set()
            if len(calls) == 1:
                raise RuntimeError("fetch_messages observed HTTP 502")
            return [message]

        scenario = {"assertions": [HISTORY]}
        record = {}
        with mock.patch.object(surface, "DENSE_INTERVAL_S", 0.01):
            watch = surface.ReactionWatch.for_scenario(scenario, mock.Mock(fetch_messages=fetch), "c1", record)
            for _ in range(2):
                driver._ack_send(driver._record_send(record, "send_discord_prompt", "p"), {"message_id": MID})
                watch.on_send(record)
            self.assertTrue(third.wait(5))
            watch.stop()
        polls = record["reaction_watch"]["2"]["polls"]
        self.assertEqual(list(record["reaction_watch"]), ["2"])
        self.assertEqual(calls[0], ("c1", {"after_id": str(int(MID) - 1), "limit": 1}))
        self.assertIn("HTTP 502", polls[0]["error"])
        self.assertEqual(polls[-1]["reactions"], ["⚠"])


def detail(*herdr_channels):
    return {"herdr": {"configured_channels": list(herdr_channels)}} if herdr_channels else {}


def host_run(tmux_up, herdr_rows):
    def run(argv, **_kwargs):
        if argv[0] == "tmux":
            return subprocess.CompletedProcess(argv, 0 if tmux_up else 1, "", "" if tmux_up else "can't find session: x")
        if herdr_rows is None:
            raise subprocess.CalledProcessError(1, argv)
        return subprocess.CompletedProcess(argv, 0, json.dumps({"executions": herdr_rows}), "")
    return run


def host_context():
    return surface.surface_context(base_url="http://127.0.0.1:1", provider="claude", agent="a", channel_id=THREAD,
                                   parent_channel_id=PARENT, thread=True, herdr_status_bin="agentdesk")


def check_host(spec, health, run, before=None):
    record = {"surface_context": host_context(), "session_probe_before": before,
              "prompt_sends": [{"step": "send_discord_prompt", "send_state": "acknowledged", "message_id": MID}]}
    with mock.patch.object(driver, "_read_api_json", return_value=(200, health)), \
            mock.patch.object(surface.subprocess, "run", run):
        surface.after_answer(record, driver._read_api_json)
        return judge(spec, record)


class SessionHost(unittest.TestCase):
    def check(self, spec, health, run, before=None):
        return check_host(spec, health, run, before)

    def test_thread_must_run_on_the_parent_configured_host(self):
        spec = {"session_host": {"expect": "configured"}}
        herdr_row = [{"channel": THREAD, "provider": "claude", "state": "bound", "pane": "provider_running",
                      "launch_evidence": "recorded"}]
        self.assertEqual(self.check(spec, detail(PARENT), host_run(False, herdr_row))["status"], "pass")
        self.assertEqual(self.check(spec, detail(), host_run(True, None))["status"], "pass")
        with self.assertRaisesRegex(assertions.AssertionError, r"session on \['tmux'\], expected herdr"):
            self.check(spec, detail(PARENT), host_run(True, []))
        with self.assertRaises(driver.HarnessEvidenceError):
            self.check(spec, detail(PARENT), host_run(False, None))

    def test_first_thread_message_must_start_a_session(self):
        spec = {"thread_session_started": True}
        empty = {"configured": "tmux", "hosts": {"tmux": False, "herdr": False}}
        self.assertEqual(self.check(spec, detail(), host_run(True, None), before=empty)["status"], "pass")
        running = {"configured": "tmux", "hosts": {"tmux": True, "herdr": False}}
        self.assertEqual(self.check(spec, detail(), host_run(True, None), before=running)["status"], "not_applicable")
        with self.assertRaisesRegex(assertions.AssertionError, "started no session"):
            self.check(spec, detail(), host_run(False, None), before=empty)
        with self.assertRaises(driver.HarnessEvidenceError):
            self.check(spec, detail(), host_run(True, None), before={"error": "probe failed"})


class ReviewFindings(unittest.TestCase):
    def test_turn_end_seen_in_the_same_read_before_injection_fails(self):
        at = read
        late = watched(at(0.5, end=False), at(11.0, end=True), at(11.5, "➕", end=True), done_at=15.0)
        with self.assertRaisesRegex(assertions.AssertionError, "after the turn-end") as caught:
            judge(INJECT, late)
        self.assertNotIsInstance(caught.exception, driver.HarnessEvidenceError)
        self.assertEqual(judge(INJECT, watched(at(0.5, end=False), at(1.2, "➕", end=False)))["status"], "pass")
        with self.assertRaises(driver.HarnessEvidenceError):
            judge(INJECT, watched(at(0.5, end=False), failed(6.0), at(12.0, "➕", end=True), done_at=80.0))

    def test_one_read_classifies_the_turn_end_marker_from_relay_bodies_only(self):
        mine = {"id": MID, "author": {"id": assertions.OUR_BOT_ID, "bot": True}, "reactions": [],
                "content": "[E2E:E63:r1:DONE] quoted in our own prompt"}
        done = {"id": str(int(MID) + 1), "author": {"id": "7", "bot": True}, "content": "[E2E:E63:r1:DONE]"}
        other = {"id": str(int(MID) + 2), "author": {"id": "7", "bot": True}, "content": "working"}
        row = {"message_id": MID, "acked_at": 0.0, "turn_end_marker": "[E2E:E63:r1:DONE]", "read_after": "1"}
        for rows, expected in (([mine, done], True), ([mine], False), ([mine] + [other] * 99, None)):
            watch = surface.ReactionWatch({}, lambda *_a, rows=rows, **kw: rows if kw["limit"] == 100 else [], "c1", {})
            with self.subTest(expected=expected):
                self.assertIs(watch.poll(row)["turn_end"], expected)

    def test_marker_edited_into_a_reply_older_than_the_input_is_seen(self):
        reply = {"id": str(int(MID) - 10), "author": {"id": "7", "bot": True}, "content": "[E2E:E63:r1:HOLD]"}
        sent = {"id": MID, "author": {"id": assertions.OUR_BOT_ID, "bot": True}, "reactions": []}
        fetch = lambda _c, after_id, limit: [m for m in (reply, sent) if int(m["id"]) > int(after_id)][:limit]
        watch, record = surface.ReactionWatch({2: {"➕"}}, fetch, "c1", {}, markers={2: "[E2E:E63:r1:DONE]"}), {}
        for _ in range(2):
            driver._ack_send(driver._record_send(record, "send_discord_prompt", "p"), {"message_id": MID})
            watch.on_send(record, read_after=str(int(MID) - 20))
        watch.stop()
        row = watch.store["2"]
        polls = [watch.poll(row)]
        reply["content"] += "\n[E2E:E63:r1:DONE]"
        polls.append(watch.poll(row))
        sent["reactions"] = [{"emoji": {"name": "➕"}, "count": 1}]
        polls.append(watch.poll(row))
        with self.assertRaisesRegex(assertions.AssertionError, "after the turn-end"):
            judge(INJECT, {"reaction_watch": {"2": {**row, "polls": polls}}})
        self.assertIsNone(watch.poll({**row, "read_after": None})["turn_end"])

    def test_failed_tmux_read_or_non_running_herdr_row_is_not_a_session(self):
        spec, context = {"session_host": {"expect": "configured"}}, host_context()
        denied = lambda argv, **_: subprocess.CompletedProcess(argv, 1, "", "error connecting to socket (Permission denied)")
        record = {"prompt_sends": [{"step": "send_discord_prompt", "send_state": "acknowledged", "message_id": MID}]}
        with mock.patch.object(driver, "_read_api_json", return_value=(200, detail())):
            surface.before_steps({"assertions": [{"thread_session_started": True}]}, record, lambda: context,
                                 driver._read_api_json, denied)
        record["session_probe_after_first"] = surface.probe_hosts(context, lambda *_: (200, detail()), host_run(True, None))
        with self.assertRaises(driver.HarnessEvidenceError):
            judge({"thread_session_started": True}, record)
        for pane, error in (("missing", "no host"), ("unreadable", None)):
            row = [{"channel": THREAD, "provider": "claude", "state": "bound", "pane": pane, "launch_evidence": "recorded"}]
            with self.subTest(pane=pane):
                if error:
                    with self.assertRaisesRegex(assertions.AssertionError, error) as caught:
                        check_host(spec, detail(PARENT), host_run(False, row))
                    self.assertNotIsInstance(caught.exception, driver.HarnessEvidenceError)
                else:
                    with self.assertRaises(driver.HarnessEvidenceError):
                        check_host(spec, detail(PARENT), host_run(False, row))
        with self.assertRaises(driver.HarnessEvidenceError):
            check_host(spec, detail(PARENT), host_run(False, [{"session_row": 1, "record": "unreadable"}]))

    def test_reaction_reads_reach_the_final_report_json(self):
        polls = [read(0.5), failed(1.0), read(1.5, "➕")]
        record = {"assertions": [], "reaction_watch": {"2": {"message_id": MID, "acked_at": 100.0, "polls": polls}}}
        result = {"assertions": []}
        driver._merge_record_into_result(result, record)
        report = json.loads(json.dumps(result))
        self.assertEqual(report["reaction_watch"]["2"]["polls"], polls)

    def test_first_thread_input_must_be_a_discord_message(self):
        empty = {"configured": "tmux", "hosts": {"tmux": False, "herdr": False}}
        running = {"configured": "tmux", "hosts": {"tmux": True, "herdr": False}}
        for step, message_id in (("send_prompt", None), ("send_prompt", MID), ("send_discord_prompt", None)):
            record = {"surface_context": host_context(), "session_probe_before": empty,
                      "session_probe_after_first": running,
                      "prompt_sends": [{"step": step, "send_state": "acknowledged", "message_id": message_id}]}
            with self.subTest(step=step, message_id=message_id), \
                    self.assertRaisesRegex(driver.HarnessEvidenceError, "Discord"):
                judge({"thread_session_started": True}, record)
        root = Path(__file__).resolve().parents[3] / "tests/e2e/tui_relay/scenarios"
        e14 = next(s for s in driver.load_scenarios(root, cell="claude-tui") if s["id"] == "E-14")
        self.assertIn("send_discord_prompt", e14["steps"][0])


class Loader(unittest.TestCase):
    def test_bad_values_are_refused_at_load_and_targeted_scenarios_carry_the_checks(self):
        for spec in ({"reaction_history": {"send": 2, "final": []}},
                     {"reaction_history": {"send": 2, "final": ["➕"], "forbid": ["➕"]}},
                     {"busy_injection_within": {"send": 0, "before_marker": "x"}},
                     {"session_host": {"expect": "pipe"}}, {"thread_session_started": False}):
            with self.subTest(spec=spec), self.assertRaises(ValueError):
                driver.validate_scenario_schema({"assertions": [spec]})
        root = Path(__file__).resolve().parents[3] / "tests/e2e/tui_relay/scenarios"
        loaded = {s["id"]: s for s in driver.load_scenarios(root, cell="claude-tui")}
        ops = {sid: {next(iter(spec)) for spec in loaded[sid]["assertions"]} for sid in ("E-63", "E-14")}
        self.assertLessEqual({"reaction_history", "busy_injection_within"}, ops["E-63"])
        self.assertLessEqual({"thread_session_started", "session_host"}, ops["E-14"])


ORIGIN = "[origin=operational_alert]"
# The producer encodes each UTF-8 bit as U+200B (0) or U+200C (1).
HIDDEN_ORIGIN = "".join("\u200c" if bit == "1" else "\u200b"
                        for byte in ORIGIN.encode() for bit in f"{byte:08b}")
NOTICE = "Codex에 입력을 전달했습니다. 기존 초안의 보존 여부는 확인하지 못했습니다."
OP = "no_unexpected_operational_alerts"


def message(key, body):
    return {"id": key, "content": body, "author": {"id": "relay", "bot": True}, "type": 0}


def window(*bodies):
    result = assertions.Window(setup_marker_id="setup")
    for index, body in enumerate(bodies):
        result.add(message(str(index), body))
    return result


class OperationalAlerts(unittest.TestCase):
    def test_three_repeated_marked_notices_are_not_duplicate_relay_bodies(self):
        observed = window(HIDDEN_ORIGIN + NOTICE, "[T1]", HIDDEN_ORIGIN + NOTICE,
                          "[T2]", HIDDEN_ORIGIN + NOTICE, "[AFTER_CLEAR]")
        assertions.no_duplicate_content(observed)
        assertions.message_count_between_markers(observed, low=3, high=3)
        self.assertEqual(len(observed.raw_messages), 6)

    def test_literal_hidden_and_inline_origins_are_excluded_from_relay(self):
        for origin in (ORIGIN, HIDDEN_ORIGIN, "-# " + ORIGIN, "-# " + HIDDEN_ORIGIN):
            with self.subTest(origin="inline" if origin.startswith("-#") else "prefix"):
                observed = window(origin + NOTICE)
                self.assertEqual(observed.messages, [])
                self.assertIsNone(assertions.relay_body(observed.raw_messages[0]))

    def test_status_patterns_match_after_origin_removal(self):
        for origin in (ORIGIN, HIDDEN_ORIGIN):
            with self.subTest(hidden=origin == HIDDEN_ORIGIN):
                self.assertTrue(assertions.is_status_chrome(message("status", origin + "⚠️ 알림")))
                observed = window(origin + "✅ 완료")
                assertions.chrome_count(observed, regex="^✅", exact=1)
                assertions.single_status_panel(observed, panel_regex="^✅")
        assertions.chrome_count(window("-# " + HIDDEN_ORIGIN + "⚠️ 알림"), regex="^-# ⚠️", exact=1)

    def test_unmarked_duplicate_and_interior_marker_prose_still_fail(self):
        for body in (NOTICE, "The answer quotes " + ORIGIN + " as an example."):
            with self.subTest(body=body), self.assertRaises(assertions.AssertionError):
                assertions.no_duplicate_content(window(body, body))

    def test_duplicate_diagnostic_removes_zero_width_characters_before_truncation(self):
        body = "\u200b" * 100 + "same answer"
        with self.assertRaises(assertions.AssertionError) as caught:
            assertions.no_duplicate_content(window(body, body))
        self.assertIn("same answer", str(caught.exception))
        self.assertNotIn("\u200b", str(caught.exception))

    def test_unexpected_alert_fails_separate_assertion_with_visible_sentence(self):
        with self.assertRaisesRegex(assertions.AssertionError, "unexpected operational alert") as caught:
            driver.run_assertion({OP: {"allow": []}}, window=window(HIDDEN_ORIGIN + NOTICE))
        self.assertIn(NOTICE, str(caught.exception))
        self.assertNotIn("\u200b", str(caught.exception))

    def test_exact_allowlist_passes_and_does_not_allow_other_notices(self):
        spec = {OP: {"allow": [NOTICE]}}
        self.assertTrue(driver.run_assertion(spec, window=window(HIDDEN_ORIGIN + NOTICE))["passed"])
        with self.assertRaisesRegex(assertions.AssertionError, "unexpected operational alert"):
            driver.run_assertion(spec, window=window(HIDDEN_ORIGIN + NOTICE + " 다른 알림"))
        joined_emoji = "👨\u200d👩 가족 알림"
        self.assertTrue(driver.run_assertion({OP: {"allow": [joined_emoji]}},
                                            window=window(HIDDEN_ORIGIN + joined_emoji))["passed"])

    def test_missing_window_is_unevaluable_instead_of_passing(self):
        with self.assertRaises(surface.Unevaluable):
            surface.run({OP: {"allow": []}}, record={}, run_id="r", read_api=mock.Mock())

    def test_clean_channel_is_evaluated_without_host_probe_or_skip(self):
        with mock.patch.object(surface, "probe_hosts", side_effect=RuntimeError("no host probe")):
            result = driver.run_assertion({OP: {"allow": []}}, window=window("answer"))
        self.assertEqual(result, {"status": "pass", "passed": True,
                                  "details": {"observed": 0, "allowed": 0}})

    def test_deleted_and_announce_authored_alerts_fail_but_human_posts_are_ignored(self):
        observed = window(HIDDEN_ORIGIN + NOTICE)
        observed.deleted_ids.add("0")
        with self.assertRaisesRegex(assertions.AssertionError, "unexpected operational alert"):
            driver.run_assertion({OP: {"allow": []}}, window=observed)
        announce = assertions.Window(setup_marker_id="setup")
        announce.add({**message("announce", HIDDEN_ORIGIN + NOTICE),
                      "author": {"id": assertions.OUR_BOT_ID, "bot": True}})
        with self.assertRaisesRegex(assertions.AssertionError, "unexpected operational alert"):
            driver.run_assertion({OP: {"allow": []}}, window=announce)
        ignored = assertions.Window(setup_marker_id="setup")
        ignored.add({**message("human", HIDDEN_ORIGIN + NOTICE),
                     "author": {"id": "human", "bot": False}})
        self.assertTrue(driver.run_assertion({OP: {"allow": []}}, window=ignored)["passed"])

    def test_allowlist_is_validated_as_exact_visible_sentences(self):
        for allow in (None, NOTICE, [""], ["   "], [1]):
            with self.subTest(allow=allow), self.assertRaises(ValueError):
                driver.validate_scenario_schema({"assertions": [{OP: {"allow": allow}}]})
        driver.validate_scenario_schema({"assertions": [{OP: {"allow": [NOTICE]}}]})

    def test_inexact_zero_width_origin_is_not_a_provenance_marker(self):
        malformed = HIDDEN_ORIGIN[:-1] + ("\u200c" if HIDDEN_ORIGIN[-1] == "\u200b" else "\u200b")
        with self.assertRaises(assertions.AssertionError):
            assertions.no_duplicate_content(window(malformed + "answer", malformed + "answer"))

    def test_both_clean_smoke_scenarios_require_operational_alert_assertion(self):
        root = Path(__file__).resolve().parents[3] / "tests/e2e/tui_relay/scenarios"
        for cell, sid in (("claude-tui", "E-50"), ("codex-tui", "E-51")):
            loaded = {scenario["id"]: scenario for scenario in driver.load_scenarios(root, cell=cell)}
            with self.subTest(scenario=sid):
                self.assertIn({OP: {"allow": []}}, loaded[sid]["assertions"])



if __name__ == "__main__":
    unittest.main()
