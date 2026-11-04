"""Retained history must not hide pending work or block an otherwise idle cell."""

from __future__ import annotations

import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import post_deploy_turn_smoke as smoke  # noqa: E402
import run_tui_relay as driver  # noqa: E402


class RetainedQueueIdle(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.target = self.root / "discord_pending_queue/codex/discord_7a14995f62164ef1/1509350778043895902.json"
        self.target.parent.mkdir(parents=True)
        self.retained = {
            "input": {
                "identity": {"primary": 1, "queued_generation": 1, "sources": []},
                "original": {"message_id": 1, "text": "보관된 입력", "queued_generation": 1},
                "phase": "reserved",
                "attempt": {"nonce": "unsent", "provider": "codex",
                            "target": {"host": "tmux", "owner": "fixture", "root": "/fixture",
                                       "execution_nonce": "fixture"},
                            "source": {"kind": "pre_source", "expected_session_id": None,
                                       "expected_transcript": None, "binding_watermark": None}},
            },
            "disposition": "known_not_sent",
            "reason": {"kind": "rejected", "reason": "known_not_sent: vetoed: composer_refused: Codex TUI warm follow-up"},
            "notice": {"kind": "retained_retry_required", "enqueued": True},
        }
        self.snapshot = {"version": 2, "items": [], "head_injection": None,
                         "retained_inputs": [self.retained], "terminal_identities": [], "admission_binds": []}

    def assert_busy(self, payload, busy):
        self.target.write_text(json.dumps(payload), encoding="utf-8")
        self.assertEqual(bool(driver._runtime_queue_violations(
            runtime_root=self.root, provider="codex", channel_id="1509350778043895902")), busy)
        with patch.object(driver, "_read_health_detail", return_value={"mailboxes": []}):
            self.assertEqual(bool(smoke.preflight_busy_reasons(
                base_url="http://offline.invalid", cell="codex-tui", channel_id="1509350778043895902",
                runtime_root=self.root)), busy)

    def test_completed_resend_notice_only_is_idle(self):
        self.assert_busy(self.snapshot, False)

    def test_other_known_completed_notices_are_idle(self):
        for disposition, kind in (("opaque_unsent", "retained_retry_required"), (None, "unconfirmed")):
            with self.subTest(disposition=disposition):
                self.retained["disposition"] = disposition
                self.retained["notice"]["kind"] = kind
                self.assert_busy(self.snapshot, False)

    def test_owner_requeue_and_owed_notice_stay_busy(self):
        for disposition, kind in (("known_not_sent", "retained_retry_required"),
                                  ("opaque_unsent", "retained_retry_required"), (None, "unconfirmed")):
            with self.subTest(disposition=disposition):
                self.retained["disposition"] = disposition
                self.retained["notice"] = {"kind": kind, "enqueued": False}
                self.assert_busy(self.snapshot, True)

    def test_completed_retained_does_not_hide_other_waiting_work(self):
        waiting = (("items", [{"message_id": 2}]), ("head_injection", self.retained["input"]),
                   ("admission_binds", [{"receipt": "r", "at": {"kind": "attempt"}}]),
                   ("retained_inputs", [self.retained, {**self.retained, "notice": {"enqueued": False}}]))
        for key, value in waiting:
            with self.subTest(key=key):
                self.assert_busy({**self.snapshot, key: value}, True)
        self.assert_busy({**self.snapshot, "admission_binds": [{"receipt": "r", "at": {"kind": "settled"}}]}, False)

    def test_attempt_bind_of_the_completed_retained_input_is_idle(self):
        at = {"kind": "attempt", "identity": self.retained["input"]["identity"],
              "attempt": self.retained["input"]["attempt"]}
        self.assert_busy({**self.snapshot, "admission_binds": [{"receipt": "r", "at": at}]}, False)
        def changed(path, value):
            bind = copy.deepcopy(at)
            node = bind
            for key in path[:-1]:
                node = node[key]
            node[path[-1]] = value
            return bind
        others = [changed(("attempt", "nonce"), "other"), changed(("identity", "primary"), 2),
                  changed(("attempt", "provider"), "claude"), changed(("attempt", "target", "owner"), "other"),
                  changed(("attempt", "source", "kind"), "other")]
        for other in others:
            with self.subTest(other=other):
                self.assert_busy({**self.snapshot, "admission_binds": [{"receipt": "r", "at": other}]}, True)
        owed = {**self.retained, "notice": {"kind": "retained_retry_required", "enqueued": False}}
        self.assert_busy({**self.snapshot, "retained_inputs": [owed], "admission_binds": [{"receipt": "r", "at": at}]}, True)
        blank = {**self.retained, "input": {}}
        for bind in ({"kind": "attempt"}, {"kind": "attempt", "identity": None, "attempt": None},
                     {"kind": "attempt", "identity": {}, "attempt": {}}):
            with self.subTest(bind=bind):
                self.assert_busy({**self.snapshot, "retained_inputs": [blank], "admission_binds": [{"receipt": "r", "at": bind}]}, True)

    def test_unknown_dispositions_and_notice_shapes_stay_busy(self):
        for disposition in ("future", [], {}, 1):
            with self.subTest(disposition=disposition):
                self.assert_busy({**self.snapshot, "retained_inputs": [{**self.retained, "disposition": disposition}]}, True)
        for notice in (None, [], "sent", {}, {"kind": "retained_retry_required"},
                       {"kind": "future", "enqueued": True}, {"kind": "unconfirmed", "enqueued": True},
                       {"kind": "retained_retry_required", "enqueued": 1},
                       {"kind": "retained_retry_required", "enqueued": "true"},
                       {**self.retained["notice"], "future": True}):
            with self.subTest(notice=notice):
                self.assert_busy({**self.snapshot, "retained_inputs": [{**self.retained, "notice": notice}]}, True)
        for entry in (None, [], "retained", {}):
            with self.subTest(entry=entry):
                self.assert_busy({**self.snapshot, "retained_inputs": [entry]}, True)

    def test_legacy_payload_is_not_given_the_v2_exception(self):
        self.assert_busy([self.retained], True)
        self.assert_busy([{"message_id": 2}], True)
        self.assert_busy([], False)
        legacy = copy.deepcopy(self.snapshot)
        del legacy["version"]
        self.assert_busy(legacy, True)


if __name__ == "__main__":
    unittest.main()
