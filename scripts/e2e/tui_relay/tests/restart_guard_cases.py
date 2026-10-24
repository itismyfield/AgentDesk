"""Restart safety when health cannot observe every mailbox."""

from __future__ import annotations

import copy
import json
import sys
import unittest
from argparse import Namespace
from itertools import product
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import run_tui_relay as driver  # noqa: E402
from tui_relay import assertions  # noqa: E402


class RestartObservationGuard(unittest.TestCase):
    @staticmethod
    def detail():
        return {
            "mailboxes": [], "global_active": 0, "global_finalizing": 0,
            "providers": [{"name": "claude", "runtime_state_complete": True},
                          {"name": "codex", "runtime_state_complete": True}],
            "mailbox_observation_failures": [],
        }

    def guard(self, detail, sessions=None, *, restart=False):
        self.guard_requests = calls = []

        class Response:
            status = 200

            def __init__(self, body):
                self.body = body

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

            def read(self):
                return json.dumps(self.body).encode()

        def urlopen(request, **kwargs):
            url = getattr(request, "full_url", request)
            calls.append(url)
            if url == "http://agentdesk.test/api/health/detail":
                return Response(detail)
            self.assertEqual(url, "http://agentdesk.test/api/sessions")
            return Response([] if sessions is None else sessions)

        with (patch("socket.socket", side_effect=AssertionError("network forbidden")),
              patch.object(driver.urllib.request, "urlopen", side_effect=urlopen),
              patch.object(driver.subprocess, "run") as process):
            try:
                if restart:
                    driver.restart_dcserver_for_e2e(
                        target="release", args=Namespace(restart_script=None),
                        base_url="http://agentdesk.test", cell="codex-tui", channel_id="42")
                else:
                    driver._guard_no_foreign_active_turns("http://agentdesk.test", "42", "codex-tui")
            finally:
                process.assert_not_called()
        return calls

    def test_observation_failures_alone_refuse_before_mailbox_iteration(self):
        for read, empty in product(("snapshot", "reconfirm", "proof"), (False, True)):
            detail = self.detail()
            detail["mailbox_observation_failures"] = [{"provider": "claude", "channel_id": 99, "read": read}]
            if not empty:
                detail["mailboxes"] = [{"provider": "claude", "channel_id": "99", "agent_turn_status": "active"}]
            with self.subTest(read=read, empty=empty), self.assertRaisesRegex(assertions.AssertionError, "mailbox_observation_failures"):
                self.guard(detail)

    def test_incomplete_provider_alone_refuses_even_for_current_cell(self):
        for provider in ("claude", "codex"):
            detail = self.detail()
            next(p for p in detail["providers"] if p["name"] == provider)["runtime_state_complete"] = False
            with self.subTest(provider=provider), self.assertRaisesRegex(assertions.AssertionError, provider + ".*runtime_state_complete"):
                self.guard(detail)

    def test_incomplete_input_combinations_refuse_and_complete_inputs_pass(self):
        for empty_mailboxes, active, incomplete, failed, empty_sessions in product((False, True), repeat=5):
            detail = self.detail()
            if not empty_mailboxes:
                detail["mailboxes"] = [{"provider": "codex", "channel_id": "42", "agent_turn_status": "active"}]
            detail["global_active"] = int(active)
            detail["providers"][0]["runtime_state_complete"] = not incomplete
            if failed:
                detail["mailbox_observation_failures"] = [{"provider": "claude", "channel_id": 99, "read": "snapshot"}]
            sessions = [] if empty_sessions else [{"status": "active", "channel_id": "42"}]
            with self.subTest(empty_mailboxes=empty_mailboxes, active=active, incomplete=incomplete,
                              failed=failed, empty_sessions=empty_sessions):
                if incomplete or failed:
                    with self.assertRaisesRegex(assertions.AssertionError, "incomplete mailbox observation"):
                        self.guard(detail, sessions)
                else:
                    self.assertEqual(len(self.guard(detail, sessions)), 2)

    def test_issue_fixture_cannot_reach_kickstart_or_session_fallback(self):
        detail = self.detail()
        detail["global_active"] = 1
        detail["providers"][0]["runtime_state_complete"] = False
        detail["mailbox_observation_failures"] = [{"provider": "claude", "channel_id": 99, "read": "snapshot"}]
        with self.assertRaisesRegex(assertions.AssertionError, "mailbox_observation_failures"):
            self.guard(detail, [], restart=True)
        self.assertEqual(self.guard_requests, ["http://agentdesk.test/api/health/detail"])

    def test_legacy_missing_fields_and_omitted_empty_failure_vector_pass(self):
        detail = self.detail()
        del detail["mailbox_observation_failures"]
        for provider in detail["providers"]:
            del provider["runtime_state_complete"]
        for providers_present in (True, False):
            with self.subTest(providers_present=providers_present):
                payload = copy.deepcopy(detail)
                if not providers_present:
                    del payload["providers"]
                self.assertEqual(len(self.guard(payload)), 2)

    def test_malformed_observation_fields_refuse(self):
        for field, invalid in (("mailbox_observation_failures", None), ("mailbox_observation_failures", {}),
                               ("providers", None), ("providers", {}), ("providers", [None]),
                               ("providers", [{"name": "claude", "runtime_state_complete": "false"}]),
                               ("providers", [{"name": "claude", "runtime_state_complete": None}])):
            detail = self.detail()
            detail[field] = invalid
            with self.subTest(field=field, invalid=invalid), self.assertRaisesRegex(assertions.AssertionError, field):
                self.guard(detail)
