"""Unit tests for relay E2E assertion primitives (#2838 P0-2).

These cover the completeness / ordering / duplicate-marker / latency
primitives that close the presence-only blind spot of the legacy contract.
"""

from __future__ import annotations

import contextlib
import copy
import io
import json
import subprocess
import sys
import tempfile
import unittest
import datetime as dt
from argparse import Namespace
from dataclasses import dataclass
from pathlib import Path
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_tui_relay as driver  # noqa: E402
import run_multi_provider_matrix as matrix  # noqa: E402
from tui_relay import assertions  # noqa: E402
from tui_relay.test_driver_health import (  # noqa: E402
    _busy_mailbox, _fake_urlopen_for, _health_detail, _idle_mailbox,
    HarnessOutcomeContract, PhasePartialEvidenceContract,  # noqa: F401
)
from tui_relay import normal_intake_evidence as e36  # noqa: E402
# ci-script-checks.sh runs this module; expose the offline fetch regressions too.
from tui_relay.test_discord_client import DiscordClientFetchMessages, _Response  # noqa: E402, F401
from tui_relay.test_known_gap import E22KnownGapContract  # noqa: E402, F401


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


class E36EvidenceContract(unittest.TestCase):
    def test_native_input_work_join_rejects_repeat_wrong_session_and_missing_work(self):
        prompt, marker = "Reply only [E36:run:S01]", "[E36:run:S01]"
        user = {"type": "user", "sessionId": "session", "uuid": "input-1", "message": {"content": prompt}}
        work = {"type": "assistant", "sessionId": "session", "uuid": "work-1", "message": {"content": marker}}
        tool = {"type": "user", "message": {"content": [{"type": "tool_result", "content": prompt}]}}
        request = {"prompt": prompt, "body_marker": marker}
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "native.jsonl"
            for name, rows, error in (
                ("valid", [user, tool, work], None), ("repeat", [user, work, user, work], assertions.AssertionError),
                ("foreign", [{**user, "sessionId": "foreign"}, work], assertions.AssertionError),
                ("no_input", [work], ValueError), ("no_work", [user, tool], ValueError)):
                with self.subTest(case=name):
                    path.write_text("")
                    before = e36.cursor(path)
                    path.write_text("".join(json.dumps(r) + "\n" for r in rows))
                    if error:
                        with self.assertRaises(error):
                            e36.native_chain(path, before, request, "session")
                    else:
                        result = e36.native_chain(path, before, request, "session")
                        self.assertEqual(result["input_id"], "input-1")
                        self.assertEqual(result["work"][0]["id"], "work-1")

    def test_strict_native_reader_preserves_partial_suffix_and_rejects_lost_generation(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "native"
            path.write_text("")
            before = e36.cursor(path)
            path.write_text('{"type":"user"}\n{')
            self.assertEqual(len(list(e36.tail(path, before))), 1)
            replacement = Path(root) / "replacement"
            replacement.write_text('{"type":"user"}\n')
            replacement.replace(path)
            with self.assertRaisesRegex(ValueError, "generation"):
                list(e36.tail(path, before))
            before = {**e36.cursor(path), "offset": 0}
            path.write_text('not-json\n')
            with self.assertRaises(ValueError):
                e36.native_chain(path, before, {"prompt": "request", "body_marker": "reply"}, "session")

    def test_actual_queue_consumer_distinguishes_commit_refusal_and_unavailable_log(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "intake"
            reader = object.__new__(e36.Evidence)
            reader.log, reader.channel = path, "42"
            reader.watcher = lambda: {"mailbox_active_user_msg_id": 100}
            for name, fields, error in (
                ("accepted", 'message_id=200 source="busy_active_turn" outcome="enqueued" persistence_error="none"', None),
                ("refused", 'message_id=200 source="busy_active_turn" outcome="refused" persistence_error="none"', assertions.AssertionError),
                ("wrong_source", 'message_id=200 source="drain_mode" outcome="enqueued" persistence_error="none"', assertions.AssertionError),
                ("wrong_id", 'message_id=201 source="busy_active_turn" outcome="enqueued" persistence_error="none"', ValueError),
                ("unavailable", None, ValueError)):
                with self.subTest(case=name):
                    clock = [0.0]
                    reader.d = SimpleNamespace(time=SimpleNamespace(monotonic=lambda: clock[0],
                        sleep=lambda t: clock.__setitem__(0, clock[0] + t)))
                    path.write_text("")
                    request = {"inbound_message_id": "200", "log_before": e36.cursor(path)}
                    if fields:
                        path.write_text(f'\x1b[32mINFO\x1b[0m {e36.LOG_TARGET}: {e36.LOG_MESSAGE} channel_id=42 {fields}\n')
                    prior = {"inbound_message_id": "100", "deadline": 20}
                    if error:
                        with self.assertRaises(error):
                            reader.queue(request, prior)
                    else:
                        reader.queue(request, prior)
                        self.assertEqual(request["queue"]["active_prior_message_id"], "100")
                    if name in ("wrong_id", "unavailable"):
                        self.assertNotIn("queue", request)

    def test_request_window_filters_prior_and_shared_chrome_and_detects_deleted_body(self):
        global_window = _window(_relay_msg(11, "old"), _raw_bot_msg(12, "✅ 응답 완료"),
                                _relay_msg(21, "new"), _raw_bot_msg(22, "✅ 응답 완료"))
        request = {"discord_before_id": 20, "discord_closed_after_id": 22, "body_marker": "new"}
        sub = e36.request_window(global_window, request)
        self.assertEqual(e36.completion_id(sub, request), "22")
        with self.assertRaises(assertions.AssertionError):
            e36.completion_id(e36.request_window(global_window, request, [_raw_bot_msg(22, "✅ 응답 완료")]), request)
        reused = {"discord_before_id": 22, "body_marker": "later"}
        with self.assertRaises(assertions.AssertionError):
            e36.completion_id(e36.request_window(global_window, reused), reused)

    def publication_fixture(self):
        rows, requests = [], []
        for index, key in enumerate(e36.KEYS, 1):
            mid, marker = index * 10, f"[E36:current:{key}]"
            hold = f"[E36:current:{key}:HOLD]" if key == "QA" else None
            rows.extend([_relay_msg(mid + 1, f"{hold}\n{marker}" if hold else marker),
                         _raw_bot_msg(mid + 2, "✅ 응답 완료")])
            requests.append({"request_key": key, "body_marker": marker, "discord_before_id": mid,
                             "discord_closed_after_id": mid + 2, "inbound_message_id": str(mid),
                             "response_ids": [str(mid + 1)], "completion_message_id": str(mid + 2),
                             "native": {"input_id": f"native-{key}"}, "admission": {"id": mid},
                             **({"hold_marker": hold} if hold else {})})
        requests[10]["hold_native"], requests[11]["queue"] = {"input": "QA"}, {"id": "QB"}
        record = {"discord_prompt_records": requests, "e36_acceptance": {}}
        return rows, record

    def test_final_global_publication_detects_late_different_header_duplicates(self):
        rows, record = self.publication_fixture()
        requests = record["discord_prompt_records"]
        e36.final_assertion(_window(*rows), record, rows)  # unchanged positive control
        late = _relay_msg(999, "different publication header\n" + requests[0]["body_marker"])
        self.assertGreater(int(late["id"]), requests[0]["discord_closed_after_id"])
        for name, observed, fresh in (
            ("late_both", rows + [late], rows + [late]),
            ("late_observed_only", rows + [late], rows),
            ("late_fresh_only", rows, rows + [late]),
            ("deleted", rows, rows[1:]),
            ("replacement_id", rows, rows[1:] + [late])):
            with self.subTest(case=name), self.assertRaises(assertions.AssertionError):
                e36.final_assertion(_window(*observed), copy.deepcopy(record), fresh)

    def test_observed_duplicate_cannot_disappear_through_message_edit(self):
        rows, record = self.publication_fixture()
        marker = record["discord_prompt_records"][0]["body_marker"]
        for same_id in (False, True):
            with self.subTest(same_id=same_id):
                window = _window(*rows)
                mid = 11 if same_id else 999
                window.add(_relay_msg(mid, marker + "\n" + marker if same_id else "different header\n" + marker))
                window.add(rows[0] if same_id else _relay_msg(mid, "edited away"))
                self.assertTrue(window.message_updates)
                with self.assertRaisesRegex(assertions.AssertionError, "observed edited duplicate"):
                    e36.final_assertion(window, copy.deepcopy(record), window.raw_messages)
        positive = _window(*rows)
        positive.add(_relay_msg(11, "valid edit\n" + marker))
        positive.add(rows[0])
        positive.add(_our_msg(999, marker))
        positive.add(_our_msg(999, "driver edit"))
        e36.final_assertion(positive, copy.deepcopy(record), positive.raw_messages)

    def test_late_qa_observation_keeps_the_exact_commit_as_the_ownership_evidence(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "intake"
            reader = object.__new__(e36.Evidence)
            reader.log, reader.channel = path, "42"
            reader.d = SimpleNamespace(time=SimpleNamespace(monotonic=lambda: 0, sleep=MagicMock()))
            for active in (200, None):
                with self.subTest(active=active):
                    path.write_text("")
                    request = {"inbound_message_id": "200", "log_before": e36.cursor(path)}
                    path.write_text(f'INFO {e36.LOG_TARGET}: {e36.LOG_MESSAGE} channel_id=42 message_id=200 '
                                    'source="busy_active_turn" outcome="enqueued" persistence_error="none"\n')
                    def watcher():
                        self.assertEqual(request["queue"]["message_id"], "200")
                        return {"mailbox_active_user_msg_id": active}
                    reader.watcher = watcher
                    reader.queue(request, {"inbound_message_id": "100", "deadline": 20})
                    self.assertEqual(request["queue"]["outcome"], "enqueued")
                    self.assertEqual(request["queue"]["active_prior_message_id"], "100")
                    self.assertEqual(request["queue_observation"]["active_message_id"], active)
                    self.assertFalse(request["queue_observation"]["active_owner_sampled"])
            path.write_text("")
            request = {"inbound_message_id": "200", "log_before": e36.cursor(path)}
            reader.watcher = lambda: {"mailbox_active_user_msg_id": 999}
            with self.assertRaisesRegex(ValueError, "before any QB commit"):
                reader.queue(request, {"inbound_message_id": "100", "deadline": 20})
            self.assertNotIn("queue", request)

    def test_rowless_idle_reads_null_unread_bytes_as_drained_but_a_row_as_unmeasured(self):
        from types import SimpleNamespace
        rowless = {"unread_bytes": None, "inflight_state_present": False, "has_pending_queue": False}
        self.assertTrue(e36.drained(rowless))
        self.assertFalse(e36.drained({**rowless, "has_pending_queue": True}))
        self.assertFalse(e36.drained({"unread_bytes": 12, "inflight_state_present": True, "has_pending_queue": False}))
        for state in ({**rowless, "inflight_state_present": True}, {"unread_bytes": None, "has_pending_queue": False}):
            with self.assertRaisesRegex(ValueError, "unmeasured"):
                e36.drained(state)
        reader = object.__new__(e36.Evidence)
        reader.args, reader.channel, reader.root = Namespace(base_url="http://offline.invalid", cell="claude-tui"), "42", Path("/offline")
        reader.d = SimpleNamespace(assert_cell_idle=lambda **kwargs: {"status": "idle"})
        reader.watcher = lambda: rowless
        self.assertEqual(reader.idle(), {"status": "idle", "watcher": rowless})
        reader.watcher = lambda: {**rowless, "has_pending_queue": True}
        with self.assertRaises(assertions.AssertionError):
            reader.idle()

    def test_tool_result_user_rows_with_hook_text_do_not_end_the_work_scan(self):
        prompt, marker = "Reply only [E36:run:S01]", "[E36:run:S01]"
        user = {"type": "user", "sessionId": "s", "uuid": "input-1", "message": {"content": prompt}}
        hooked = {"type": "user", "sessionId": "s", "message": {"content": [
            {"type": "tool_result", "content": "ok"},
            {"type": "text", "text": "<system-reminder>hook additional context</system-reminder>"}]}}
        following = {"type": "user", "sessionId": "s", "uuid": "next", "message": {"content": "a later prompt"}}
        work = {"type": "assistant", "sessionId": "s", "uuid": "work-1", "message": {"content": marker}}
        request = {"prompt": prompt, "body_marker": marker}
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "native.jsonl"
            for name, rows, error in (("hook_text", [user, hooked, work], None),
                                      ("new_input", [user, following, work], ValueError)):
                with self.subTest(case=name):
                    path.write_text("")
                    before = e36.cursor(path)
                    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
                    if error:
                        with self.assertRaises(error):
                            e36.native_chain(path, before, request, "s")
                    else:
                        self.assertEqual(e36.native_chain(path, before, request, "s")["work"][0]["id"], "work-1")

    def test_stranded_qa_hold_prefix_counts_as_a_duplicate_publication(self):
        rows, record = self.publication_fixture()
        stray = _relay_msg(999, record["discord_prompt_records"][10]["hold_marker"])
        for name, observed, fresh in (("both", rows + [stray], rows + [stray]),
                                      ("observed_only", rows + [stray], rows)):
            with self.subTest(case=name), self.assertRaisesRegex(assertions.AssertionError, "publication"):
                e36.final_assertion(_window(*observed), copy.deepcopy(record), fresh)

    def test_completion_outside_every_closed_request_window_is_not_dropped(self):
        rows, record = self.publication_fixture()
        stray = _raw_bot_msg(999, "✅ 응답 완료")
        for name, observed, fresh in (("both", rows + [stray], rows + [stray]),
                                      ("observed_only", rows + [stray], rows),
                                      ("fresh_only", rows, rows + [stray])):
            with self.subTest(case=name), self.assertRaisesRegex(assertions.AssertionError, "unattributed completion"):
                e36.final_assertion(_window(*observed), copy.deepcopy(record), fresh)
        # An edit cannot retire chrome we already observed and stored.
        edited = _window(*rows, stray)
        edited.add({**stray, "content": "edited away"})
        self.assertTrue(edited.message_updates)
        with self.subTest(case="edited_away"), self.assertRaisesRegex(assertions.AssertionError, "unattributed completion"):
            e36.final_assertion(edited, copy.deepcopy(record), edited.raw_messages)
        ours = _window(*rows, _our_msg(998, "✅ 응답 완료"))
        ours.add(_our_msg(998, "driver edit"))  # our own driver edits are not product chrome
        e36.final_assertion(ours, copy.deepcopy(record), ours.raw_messages)

    def test_qa_hold_prefix_may_land_on_its_own_id_but_never_on_two(self):
        rows, record = self.publication_fixture()
        qa = record["discord_prompt_records"][10]
        hold, marker = qa["hold_marker"], qa["body_marker"]
        # #5731 contract B: message1 keeps the PRE prefix, message2 carries the terminal body.
        split = [row for row in rows if str(row["id"]) not in ("111", "112")] + [
            _relay_msg(111, hold), _relay_msg(113, marker), _raw_bot_msg(114, "✅ 응답 완료")]
        qa.update(response_ids=["113"], completion_message_id="114", discord_closed_after_id=114)
        e36.final_assertion(_window(*split), copy.deepcopy(record), split)
        # E-22 shape: the prefix is stranded on one ID while a second ID republishes it.
        stranded = split + [_relay_msg(115, f"{hold}\nrepublished")]
        with self.assertRaisesRegex(assertions.AssertionError, "hold publication"):
            e36.final_assertion(_window(*stranded), copy.deepcopy(record), stranded)

    def test_two_ordered_turns_on_one_page_are_attributed_not_called_a_queue_bypass(self):
        from types import SimpleNamespace
        qa = {"prompt": "ask QA", "body_marker": "[QA]", "discord_before_id": 1000}
        qb = {"prompt": "ask QB", "body_marker": "[QB]"}
        window = _window(_relay_msg(1001, "[QA]"), _raw_bot_msg(1003, "✅ 응답 완료"),
                         _relay_msg(1004, "[QB]"), _raw_bot_msg(1005, "✅ 응답 완료"))
        row = lambda kind, text: {"type": kind, "sessionId": "s", "message": {"content": text}}  # noqa: E731
        ordered = [row("user", "ask QA"), row("assistant", "[QA]"), row("user", "ask QB"), row("assistant", "[QB]")]
        early = [row("user", "ask QA"), row("user", "ask QB"), row("assistant", "[QB]"), row("assistant", "[QA]")]
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "native.jsonl"
            path.write_text("")
            qa["native_before"] = qb["native_before"] = e36.cursor(path)
            evidence = SimpleNamespace(path=path, session="s", d=SimpleNamespace(HarnessEvidenceError=RuntimeError))
            narrow = e36.sequential_boundary(evidence, qa, qb)
            for name, native, error in (("sequential", ordered, None), ("early", early, assertions.AssertionError),
                                        ("no_native_order", ordered[:1], RuntimeError)):
                with self.subTest(case=name):
                    path.write_text("".join(json.dumps(entry) + "\n" for entry in native))
                    request = dict(qa)
                    self.assertEqual(len(e36.completion_candidates(e36.request_window(window, request))), 2)
                    if error:
                        with self.assertRaises(error):
                            narrow(window, request)
                        continue
                    narrow(window, request)
                    self.assertEqual(request["queued_followup_order"]["qb_first_body_id"], 1004)
                    self.assertEqual(request["discord_closed_after_id"], 1003)
                    request["response_ids"] = ["1001"]
                    self.assertEqual(e36.completion_id(e36.request_window(window, request), request), "1003")


def _wait_predicate(window: assertions.Window, needle: str) -> bool:
    """Mirror the driver's per-message relay wait predicate."""

    return any(
        (body := assertions.relay_body(message)) is not None and needle in body
        for message in window.raw_messages
    )


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


class SessionAndCompletionChromeRegression(unittest.TestCase):
    """Pin the wire shapes that the post-deploy E-1 smoke actually observes."""

    MARKER = "[E2E:E1:OK]"
    RESUMED_BANNER = "기존 세션 복원 · provider session claude#anon…"

    def test_resumed_banner_and_body_stay_one_relay_response(self):
        message = _raw_bot_msg(1, f"{self.RESUMED_BANNER}\n\n{self.MARKER}")
        completion = _raw_bot_msg(2, "-# ✅ 완료\n-# 턴 시작 : anonymized")

        self.assertEqual(assertions.is_relay_response(message), True)
        window = _window(message, completion)
        self.assertEqual(window.messages, [message])
        self.assertEqual(_wait_predicate(window, self.MARKER), True)
        assertions.text_present(window, needle=self.MARKER)
        assertions.completion_chrome_after_body(
            window, body_marker=self.MARKER, required=True
        )

    def test_fresh_banner_and_body_stay_one_relay_response(self):
        message = _raw_bot_msg(1, f"🆕 새 세션 시작\n\n{self.MARKER}")

        self.assertEqual(assertions.is_relay_response(message), True)
        window = _window(message)
        self.assertEqual(_wait_predicate(window, self.MARKER), True)
        assertions.text_present(window, needle=self.MARKER)

    def test_marker_attached_with_single_newline_is_not_a_relay_body(self):
        message = _raw_bot_msg(1, f"기존 세션 복원\n{self.MARKER}")
        completion = _raw_bot_msg(2, "-# ✅ 완료\n-# 턴 시작 : anonymized")
        window = _window(message, completion)

        # The malformed banner-shaped post is still observable raw output, but
        # its marker is not evidence of a delivered answer body.
        self.assertEqual(assertions.is_relay_response(message), True)
        self.assertEqual(_wait_predicate(window, self.MARKER), False)
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle=self.MARKER)
        with self.assertRaises(assertions.AssertionError):
            assertions.completion_chrome_after_body(
                window, body_marker=self.MARKER, required=True
            )

    def test_marker_in_banner_prefix_is_not_a_relay_body(self):
        message = _raw_bot_msg(
            1,
            "기존 세션 복원 · provider session "
            f"{self.MARKER}\n\nwrong response body",
        )
        completion = _raw_bot_msg(2, "-# ✅ 완료\n-# 턴 시작 : anonymized")
        window = _window(message, completion)

        self.assertEqual(_wait_predicate(window, self.MARKER), False)
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle=self.MARKER)
        with self.assertRaises(assertions.AssertionError):
            assertions.completion_chrome_after_body(
                window, body_marker=self.MARKER, required=True
            )

    def test_marker_in_normal_body_without_banner_stays_a_relay_body(self):
        message = _raw_bot_msg(1, self.MARKER)
        completion = _raw_bot_msg(2, "-# ✅ 완료\n-# 턴 시작 : anonymized")
        window = _window(message, completion)

        self.assertEqual(_wait_predicate(window, self.MARKER), True)
        assertions.text_present(window, needle=self.MARKER)
        assertions.completion_chrome_after_body(
            window, body_marker=self.MARKER, required=True
        )

    def test_normal_body_marker_with_completion_footer_stays_body(self):
        message = _raw_bot_msg(
            1,
            f"normal answer {self.MARKER}\n\n"
            "-# ✅ 완료\n"
            "-# Tasks\n"
            "-# └ Bash finished ✓",
        )

        self.assertEqual(assertions.relay_body(message), f"normal answer {self.MARKER}")
        assertions.text_present(_window(message), needle=self.MARKER)

    def test_banner_body_marker_with_completion_footer_stays_body(self):
        message = _raw_bot_msg(
            1,
            f"{self.RESUMED_BANNER}\n\n{self.MARKER}\n\n"
            "-# ✅ 완료\n"
            "-# 턴 시작 : anonymized",
        )

        self.assertEqual(assertions.relay_body(message), self.MARKER)
        assertions.text_present(_window(message), needle=self.MARKER)

    def test_completion_footer_is_tail_anchored_after_mid_body_chrome_like_text(self):
        message = _raw_bot_msg(
            1,
            "정상 응답: 아래는 UI 예시입니다.\n\n"
            "-# ✅ 완료\n"
            f"설명 계속 {self.MARKER}\n\n"
            "-# ✅ 완료\n"
            "-# 턴 시작 : anonymized",
        )
        window = _window(message)

        expected = (
            "정상 응답: 아래는 UI 예시입니다.\n\n"
            "-# ✅ 완료\n"
            f"설명 계속 {self.MARKER}"
        )
        self.assertEqual(assertions.relay_body(message), expected)
        self.assertEqual(_wait_predicate(window, self.MARKER), True)
        assertions.text_present(window, needle=self.MARKER)
        assertions.ordered_text_present(
            window,
            needles=["정상 응답: 아래는 UI 예시입니다.", self.MARKER],
        )
        assertions.body_complete(
            window,
            head="정상 응답: 아래는 UI 예시입니다.",
            tail=self.MARKER,
        )

    def test_non_chrome_tail_keeps_resume_prompt_visible_to_body_assertion(self):
        message = _raw_bot_msg(
            1,
            f"{self.MARKER}\n\n-# ✅ 완료\nNo response requested.",
        )
        window = _window(message)

        # Marker presence alone still passes; the body-scoped #2718 check must
        # see the non-chrome tail instead of losing it to an early cut.
        self.assertEqual(assertions.relay_body(message), message["content"])
        self.assertEqual(_wait_predicate(window, self.MARKER), True)
        assertions.text_present(window, needle=self.MARKER)
        with self.assertRaisesRegex(assertions.AssertionError, "No response requested"):
            assertions.no_resume_prompt_chrome(window)

    def test_resume_chrome_inside_footer_shaped_line_is_never_stripped(self):
        # `-# └ {label} {summary}` and the icon-led metadata lines render
        # provider free text, so a forbidden string can sit on a line that is
        # otherwise footer-shaped.  Stripping it would hide #2718 chrome from
        # the body-scoped detector, so the whole suffix stops being a strip
        # candidate.
        for footer in (
            "-# Tasks\n-# └ Bash No response requested. ✓",
            "-# ⏱ No response requested.",
            "-# Task     No response requested.",
            "-# Tasks\n-# └ Bash Continue from where you left off. ✓",
        ):
            with self.subTest(footer=footer):
                message = _raw_bot_msg(1, f"{self.MARKER}\n\n{footer}")
                window = _window(message)

                self.assertEqual(assertions.relay_body(message), message["content"])
                assertions.text_present(window, needle=self.MARKER)
                with self.assertRaises(assertions.AssertionError):
                    assertions.no_resume_prompt_chrome(window)

    def test_clean_footer_of_same_shape_is_still_stripped(self):
        # The guard must key on the forbidden string, not on the footer shape:
        # the identical shapes without resume chrome still strip normally.
        for footer in (
            "-# Tasks\n-# └ Bash (3s) ✓",
            "-# ⏱ 2m 34s",
            "-# Task     빌드",
        ):
            with self.subTest(footer=footer):
                body = f"{self.MARKER}\n\n{footer}"
                self.assertEqual(
                    assertions._strip_completion_chrome_tail(body), self.MARKER
                )

    def test_real_spinner_merged_footer_shapes_are_tail_chrome(self):
        for footer in (
            "-# ⠸ 완료",
            "-# ⠸ monitor 대기",
            "-# ⠸ 진행 중",
            "⠸ 계속 처리 중",
            "-# 🟡 응답 지연 · 조사 권장",
        ):
            with self.subTest(footer=footer):
                body = f"{self.MARKER}\n\n{footer}"
                self.assertEqual(
                    assertions._strip_completion_chrome_tail(body), self.MARKER
                )

    def test_body_prose_with_subtext_and_completion_words_is_not_cut(self):
        message = _raw_bot_msg(
            1,
            f"설명 속 리터럴 -# 줄과 ✅ 완료 문구\n{self.MARKER}",
        )
        window = _window(message)

        self.assertEqual(assertions.relay_body(message), message["content"])
        assertions.text_present(window, needle=self.MARKER)
        assertions.ordered_text_present(window, needles=["-# ", "✅", self.MARKER])
        assertions.body_complete(window, head="설명 속 리터럴", tail=self.MARKER)

    def test_repeated_banner_marker_is_not_body_evidence(self):
        message = _raw_bot_msg(
            1,
            "기존 세션 복원\n\n"
            f"{self.RESUMED_BANNER} {self.MARKER}\n\n",
        )
        window = _window(message)

        # This shape is not product-emitted (session claims are one-shot), but
        # the body boundary remains fail-closed if a regression recreates it.
        self.assertEqual(assertions.relay_body(message), "")
        self.assertEqual(_wait_predicate(window, self.MARKER), False)
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle=self.MARKER)

    def test_completion_panel_marker_is_not_body_evidence(self):
        message = _raw_bot_msg(
            1,
            f"wrong body\n\n-# ✅ 완료\n-# Tasks · {self.MARKER}",
        )
        window = _window(message)

        self.assertEqual(assertions.relay_body(message), "wrong body")
        self.assertEqual(_wait_predicate(window, self.MARKER), False)
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle=self.MARKER)

    def test_no_control_chars_scans_full_wire_message_including_banner(self):
        message = _raw_bot_msg(
            1,
            f"기존 세션 복원 · provider session claude#bad\x1b\n\n{self.MARKER}",
        )
        window = _window(message)

        # The marker body is valid; the wire-level ESC must still fail.
        self.assertEqual(assertions.relay_body(message), self.MARKER)
        with self.assertRaisesRegex(assertions.AssertionError, "control byte"):
            assertions.no_control_chars(window)

    def test_no_resume_prompt_chrome_remains_body_scoped(self):
        message = _raw_bot_msg(
            1,
            f"기존 세션 복원\n\nNo response requested. {self.MARKER}",
        )
        window = _window(message)

        with self.assertRaisesRegex(assertions.AssertionError, "No response requested"):
            assertions.no_resume_prompt_chrome(window)

    def test_response_completion_phrase_inside_normal_body_stays_a_relay(self):
        message = _raw_bot_msg(
            1,
            f"정상 응답 본문: 응답 완료를 설명합니다 {self.MARKER}",
        )
        window = _window(message)

        self.assertEqual(assertions.is_relay_response(message), True)
        self.assertEqual(_wait_predicate(window, self.MARKER), True)
        assertions.text_present(window, needle=self.MARKER)

    def test_current_completion_producer_shapes_stay_chrome(self):
        for content in (
            "✅ **응답 완료**\n> **시작**: <t:1700000000:R>",
            "📦 응답 완료 · resumed\n세션: claude · context unknown · idle 1분",
        ):
            message = _raw_bot_msg(1, content)
            self.assertEqual(assertions.is_relay_response(message), False)
            self.assertEqual(assertions.relay_body(message), None)

    def test_completion_panel_is_chrome_and_completion_after_body(self):
        body = _relay_msg(1, self.MARKER)
        completion = _raw_bot_msg(
            2,
            "-# ✅ 완료\n-# 턴 시작 : anonymized\n\n-# 📦 usage anonymized",
        )
        window = _window(body, completion)

        self.assertEqual(assertions.is_relay_response(completion), False)
        self.assertEqual(window.messages, [body])
        assertions.completion_chrome_after_body(
            window, body_marker=self.MARKER, required=True
        )

    def test_missing_marker_body_fails_the_relay_wait_predicate(self):
        window = _window(
            _raw_bot_msg(1, self.RESUMED_BANNER),
            _raw_bot_msg(2, "-# ✅ 완료\n-# 턴 시작 : anonymized"),
        )

        self.assertEqual(window.messages, [])
        self.assertEqual(_wait_predicate(window, self.MARKER), False)
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle=self.MARKER)

    def test_marker_in_pure_completion_chrome_never_promotes_to_relay(self):
        completion = _raw_bot_msg(1, f"-# ✅ 완료\n{self.MARKER}")
        window = _window(completion)

        assertions.raw_text_present(window, needle=self.MARKER)
        assertions.marker_absent(window, marker=self.MARKER, surface="relay")
        self.assertEqual(_wait_predicate(window, self.MARKER), False)
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle=self.MARKER)

    def test_session_phrase_inside_body_is_not_session_panel_chrome(self):
        message = _raw_bot_msg(1, f"답변 본문에서 {self.RESUMED_BANNER}를 언급함")

        self.assertEqual(assertions.is_relay_response(message), True)


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


class ScenarioFilterFailClosed(unittest.TestCase):
    def test_direct_main_rejects_invalid_filters_before_side_effects(self):
        for raw in ("E-999", "E-1,E-999", "E-1-extra", " , ", "E-29", "E-1,E-29"):
            with self.subTest(raw=raw):
                args = Namespace(
                    base_url="http://agentdesk.test", cell="claude-tui", channel_id="222",
                    thread_channel_id=None, scenarios=str(ROOT / "tests/e2e/tui_relay/scenarios"),
                    filter=raw, output="/unused/direct-filter-output", dry_run=False,
                    allow_destructive=False, reset_before_each=True,
                    queue_runtime_root="/unused/runtime", hard_reset_session_each=False,
                    handoff_to_agent=None, handoff_from_agent="e2e-orchestrator",
                    restart_script=None, restart_target_override=None,
                    turn_start_timeout_s=1, phase_deadline_s=None,
                    required_agent_mode=None, required_coverage_class=None,
                    final_refetches=0, final_refetch_interval_s=0,
                )
                stderr = io.StringIO()
                with (
                    patch.object(driver, "parse_args", return_value=args),
                    patch("pathlib.Path.mkdir") as mkdir,
                    patch("pathlib.Path.write_text") as report,
                    patch.object(driver.discord, "DiscordClient") as client,
                    patch.object(driver.subprocess, "run") as child,
                    patch.object(driver.lease, "acquire") as acquire,
                    patch.object(driver, "run_scenario", return_value={"id": "E-1", "status": "pass"}) as run,
                    contextlib.redirect_stderr(stderr),
                ):
                    rc = driver.main()
                self.assertEqual(rc, 2)
                self.assertRegex(stderr.getvalue(), r"invalid --filter: (unknown|--filter)")
                for side_effect in (mkdir, report, client, child, acquire, run):
                    side_effect.assert_not_called()

    def test_matrix_main_rejects_invalid_filters_before_side_effects(self):
        with tempfile.TemporaryDirectory() as tmp:
            for raw in ("E-999", "E-1,E-999", "E-1-extra", " , ", "E-29", "E-1,E-29"):
                with self.subTest(raw=raw):
                    args = Namespace(
                        base_url="http://agentdesk.test", config="/unused/config.yaml",
                        scenarios=str(ROOT / "tests/e2e/tui_relay/scenarios"),
                        cells="claude-tui", filter=raw, output=str(Path(tmp) / "sentinel"),
                        twice=False, dry_run=True, allow_destructive=False,
                        reset_before_each=True, queue_runtime_root="/unused/runtime",
                        hard_reset_session_each=False, turn_start_timeout_s=1,
                        required_agent_mode=None, required_coverage_class=None,
                        final_refetches=0, final_refetch_interval_s=0,
                    )
                    stderr = io.StringIO()
                    with (
                        patch.object(matrix, "parse_args", return_value=args),
                        patch.object(matrix, "load_channel_ids", return_value={"claude-tui": "222"}) as config,
                        patch.object(matrix, "load_cross_channel_scenarios", return_value=[]) as cross,
                        patch.object(matrix, "load_restart_guard_scenarios", return_value=[]) as restart,
                        patch("pathlib.Path.mkdir") as mkdir,
                        patch("pathlib.Path.write_text") as report,
                        patch.object(matrix.cell_driver.discord, "DiscordClient") as client,
                        patch.object(matrix.subprocess, "run", return_value=Namespace(returncode=0)) as child,
                        contextlib.redirect_stderr(stderr),
                    ):
                        rc = matrix.main()
                    self.assertEqual(rc, 2)
                    self.assertRegex(stderr.getvalue(), r"invalid --filter: (unknown|--filter)")
                    for side_effect in (config, cross, restart, mkdir, report, client, child):
                        side_effect.assert_not_called()

    def test_real_matrix_cli_uses_last_filter_and_global_orchestrator_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "agentdesk.yaml"
            agents = (("claude-pipe", "claude", "111"), ("claude-tui", "claude", "222"),
                      ("codex-pipe", "codex", "444"),
                      ("codex-tui", "codex", "555"))
            config.write_text("agents:\n" + "".join(
                f"  - {{id: adk-{cell}-e2e, channels: {{{provider}: {{id: '{channel}'}}}}}}\n"
                for cell, provider, channel in agents
            ), encoding="utf-8")
            output = root / "matrix"
            proc = subprocess.run(
                ["env", "AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1", sys.executable, str(ROOT / "scripts/e2e/run_multi_provider_matrix.py"),
                 "--config", str(config), "--scenarios", str(ROOT / "tests/e2e/tui_relay/scenarios"),
                 "--output", str(output), "--dry-run", "--allow-destructive", "--cells", "claude-tui,codex-tui",
                 "--filter", "E-999", "--filter", " E-17, E-17, "],
                cwd=ROOT, check=False, capture_output=True, text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            report = json.loads((output / "matrix.json").read_text(encoding="utf-8"))
        self.assertEqual(report["cells"], ["claude-tui", "codex-tui"])
        self.assertEqual(report["cross_channel_scenarios"], [])
        self.assertEqual(report["restart_guard_scenarios"], ["E-17"])
        self.assertEqual(len(report["results"]), 3)
        cells = [row for row in report["results"] if row["kind"] == "cell"]
        self.assertEqual([(row["cell"], row["provider_identity"]["cell"]) for row in cells],
                         [("claude-tui", "claude-tui"), ("codex-tui", "codex-tui")])
        for row in cells:
            self.assertTrue(row["ok"])
            self.assertEqual(row["totals"], {"pass": 0, "fail": 0, "skipped": 0})
        restart = [row for row in report["results"] if row["kind"] == "foreign_active_restart_guard"]
        self.assertEqual([(row["id"], row["status"], row["ok"]) for row in restart],
                         [("E-17", "pass", True)])


class TargetHealthContract(unittest.TestCase):
    """Loaded by the existing CI assertion entry, including the real step caller."""

    def probe(self, mailboxes=None, *, options=None, health=None, counters=None, **target):
        detail = _health_detail(*(mailboxes if mailboxes is not None else [_idle_mailbox()]))
        detail.update(counters or {"global_active": 1, "global_finalizing": 1})
        public = {"status": "healthy", "ok": True, "fully_recovered": True, **(health or {})}
        params = {"global_active_max": 0, "global_finalizing_max": 0, **(options or {})}
        binding = {"channel_id": "42", "cell": "codex-tui", **target}
        with patch.object(driver.urllib.request, "urlopen", _fake_urlopen_for({
            "/api/health": [(200, public)], "/api/health/detail": [(200, detail)],
        })):
            return driver.assert_health("http://agentdesk.test", params, **binding)

    def test_foreign_occupancy_preserves_global_evidence(self):
        result = self.probe([_idle_mailbox(), _busy_mailbox()])
        self.assertEqual((result["global_active"], result["global_finalizing"]), (1, 1))
        self.assertEqual(result["target_mailbox_idle"]["channel_id"], "42")
        self.assertEqual(result["target_mailbox_idle"]["provider"], "codex")

    def test_target_busy_witnesses_refuse(self):
        mutations = [
            ("agent_turn_status", value) for value in ("active", "residual", "residual_held")
        ] + [(key, True) for key in (
            "has_cancel_token", "inflight_state_present", "recovery_started", "active_dispatch_present",
        )] + [("queue_depth", 1), ("active_user_message_id", 7), ("relay_stall_state", "tmux_alive_relay_dead")]
        relay_mutations = [("active_turn", "foreground"), ("active_turn", "explicit_background")]
        relay_mutations += [(key, True) for key in (
            "bridge_inflight_present", "mailbox_has_cancel_token", "pending_thread_proof", "stale_thread_proof", "desynced",
        )] + [("queue_depth", 1), ("mailbox_active_user_msg_id", 7), ("pending_discord_callback_msg_id", 7)]
        for nested, changes in ((False, mutations), (True, relay_mutations)):
            for key, value in changes:
                with self.subTest(relay=nested, key=key, value=value):
                    box = _idle_mailbox()
                    (box["relay_health"] if nested else box)[key] = value
                    with self.assertRaisesRegex(assertions.AssertionError, "target .* busy"):
                        self.probe([box, _busy_mailbox()])

    def test_every_required_witness_rejects_missing_or_malformed_values(self):
        for nested in (False, True):
            fields = _idle_mailbox()["relay_health"] if nested else _idle_mailbox()
            for key, value in fields.items():
                if key in {"provider", "channel_id", "relay_health"}:
                    continue
                for missing in (False, True):
                    with self.subTest(relay=nested, key=key, missing=missing):
                        box = _idle_mailbox()
                        payload = box["relay_health"] if nested else box
                        if missing:
                            del payload[key]
                        else:
                            payload[key] = "false" if type(value) is bool else {}
                        with self.assertRaisesRegex(assertions.AssertionError, "missing/invalid witnesses"):
                            self.probe([box])

    def test_missing_duplicate_malformed_and_wrong_target_refuse(self):
        for boxes in ([], [_idle_mailbox(), _idle_mailbox()], [None],
                      [_idle_mailbox("84")], [_idle_mailbox(provider="claude")],
                      [{**_idle_mailbox(), "relay_health": None}]):
            with self.subTest(boxes=boxes), self.assertRaises(assertions.AssertionError):
                self.probe(boxes)
        for binding in ({"channel_id": None}, {"channel_id": ""}, {"channel_id": "0"},
                        {"channel_id": "٤٢"}, {"cell": None}, {"cell": "invalid"}):
            with self.subTest(binding=binding), self.assertRaises(assertions.AssertionError):
                self.probe(**binding)
        with patch.object(driver, "_read_health_detail", return_value={"mailboxes": {}}):
            with self.assertRaisesRegex(assertions.AssertionError, "list of objects"):
                self.probe()

    def test_public_health_recovery_and_forbidden_reasons_remain_guards(self):
        for health in ({"status": "unhealthy"}, {"ok": False}, {"fully_recovered": False},
                       {"degraded": True}, {"degraded_reasons": ["global_active_counter_out_of_bounds"]}):
            with self.subTest(health=health), self.assertRaises(assertions.AssertionError):
                self.probe(health=health, options={"forbid_degraded_reasons": ["global_active_counter_out_of_bounds"]})

    def test_positive_and_mixed_global_bounds_keep_numeric_limits(self):
        positive = {"global_active_max": 1, "global_finalizing_max": 1}
        result = self.probe([], options=positive, channel_id=None, cell=None)
        self.assertNotIn("target_mailbox_idle", result)
        for active_bound, final_bound in ((1, 1), (0, 1), (1, 0)):
            options = {"global_active_max": active_bound, "global_finalizing_max": final_bound}
            self.probe(options=options)
            counters = {"global_active": 2 if active_bound else 1,
                        "global_finalizing": 2 if final_bound else 1}
            with self.subTest(options=options), self.assertRaisesRegex(assertions.AssertionError, "> 1"):
                self.probe(options=options, counters=counters)

    def test_all_nine_zero_bound_consumers_bind_thread_and_provider_in_real_runner(self):
        consumers = []
        for path in (ROOT / "tests/e2e/tui_relay/scenarios").glob("*.yaml"):
            scenario = driver.yaml.safe_load(path.read_text(encoding="utf-8"))
            for step in scenario.get("steps", []):
                params = step.get("assert_health", {})
                if params.get("global_active_max") == params.get("global_finalizing_max") == 0:
                    consumers.append((scenario["id"], step))
        self.assertEqual({sid for sid, _ in consumers}, {"E-8", "E-9", "E-10", "E-12", "E-14", "E-16", "E-18", "E-19", "E-20"})
        self.assertEqual(len(consumers), 9)
        for sid, step in consumers:
            for cell in driver.SUPPORTED_CELLS:
                with self.subTest(scenario=sid, cell=cell):
                    provider = driver.cell_provider(cell)
                    foreign = "claude" if provider == "codex" else "codex"
                    detail = _health_detail(_idle_mailbox("84", provider),
                                           _busy_mailbox("42", provider), _busy_mailbox("84", foreign))
                    detail.update(global_active=1, global_finalizing=1)
                    client = MagicMock(base_url="http://agentdesk.test")
                    client.send_control.return_value = {"id": "1"}
                    client.fetch_messages.return_value = []
                    args = Namespace(cell=cell, channel_id="42", thread_channel_id="84", dry_run=False,
                                     reset_before_each=False, allow_destructive=False, queue_runtime_root="unused")
                    scenario = {"id": sid, "agent_mode": "controlled", "coverage_class": "live",
                                "requires_thread_channel": True, "steps": [step], "assertions": []}
                    with patch.object(driver.urllib.request, "urlopen", _fake_urlopen_for({
                        "/api/health": [(200, {"status": "healthy", "ok": True, "fully_recovered": True})],
                        "/api/health/detail": [(200, detail)],
                    })), patch.object(driver.time, "sleep"), patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}):
                        result = driver.run_scenario(scenario, args=args, run_id="target-binding", client=client)
                    self.assertEqual(result["status"], "pass", result)
                    evidence = result["health_assertions"][0]
                    self.assertEqual((evidence["global_active"], evidence["global_finalizing"]), (1, 1))
                    self.assertEqual((evidence["target_mailbox_idle"]["channel_id"], evidence["target_mailbox_idle"]["provider"]), ("84", provider))


class TargetHealthR2Contract(unittest.TestCase):
    def run_health(self, options, *, mutation=None):
        box = _idle_mailbox("84")
        if mutation:
            key, value = mutation
            if value == "<missing>":
                del box["relay_health"][key]
            else:
                box["relay_health"][key] = value
        detail = _health_detail(box, _busy_mailbox("42", "codex"), _busy_mailbox("84", "claude"))
        detail.update(global_active=1, global_finalizing=1)
        client = MagicMock(base_url="http://agentdesk.test")
        client.send_control.return_value = {"id": "1"}
        client.fetch_messages.return_value = []
        scenario = {"id": "offline-health-r2", "agent_mode": "controlled", "coverage_class": "live",
                    "requires_thread_channel": True, "steps": [{"assert_health": options}], "assertions": []}
        args = Namespace(cell="codex-tui", channel_id="42", thread_channel_id="84", dry_run=False,
                         reset_before_each=False, allow_destructive=False, queue_runtime_root="unused")
        with patch.object(driver.urllib.request, "urlopen", _fake_urlopen_for({
            "/api/health": [(200, {"status": "healthy", "ok": True, "fully_recovered": True})],
            "/api/health/detail": [(200, detail)],
        })), patch.object(driver.time, "sleep"), patch.object(driver, "_runtime_queue_violations", return_value=[]):
            # Keep the real post-scenario assert_cell_idle, as in the independent counterexample.
            return driver.run_scenario(scenario, args=args, run_id="identity-and-bounds", client=client)

    def test_nested_serialized_identity_must_match_resolved_thread_and_provider(self):
        options = {"global_active_max": 0, "global_finalizing_max": 0}
        valid = self.run_health(options)
        self.assertEqual(valid["status"], "pass", valid["reason"])
        self.assertEqual(valid["post_scenario_idle"]["channel_id"], "84")
        for mutation in (("provider", "claude"), ("provider", []), ("provider", None),
                         ("channel_id", 42), ("channel_id", "84"), ("channel_id", 84.0),
                         ("channel_id", True), ("channel_id", {}),
                         ("provider", "<missing>"), ("channel_id", "<missing>")):
            with self.subTest(mutation=mutation):
                result = self.run_health(options, mutation=mutation)
                self.assertEqual(result["status"], "fail", result["reason"])
                self.assertIn("target relay identity", result["reason"])

    def test_only_original_integer_zero_selects_target_policy(self):
        for key, other in (("global_active_max", "global_finalizing_max"),
                           ("global_finalizing_max", "global_active_max")):
            for value in (0.5, 0.0, False, "0"):
                for other_bound in (None, 0, 1):
                    with self.subTest(key=key, value=value, other=other_bound):
                        options = {key: value}
                        if other_bound is not None:
                            options[other] = other_bound
                        result = self.run_health(options)
                        self.assertEqual(result["status"], "fail", result["reason"])
                        self.assertIn(f"{key.removesuffix('_max')}=1 > 0", result["reason"])
            for value in (0, 1, 1.5):
                result = self.run_health({key: value, other: 0})
                self.assertEqual(result["status"], "pass", result["reason"])
                observed = result["health_assertions"][0]
                self.assertEqual((observed["global_active"], observed["global_finalizing"]), (1, 1))


class E35CurrentRunContract(unittest.TestCase):
    def test_actual_yaml_through_real_runner_checks_final_body_and_completion(self):
        path = ROOT / "tests/e2e/tui_relay/scenarios/E-35-durable-delivery-record.yaml"
        scenario = driver.yaml.safe_load(path.read_text(encoding="utf-8"))
        original = copy.deepcopy(scenario)
        marker = "[E2E:E35:current-run:OK]"
        body, completion = _relay_msg(3, marker), _raw_bot_msg(6, "✅ 응답 완료")
        cases = {
            "current": ([body, completion], "pass"),
            "stale": ([_relay_msg(3, "[E2E:E35:old-run:OK]"), completion], "fail"),
            "missing": ([_relay_msg(3, "unrelated response"), completion], "fail"),
            "duplicate marker": ([body, _relay_msg(4, marker + " resent"), completion], "fail"),
            "duplicate content": ([body, _relay_msg(4, "same body"), _relay_msg(5, "same body"), completion], "fail"),
            "missing completion": ([body], "fail"),
            "early completion": ([_raw_bot_msg(2, "✅ 응답 완료"), body], "fail"),
        }
        for cell in scenario["cells"]:
            for label, (messages, expected) in cases.items():
                with self.subTest(cell=cell, case=label):
                    client = MagicMock(base_url="http://agentdesk.test")
                    client.send_control.return_value = {"id": "1"}
                    client.send.return_value = {"id": "2"}
                    # The real wait first sees the current marker; final edits must still pass assertions.
                    client.fetch_messages.side_effect = [[], [body], messages]
                    args = Namespace(base_url=client.base_url, cell=cell, channel_id="42", thread_channel_id=None,
                                     dry_run=False, reset_before_each=True, hard_reset_session_each=True,
                                     allow_destructive=False, queue_runtime_root="unused", final_refetches=1)
                    with patch.object(driver, "durable_probe_safety_gate", return_value={"status": "idle"}) as safety, \
                         patch.object(driver.durable_delivery, "poll_records", return_value={"status": "evaluated"}) as receipt, \
                         patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}), \
                         patch.object(driver, "reset_channel_state") as reset, \
                         patch.object(driver, "hard_reset_provider_session") as hard_reset, patch.object(driver.time, "sleep"):
                        result = driver.run_scenario(scenario, args=args, run_id="current-run", client=client)
                    self.assertEqual(result["status"], expected, result)
                    if expected == "fail":
                        self.assertEqual(result["failure_attribution"]["source"], "assertion")
                    client.send.assert_called_once_with("42", original["steps"][0]["send_discord_prompt"].replace("{run_id}", "current-run"))
                    client.send_prompt.assert_not_called()
                    receipt.assert_called_once_with(Path("unused"), provider="claude", channel_id="42", message_id="3")
                    self.assertEqual(safety.call_count, 2)
                    reset.assert_not_called()
                    hard_reset.assert_not_called()
        self.assertEqual(scenario, original)


class RetiredCellAdmission(unittest.TestCase):
    def test_direct_cli_rejects_retired_cell_before_side_effects(self):
        stderr = io.StringIO()
        with (
            patch.object(sys, "argv", ["driver", "--cell", "claude-e", "--channel-id", "333"]),
            patch.object(driver, "load_scenarios") as scenarios,
            patch("pathlib.Path.mkdir") as mkdir,
            patch("pathlib.Path.write_text") as report,
            patch.object(driver.discord, "DiscordClient") as client,
            patch.object(driver.subprocess, "run") as child,
            patch.object(driver.lease, "acquire") as acquire,
            patch.object(driver, "run_scenario") as run,
            contextlib.redirect_stderr(stderr),
        ):
            with self.assertRaises(SystemExit) as error:
                driver.main()
        self.assertEqual(error.exception.code, 2)
        self.assertIn("invalid choice: 'claude-e'", stderr.getvalue())
        for side_effect in (scenarios, mkdir, report, client, child, acquire, run):
            side_effect.assert_not_called()

    def test_matrix_cli_rejects_retired_cell_before_side_effects(self):
        for cells in ("claude-e", "claude-tui,claude-e"):
            with (
                self.subTest(cells=cells),
                patch.object(sys, "argv", ["matrix", "--cells", cells, "--scenarios",
                                          str(ROOT / "tests/e2e/tui_relay/scenarios")]),
                patch.object(matrix, "load_channel_ids") as config,
                patch.object(matrix, "load_cross_channel_scenarios") as cross,
                patch.object(matrix, "load_restart_guard_scenarios") as restart,
                patch("pathlib.Path.mkdir") as mkdir,
                patch("pathlib.Path.write_text") as report,
                patch.object(matrix.cell_driver.discord, "DiscordClient") as client,
                patch.object(matrix.subprocess, "run") as child,
                patch.object(matrix, "run_cell") as run,
            ):
                with self.assertRaisesRegex(ValueError, "claude-e"):
                    matrix.main()
            for side_effect in (config, cross, restart, mkdir, report, client, child, run):
                side_effect.assert_not_called()


class RequiredCompletionWait(unittest.TestCase):
    """Real E35 driver/client/primitive composition with inert transport/clocks."""

    RUN = "offline-c6"
    MARKER = "[E2E:E35:offline-c6:OK]"

    def _run(self, *, arrival=4.649, initial_delay=0, return_delay=0, retry_after=None,
             before=False, optional=False, mutation=None, other_failure=False, client_kind="default"):
        scenario = driver.yaml.safe_load((ROOT / "tests/e2e/tui_relay/scenarios/"
                                         "E-35-durable-delivery-record.yaml").read_text())
        scenario["assertions"][-1]["completion_chrome_after_body"]["required"] = not optional
        if mutation == "raw_count":
            scenario["assertions"].insert(0, {"raw_message_count_between_markers": {"min": 1, "max": 36}})
        if other_failure:
            scenario["assertions"].insert(0, {"text_present": "[MISSING]"})
        clock, requests, sleeps = [90.0], [], []
        def fetch(request, **kwargs):
            index = len(requests)
            requests.append((clock[0], kwargs["timeout"]))
            if index == 0:
                return _Response([_our_msg(109, self.MARKER)])
            if index == 1:
                clock[0] = 100.0
            if index == 4 and retry_after is not None:
                return _Response({"retry_after": retry_after}, status=429)
            if index >= 4:
                clock[0] += return_delay
            rows = [_raw_bot_msg(201, self.MARKER, "2026-01-01T00:00:01Z")]
            content = "-# ✅ 완료" if arrival is not None and clock[0] - 100 >= arrival else "-# 🔧 마지막 도구 (아직 없음)"
            rows.append(_raw_bot_msg(202, content, "2026-01-01T00:00:00Z" if before else "2026-01-01T00:00:02Z"))
            if mutation == "raw_count" and index >= 4:
                rows.extend(_raw_bot_msg(mid, f"ordinary filler {mid}") for mid in range(203, 239))
            if mutation == "body" and index >= 4:
                rows[0]["content"] = "body overwritten"
            return _Response(rows)
        def sleep(seconds):
            if len(requests) >= 2:
                sleeps.append(seconds)
                clock[0] += seconds
        def receipt(*args, **kwargs):
            clock[0] += initial_delay
            return {"status": "evaluated"}
        client = driver.discord.DiscordClient("http://offline.invalid")
        original_client = client
        class FixedConstructorClient(driver.discord.DiscordClient):
            def __init__(self):
                super().__init__("http://offline.invalid")
        @dataclass
        class DataclassAdapter:
            base_url: str = "http://offline.invalid"
            def send_control(self, *args, **kwargs):
                return original_client.send_control(*args, **kwargs)
            def send(self, *args, **kwargs):
                return original_client.send(*args, **kwargs)
            def fetch_messages(self, *args, **kwargs):
                return original_client.fetch_messages(*args, **kwargs)
        if client_kind == "fixed_constructor":
            client = FixedConstructorClient()
        elif client_kind == "dataclass_adapter":
            client = DataclassAdapter()
        with (
            patch("socket.socket", side_effect=AssertionError("network forbidden")),
            patch("subprocess.Popen", side_effect=AssertionError("process forbidden")),
            patch("urllib.request.urlopen", side_effect=fetch),
            patch.object(driver.time, "monotonic", side_effect=lambda: clock[0]),
            patch.object(driver.time, "time", side_effect=lambda: 1767225600 + clock[0]),
            patch.object(driver.time, "sleep", side_effect=sleep),
            patch.object(driver.discord.DiscordClient, "send_control", return_value={"id": "100"}),
            patch.object(driver.discord.DiscordClient, "send", return_value={"id": "110"}),
            patch.object(driver, "durable_probe_safety_gate", return_value={"status": "idle"}),
            patch.object(driver.durable_delivery, "poll_records", side_effect=receipt),
            patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}),
            patch.object(driver, "_raise_if_tui_prompt_stuck_while_idle", side_effect=AssertionError("unexpected idle probe")),
        ):
            try:
                record = driver.run_one_cell(scenario=scenario, cell="claude-tui", channel_id="offline",
                    client=client, run_id=self.RUN, dry_run=False,
                    args=Namespace(base_url=client.base_url, queue_runtime_root="/offline-denied"))
            except driver.ScenarioStepAssertionError as error:
                return error.record, error, requests, sleeps
        return record, None, requests, sleeps

    def test_observed_4649ms_lag_passes_without_resetting_body_anchor(self):
        record, error, requests, _ = self._run()
        self.assertIsNone(error, str(error))
        self.assertEqual(len(requests), 6)
        trace = record["completion_rechecks"][0]
        self.assertEqual(trace, {"refetches": 2, "deadline_at": 110, "elapsed_s": 5, "outcome": "PASS"})
        self.assertEqual(record["_body_observations"][self.MARKER], 100)
        self.assertEqual(record["durable_record_probe"]["status"], "evaluated")
        revalidated = record["revalidated_after_recheck"]
        self.assertEqual(len(revalidated), 2)
        self.assertTrue(all(item["passed"] and len(item["assertions"]) == 3 for item in revalidated))
        result = {"assertions": []}
        driver._merge_record_into_result(result, record)
        self.assertEqual(result["completion_rechecks"], [trace])
        self.assertEqual(result["revalidated_after_recheck"], revalidated)
        self.assertNotIn("_body_observations", result)

    def test_exhaustion_uses_at_most_three_additional_fetches(self):
        record, error, requests, _ = self._run(arrival=None)
        self.assertIn("completion chrome not found", str(error))
        self.assertEqual(len(requests), 7)
        self.assertEqual(record.get("completion_rechecks"), [
            {"refetches": 3, "deadline_at": 110, "elapsed_s": 7, "outcome": "EXHAUSTED"}])

    def test_same_clock_late_response_boundary_and_retry_wait_are_not_accepted(self):
        for delay, passed in ((6.999, True), (7.0, False), (8.0, False)):
            with self.subTest(return_delay=delay):
                record, error, requests, _ = self._run(return_delay=delay)
                self.assertEqual(error is None, passed)
                self.assertEqual(len(requests), 5)
                self.assertEqual(record["completion_rechecks"][0]["refetches"], 1)
                self.assertEqual(record["completion_rechecks"][0]["outcome"], "PASS" if passed else "EXHAUSTED")
        record, error, requests, _ = self._run(retry_after=8)
        self.assertIsNotNone(error)
        self.assertEqual(len(requests), 6)  # One fetch call includes the existing HTTP429 retry.
        self.assertEqual(record["completion_rechecks"][0],
                         {"refetches": 1, "deadline_at": 110, "elapsed_s": 11, "outcome": "EXHAUSTED"})

    def test_expired_first_observation_does_not_reset_at_assertion_start(self):
        record, error, requests, _ = self._run(arrival=None, initial_delay=10)
        self.assertIsNotNone(error)
        self.assertEqual(len(requests), 4)
        self.assertEqual(record.get("completion_rechecks"), [
            {"refetches": 0, "deadline_at": 110, "elapsed_s": 11, "outcome": "EXHAUSTED"}])

    def test_pre_body_completion_and_missing_body_fail_without_wait(self):
        for arrival in (0, 4.649):
            with self.subTest(arrival=arrival):
                record, error, requests, _ = self._run(arrival=arrival, before=True)
                self.assertIn("completion chrome appeared before body", str(error))
                self.assertEqual(len(requests), 4 if arrival == 0 else 6)
        callback = MagicMock()
        with self.assertRaisesRegex(assertions.AssertionError, "body marker.*not found"):
            driver.run_assertion({"completion_chrome_after_body": {"body_marker": "missing", "required": True}},
                                 window=assertions.Window("100"), record={}, pending_refetch=callback)
        callback.assert_not_called()

    def test_recheck_invalidates_prior_count_and_body_guards(self):
        for mutation, owner, reason in (("raw_count", "raw_message_count_between_markers", "raw message count 38 outside"),
                                        ("body", "text_present", "expected to find")):
            with self.subTest(mutation=mutation):
                record, error, requests, _ = self._run(mutation=mutation)
                self.assertIsNotNone(error)
                self.assertIn(reason, str(error))
                self.assertEqual(len(requests), 5)
                self.assertEqual(record["completion_rechecks"][0]["outcome"], "FAIL")
                self.assertEqual(record["completion_rechecks"][0]["elapsed_s"], 3)
                self.assertEqual(record["revalidated_after_recheck"][-1]["failed_assertion"], owner)
                self.assertFalse(record["revalidated_after_recheck"][-1]["passed"])

    def test_optional_initially_complete_and_other_assertions_preserve_fetches(self):
        for options in ({"optional": True, "arrival": None}, {"arrival": 0}, {"other_failure": True}):
            with self.subTest(options=options):
                record, error, requests, _ = self._run(**options)
                self.assertEqual(error is not None, bool(options.get("other_failure")))
                self.assertEqual(len(requests), 4)
                self.assertNotIn("completion_rechecks", record)
                self.assertNotIn("revalidated_after_recheck", record)
        callback = MagicMock()
        with self.assertRaisesRegex(assertions.AssertionError, "unknown assertion"):
            driver.run_assertion({"unknown_completion_wait": True}, window=assertions.Window("100"),
                                 pending_refetch=callback)
        callback.assert_not_called()

    def _check_unsupported_client(self, **options):
        for kind in ("fixed_constructor", "dataclass_adapter"):
            with self.subTest(client_kind=kind, options=options):
                try:
                    record, error, requests, sleeps = self._run(client_kind=kind, **options)
                except TypeError as error:
                    self.fail(f"client reconstruction escaped: {error}")
                missing = options.get("arrival") is None and not options.get("optional", False)
                self.assertEqual(error is not None, missing)
                if missing:
                    self.assertIsInstance(error, driver.ScenarioStepAssertionError)
                    self.assertIn("completion chrome not found", str(error))
                self.assertEqual(len(requests), 4)
                self.assertEqual(sleeps, [1.0])
                self.assertNotIn("completion_rechecks", record)
                self.assertNotIn("revalidated_after_recheck", record)
                result = {"assertions": []}
                driver._merge_record_into_result(result, record)
                json.dumps(result, allow_nan=False)

    def test_unsupported_constructors_preserve_completed_required_scenario(self):
        self._check_unsupported_client(arrival=0)

    def test_unsupported_constructors_preserve_optional_scenario(self):
        self._check_unsupported_client(arrival=None, optional=True)

    def test_unsupported_constructors_refuse_unanchored_required_without_refetch(self):
        self._check_unsupported_client(arrival=None)

    def test_missing_matching_anchor_never_emits_nonfinite_trace(self):
        for record in (None, {}, {"_body_observations": {}}, {"_body_observations": {"other body": 100}}):
            with self.subTest(record=record):
                callback = MagicMock()
                try:
                    driver.run_assertion({"completion_chrome_after_body": {
                        "body_marker": self.MARKER, "required": True}},
                        window=_window(_relay_msg(201, self.MARKER)), record=record, pending_refetch=callback)
                except assertions.AssertionError as error:
                    self.assertIn("completion chrome not found", str(error))
                except Exception as error:
                    self.fail(f"unexpected unanchored error: {type(error).__name__}: {error}")
                else:
                    self.fail("missing required completion was accepted")
                callback.assert_not_called()
                self.assertNotIn("completion_rechecks", record or {})
                json.dumps(record or {}, allow_nan=False)

    def test_immediate_paths_evaluate_completion_once(self):
        for options in ({"arrival": 0}, {"arrival": None, "optional": True}):
            with self.subTest(options=options), patch.object(
                assertions, "completion_chrome_after_body", wraps=assertions.completion_chrome_after_body
            ) as primitive:
                record, error, requests, _ = self._run(**options)
                self.assertIsNone(error, str(error))
                self.assertEqual(primitive.call_count, 1)
                self.assertEqual(len(requests), 4)
                self.assertNotIn("completion_rechecks", record)

    def test_non_typeerror_reconstruction_failure_is_not_swallowed(self):
        with patch.object(driver, "replace", side_effect=ValueError("unrelated reconstruction error")):
            with self.assertRaisesRegex(ValueError, "unrelated reconstruction error"):
                self._run(arrival=0)


if __name__ == "__main__":
    unittest.main()
