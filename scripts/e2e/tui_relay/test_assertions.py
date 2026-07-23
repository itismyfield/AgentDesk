"""Unit tests for relay E2E assertion primitives (#2838 P0-2).

These cover the completeness / ordering / duplicate-marker / latency
primitives that close the presence-only blind spot of the legacy contract.
"""

from __future__ import annotations

import sys
import unittest
import datetime as dt
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

from tui_relay import assertions  # noqa: E402


def _relay_msg(msg_id: int, content: str, ts: str | None = None) -> dict:
    """A bot post that qualifies as ADK relay output (not our driver, not chrome)."""

    message = {
        "id": str(msg_id),
        "content": content,
        "author": {"id": "999", "bot": True},
        "type": 0,
    }
    if ts is not None:
        message["timestamp"] = ts
    return message


def _reply_msg(msg_id: int, content: str, ts: str | None = None) -> dict:
    message = _relay_msg(msg_id, content, ts=ts)
    message["type"] = 19
    return message


def _system_msg(msg_id: int, content: str) -> dict:
    message = _relay_msg(msg_id, content)
    message["type"] = 7
    return message


def _raw_bot_msg(msg_id: int, content: str, ts: str | None = None) -> dict:
    message = {
        "id": str(msg_id),
        "content": content,
        "author": {"id": "999", "bot": True},
        "type": 0,
    }
    if ts is not None:
        message["timestamp"] = ts
    return message


def _our_msg(msg_id: int, content: str) -> dict:
    return {
        "id": str(msg_id),
        "content": content,
        "author": {"id": assertions.OUR_BOT_ID, "bot": True},
        "type": 0,
    }


def _window(*messages: dict) -> assertions.Window:
    window = assertions.Window(setup_marker_id="setup")
    for message in messages:
        window.add(message)
    return window


class OrderedTextPresent(unittest.TestCase):
    def test_passes_in_order_across_messages(self):
        window = _window(_relay_msg(1, "alpha part"), _relay_msg(2, "beta part"))
        assertions.ordered_text_present(window, needles=["alpha", "beta"])

    def test_passes_in_order_same_message(self):
        window = _window(_relay_msg(1, "alpha then beta"))
        assertions.ordered_text_present(window, needles=["alpha", "beta"])

    def test_fails_out_of_order(self):
        window = _window(_relay_msg(1, "beta"), _relay_msg(2, "alpha"))
        with self.assertRaises(assertions.AssertionError):
            assertions.ordered_text_present(window, needles=["alpha", "beta"])

    def test_fails_when_fragment_missing(self):
        window = _window(_relay_msg(1, "alpha"))
        with self.assertRaises(assertions.AssertionError):
            assertions.ordered_text_present(window, needles=["alpha", "beta"])


class NoDuplicateMarker(unittest.TestCase):
    def test_single_marker_passes(self):
        window = _window(_relay_msg(1, "the answer [E2E:T1]"))
        assertions.no_duplicate_marker(window, marker="[E2E:T1]")

    def test_duplicate_with_differing_body_fails(self):
        # Same E2E marker, different surrounding text → no_duplicate_content
        # (byte-identical only) would miss this re-emit; no_duplicate_marker
        # must catch it.
        window = _window(
            _relay_msg(1, "answer one [E2E:T1]"),
            _relay_msg(2, "answer one (resent) [E2E:T1]"),
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.no_duplicate_marker(window, marker="[E2E:T1]")
        # Confirm the legacy assertion is indeed blind to this case.
        assertions.no_duplicate_content(window)


class BodyComplete(unittest.TestCase):
    def test_complete_body_passes(self):
        window = _window(_relay_msg(1, "START middle END"))
        assertions.body_complete(window, head="START", tail="END")

    def test_truncated_tail_fails(self):
        window = _window(_relay_msg(1, "START middle"))
        with self.assertRaises(assertions.AssertionError):
            assertions.body_complete(window, head="START", tail="END")


class RelayLatency(unittest.TestCase):
    def test_within_budget_passes(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00.000000+00:00"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:02.000000+00:00"),
        )
        assertions.relay_latency_within(window, max_seconds=5)

    def test_exceeds_budget_fails(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00.000000+00:00"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:30.000000+00:00"),
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.relay_latency_within(window, max_seconds=5)

    def test_zulu_suffix_timestamp_parsed(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00Z"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:01Z"),
        )
        assertions.relay_latency_within(window, max_seconds=5)

    def test_single_message_is_noop(self):
        window = _window(_relay_msg(1, "only", ts="2026-05-29T00:00:00Z"))
        assertions.relay_latency_within(window, max_seconds=0)

    def test_single_message_uses_prompt_start_when_available(self):
        window = _window(_relay_msg(1, "only", ts="2026-05-29T00:00:02Z"))
        window.mark_prompt_sent(dt.datetime.fromisoformat("2026-05-29T00:00:00+00:00"))
        assertions.relay_latency_within(window, max_seconds=3)
        with self.assertRaises(assertions.AssertionError):
            assertions.relay_latency_within(window, max_seconds=1)

    def test_prompt_start_without_post_prompt_relay_fails(self):
        window = _window(_relay_msg(1, "old", ts="2026-05-29T00:00:00Z"))
        window.mark_prompt_sent(dt.datetime.fromisoformat("2026-05-29T00:00:10+00:00"))
        with self.assertRaisesRegex(
            assertions.AssertionError, "could not be measured"
        ):
            assertions.relay_latency_within(window, max_seconds=30)

    def test_multi_turn_uses_each_prompt_start(self):
        window = _window(
            _relay_msg(1, "first", ts="2026-05-29T00:00:01Z"),
            _relay_msg(2, "second", ts="2026-05-29T00:00:50Z"),
        )
        window.mark_prompt_sent(dt.datetime.fromisoformat("2026-05-29T00:00:00+00:00"))
        window.mark_prompt_sent(dt.datetime.fromisoformat("2026-05-29T00:00:10+00:00"))
        assertions.relay_latency_within(window, max_seconds=45)
        with self.assertRaises(assertions.AssertionError):
            assertions.relay_latency_within(window, max_seconds=30)


class RawChromeAndEditAssertions(unittest.TestCase):
    def test_direct_input_reply_body_counts_as_relay_response(self):
        window = _window(
            _reply_msg(1, "[E2E:E21:HEAD]\nDIRECT_E21_OK\n[E2E:E21:TAIL]")
        )

        self.assertEqual(len(window.raw_messages), 1)
        self.assertEqual(len(window.messages), 1)
        assertions.text_present(window, needle="[E2E:E21:HEAD]")
        assertions.text_present(window, needle="DIRECT_E21_OK")
        assertions.text_present(window, needle="[E2E:E21:TAIL]")
        assertions.ordered_text_present(
            window,
            needles=["[E2E:E21:HEAD]", "DIRECT_E21_OK", "[E2E:E21:TAIL]"],
        )
        assertions.body_complete(
            window, head="[E2E:E21:HEAD]", tail="[E2E:E21:TAIL]"
        )

    def test_status_reply_and_non_reply_system_messages_stay_out_of_relay_surface(self):
        window = _window(
            _reply_msg(1, "✅ 응답 완료 [E2E:E21:TAIL]"),
            _system_msg(2, "[E2E:E21:TAIL]"),
        )

        self.assertEqual(len(window.raw_messages), 2)
        self.assertEqual(window.messages, [])
        assertions.raw_text_present(window, needle="[E2E:E21:TAIL]")
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle="[E2E:E21:TAIL]")

    def test_window_updates_same_message_id_to_final_body(self):
        window = _window(_raw_bot_msg(1, "Processing..."))
        window.add(_relay_msg(1, "final [E2E:EDIT]", ts="2026-05-29T00:00:00Z"))

        self.assertEqual(len(window.raw_messages), 1)
        self.assertEqual(window.raw_messages[0]["content"], "final [E2E:EDIT]")
        self.assertEqual(len(window.messages), 1)
        self.assertEqual(len(window.message_updates), 1)
        assertions.text_present(window, needle="[E2E:EDIT]")

    def test_body_not_overwritten_uses_final_non_own_raw_body(self):
        window = _window(
            _our_msg(1, "prompt contains [E2E:BODY]"),
            _relay_msg(2, "answer [E2E:BODY]"),
        )
        assertions.body_not_overwritten(window, marker="[E2E:BODY]")
        window.add(_raw_bot_msg(2, "SUPPRESSED_INTERNAL_LABEL"))
        with self.assertRaises(assertions.AssertionError):
            assertions.body_not_overwritten(window, marker="[E2E:BODY]")
        with self.assertRaises(assertions.AssertionError):
            assertions.no_suppressed_label_chrome(window)

    def test_raw_text_absent_and_marker_absent(self):
        window = _window(
            _our_msg(1, "prompt [LATE]"),
            _raw_bot_msg(2, "✅ 응답 완료"),
            _relay_msg(3, "body [OK]"),
        )
        assertions.raw_text_absent(window, needle="[LATE]")
        assertions.marker_absent(window, marker="[LATE]")
        assertions.marker_absent(window, marker="✅", surface="relay")
        with self.assertRaises(assertions.AssertionError):
            assertions.marker_absent(window, marker="[OK]")
        with self.assertRaises(assertions.AssertionError):
            assertions.raw_text_absent(window, needle="✅")

    def test_raw_message_count_between_markers_counts_chrome(self):
        window = _window(
            _our_msg(1, "prompt"),
            _raw_bot_msg(2, "✅ 응답 완료"),
            _relay_msg(3, "body"),
        )
        assertions.raw_message_count_between_markers(window, low=2, high=2)
        assertions.raw_message_count_between_markers(
            window, low=3, high=3, include_our_send=True
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.raw_message_count_between_markers(window, low=1, high=1)

    def test_chrome_count_exact_text_and_regex(self):
        window = _window(
            _raw_bot_msg(1, "✅ 응답 완료"),
            _raw_bot_msg(2, "✅ 응답 완료"),
            _relay_msg(3, "body"),
        )
        assertions.chrome_count(window, text="응답 완료", exact=2)
        assertions.chrome_count(window, regex=r"^✅", min_count=2, max_count=2)
        with self.assertRaises(assertions.AssertionError):
            assertions.chrome_count(window, text="응답 완료", exact=1)

    def test_status_panel_after_body(self):
        good = _window(
            _relay_msg(10, "body [BODY]"),
            _raw_bot_msg(20, "Processing..."),
        )
        assertions.status_panel_after_body(good, body_marker="[BODY]")

        stranded = _window(
            _raw_bot_msg(10, "Processing..."),
            _relay_msg(20, "body [BODY]"),
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.status_panel_after_body(stranded, body_marker="[BODY]")

        missing = _window(_relay_msg(10, "body [BODY]"))
        with self.assertRaises(assertions.AssertionError):
            assertions.status_panel_after_body(missing, body_marker="[BODY]")

    def test_single_status_panel(self):
        good = _window(_raw_bot_msg(10, "Processing..."))
        assertions.single_status_panel(good)
        self.assertEqual(assertions.latest_status_panel(good)["id"], "10")

        ordered = _window(
            _raw_bot_msg(10, "Processing..."),
            _raw_bot_msg(20, "✅ 응답 완료"),
        )
        self.assertEqual(assertions.latest_status_panel(ordered)["id"], "20")

        duplicate = _window(
            _raw_bot_msg(10, "Processing..."),
            _raw_bot_msg(20, "🟢 진행 중"),
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.single_status_panel(duplicate)

    def test_completion_chrome_after_body(self):
        window = _window(
            _relay_msg(1, "body [BODY]"),
            _raw_bot_msg(2, "✅ 응답 완료"),
        )
        assertions.completion_chrome_after_body(window, body_marker="[BODY]")
        assertions.completion_chrome_after_body(
            window, body_marker="[BODY]", required=True
        )

        bad = _window(
            _raw_bot_msg(1, "✅ 응답 완료"),
            _relay_msg(2, "body [BODY]"),
        )
        with self.assertRaises(assertions.AssertionError):
            assertions.completion_chrome_after_body(bad, body_marker="[BODY]")

        no_completion = _window(_relay_msg(1, "body [BODY]"))
        assertions.completion_chrome_after_body(no_completion, body_marker="[BODY]")
        with self.assertRaises(assertions.AssertionError):
            assertions.completion_chrome_after_body(
                no_completion, body_marker="[BODY]", required=True
            )


class RunAssertionDispatch(unittest.TestCase):
    """The YAML `run_assertion` dispatch must route the new spec keys, and every
    assertion spec used by a checked-in scenario must be dispatchable (no
    'unknown assertion' / 'bad assertion spec')."""

    def setUp(self):
        import run_tui_relay  # noqa: PLC0415

        self.run_assertion = run_tui_relay.run_assertion

    def test_ordered_text_present_dispatch(self):
        window = _window(_relay_msg(1, "a"), _relay_msg(2, "b"))
        self.run_assertion({"ordered_text_present": ["a", "b"]}, window=window)
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"ordered_text_present": ["b", "a"]}, window=window)

    def test_feature_required_assertion_is_skipped_until_enabled(self):
        spec = {
            "requires_feature": "two_message_panel",
            "status_panel_after_body": {"body_marker": "[BODY]"},
        }
        empty = _window(_relay_msg(1, "body [BODY]"))
        self.run_assertion(spec, window=empty)
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion(
                spec,
                window=empty,
                enabled_features=frozenset({"two_message_panel"}),
            )

    def test_no_duplicate_marker_dispatch(self):
        window = _window(_relay_msg(1, "x [M]"), _relay_msg(2, "y [M]"))
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"no_duplicate_marker": "[M]"}, window=window)

    def test_body_complete_dispatch(self):
        window = _window(_relay_msg(1, "H mid T"))
        self.run_assertion({"body_complete": {"head": "H", "tail": "T"}}, window=window)
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"body_complete": {"head": "H", "tail": "ZZZ"}}, window=window)

    def test_relay_latency_within_dispatch_dict_and_scalar(self):
        window = _window(
            _relay_msg(1, "a", ts="2026-05-29T00:00:00Z"),
            _relay_msg(2, "b", ts="2026-05-29T00:00:01Z"),
        )
        self.run_assertion({"relay_latency_within": {"max_seconds": 5}}, window=window)
        self.run_assertion({"relay_latency_within": 5}, window=window)

    def test_raw_and_chrome_dispatch(self):
        window = _window(
            _our_msg(1, "prompt [LATE]"),
            _relay_msg(2, "body [BODY]"),
            _raw_bot_msg(3, "✅ 응답 완료"),
        )
        self.run_assertion(
            {"raw_message_count_between_markers": {"min": 2, "max": 2}},
            window=window,
        )
        self.run_assertion({"raw_text_absent": "[LATE]"}, window=window)
        self.run_assertion({"marker_absent": {"marker": "[LATE]"}}, window=window)
        self.run_assertion({"chrome_count": {"text": "응답 완료", "exact": 1}}, window=window)
        self.run_assertion(
            {
                "status_panel_after_body": {
                    "body_marker": "[BODY]",
                    "panel_regex": r"^✅",
                }
            },
            window=window,
        )
        self.run_assertion(
            {"single_status_panel": {"panel_regex": r"^✅"}},
            window=window,
        )
        self.run_assertion(
            {"completion_chrome_after_body": {"body_marker": "[BODY]"}},
            window=window,
        )
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion(
                {
                    "completion_chrome_after_body": {
                        "body_marker": "[BODY]",
                        "required": True,
                    }
                },
                window=_window(_relay_msg(1, "body [BODY]")),
            )
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"raw_text_absent": {"include_our_send": True}}, window=window)
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion({"marker_absent": {"surface": "raw"}}, window=window)
        self.run_assertion({"body_not_overwritten": "[BODY]"}, window=window)
        self.run_assertion({"no_suppressed_label_chrome": True}, window=window)

    def test_provider_hold_marker_seen_dispatch_uses_record_not_relay(self):
        window = _window()
        record = {
            "provider_hold_states": [
                {
                    "ok_marker": "[E2E:E18:OK]",
                    "ok_marker_seen": True,
                    "late_marker": "[E2E:E18:LATE]",
                    "late_marker_seen": False,
                }
            ]
        }

        self.run_assertion(
            {"provider_hold_marker_seen": "[E2E:E18:OK]"},
            window=window,
            record=record,
        )
        with self.assertRaises(assertions.AssertionError):
            self.run_assertion(
                {"provider_hold_marker_seen": "[E2E:E18:OTHER]"},
                window=window,
                record=record,
            )

    def test_fixture_assertion_dispatch_uses_record_state(self):
        window = _window(_relay_msg(1, "[E2E:E25:FINAL]"))
        record = {
            "fixture_state": {
                "task_notification_kind": "Background",
                "task_notification_source": "CronCreate",
                "task_notification_status": "completed",
                "task_complete_seen": True,
                "task_complete_turn_id": "turn-1",
                "result_text_source": "task_complete.last_agent_message",
                "finalized": True,
                "active_turn": "none",
                "followup_ready": True,
                "followup_probe_accepted": True,
                "queue_depth": 0,
                "pending_discord_callback": False,
            },
            "fixture_health": {
                "status": "healthy",
                "degraded_reasons": [],
                "active_turn": "none",
                "queue_depth": 0,
                "pending_discord_callback": False,
                "stale_thread_proof": False,
                "relay_stall_state": "healthy",
            },
        }

        self.run_assertion(
            {
                "fixture_task_notification": {
                    "kind": "Background",
                    "source": "CronCreate",
                    "status": "completed",
                }
            },
            window=window,
            record=record,
        )
        self.run_assertion({"fixture_finalized": {"active_turn": "none"}}, window=window, record=record)
        self.run_assertion({"fixture_followup_ready": True}, window=window, record=record)
        self.run_assertion({"fixture_no_health_degradation": True}, window=window, record=record)
        self.run_assertion(
            {
                "fixture_task_complete_finalized": {
                    "turn_id": "turn-1",
                    "result_text_source": "task_complete.last_agent_message",
                }
            },
            window=window,
            record=record,
        )
        self.run_assertion(
            {"fixture_state": {"followup_probe_accepted": True}},
            window=window,
            record=record,
        )

        with self.assertRaises(assertions.AssertionError):
            self.run_assertion(
                {"fixture_task_complete_finalized": {"turn_id": "other"}},
                window=window,
                record=record,
            )

    def test_every_scenario_assertion_spec_is_dispatchable(self):
        import glob  # noqa: PLC0415

        import yaml  # noqa: PLC0415

        window = _window(_relay_msg(1, "placeholder body"))
        scenarios = sorted(glob.glob(str(ROOT / "tests/e2e/tui_relay/scenarios/*.yaml")))
        self.assertTrue(scenarios, "no scenario YAMLs found")
        for path in scenarios:
            with open(path, encoding="utf-8") as handle:
                data = yaml.safe_load(handle)
            for spec in data.get("assertions") or []:
                try:
                    self.run_assertion(spec, window=window)
                except assertions.AssertionError as error:
                    # A scenario assertion may legitimately fail against this
                    # synthetic window (e.g. text_present), but it must never be
                    # an unrouted spec.
                    message = str(error)
                    self.assertNotIn("unknown assertion", message, f"{path}: {spec}")
                    self.assertNotIn("bad assertion spec", message, f"{path}: {spec}")


if __name__ == "__main__":
    unittest.main()
