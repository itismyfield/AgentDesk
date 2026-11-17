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


def read(t, *marks):
    return {"t0": round(t - 0.2, 3), "t": t, "reactions": list(marks)}


def failed(t):
    return {"t0": round(t - 0.2, 3), "t": t, "error": "fetch_messages URL error"}


def watched(*polls, done_at=None):
    bodies = {"[E2E:E63:r1:DONE]": 100.0 + done_at} if done_at is not None else {}
    return {"reaction_watch": {"2": {"message_id": MID, "acked_at": 100.0, "polls": list(polls)}},
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
        outcome = judge(INJECT, watched(read(0.5), read(1.2, "➕"), done_at=80.0))
        self.assertEqual(outcome["details"]["injected_between_s"], [0.3, 1.2])

    def test_missing_late_or_post_turn_injection_fails(self):
        cases = ((INJECT, (read(0.5), read(90.0)), "never shown"),
                 (INJECT, (read(0.5), read(40.2), read(41.0, "➕")), "later than 30"),
                 ({"busy_injection_within": {**INJECT["busy_injection_within"], "max_seconds": 300}},
                  (read(0.5), read(25.2), read(26.0, "➕")), "after the turn-end"))
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
            return subprocess.CompletedProcess(argv, 0 if tmux_up else 1, "", "")
        if herdr_rows is None:
            raise subprocess.CalledProcessError(1, argv)
        return subprocess.CompletedProcess(argv, 0, json.dumps({"executions": herdr_rows}), "")
    return run


class SessionHost(unittest.TestCase):
    def context(self):
        return surface.surface_context(base_url="http://127.0.0.1:1", provider="claude", agent="a", channel_id=THREAD,
                                       parent_channel_id=PARENT, thread=True, herdr_status_bin="agentdesk")

    def check(self, spec, health, run, before=None):
        record = {"surface_context": self.context(), "session_probe_before": before,
                  "prompt_sends": [{"send_state": "acknowledged"}]}
        with mock.patch.object(driver, "_read_api_json", return_value=(200, health)), \
                mock.patch.object(surface.subprocess, "run", run):
            return judge(spec, record)

    def test_thread_must_run_on_the_parent_configured_host(self):
        spec = {"session_host": {"expect": "configured"}}
        herdr_row = [{"channel": THREAD, "state": "bound"}]
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


if __name__ == "__main__":
    unittest.main()
