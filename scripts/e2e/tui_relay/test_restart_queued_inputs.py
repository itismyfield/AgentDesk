"""Restart health waits accept only the scenario's own queued inputs on the target mailbox."""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from tui_relay.test_driver_health import ROOT, _OutcomeFixture, driver  # noqa: E402
from tui_relay import assertions  # noqa: E402
from tui_relay.discord import DiscordClient  # noqa: E402
from tui_relay.test_discord_client import _Response  # noqa: E402

TARGET, OTHER = "99", 1475086789696946196
QUEUED_2 = "provider:claude:pending_queue_depth:2"


def restart_options(name):
    data = driver.yaml.safe_load((ROOT / "tests/e2e/tui_relay/scenarios" / f"{name}.yaml").read_text())
    return next(step["restart_dcserver"] for step in data["steps"] if "restart_dcserver" in step)


def public(reasons):
    # The last /api/health body of the failed E-63 restart wait, without the http field.
    return {"degraded": True, "degraded_reasons": reasons, "fully_recovered": True, "ok": False, "status": "degraded"}


def box(channel, depth, provider="claude"):
    return {"provider": provider, "channel_id": channel, "queue_depth": depth}


class RestartQueuedInputs(_OutcomeFixture, unittest.TestCase):
    def restart_row(self, options, reasons, boxes):
        kicked, clock = [], [0.0]

        def api(base, path, **kwargs):
            if not kicked:
                return 200, {"boot_incarnation": "boot-before"} if path == "/api/health/detail" else {
                    "cluster_standby": False, "status": "healthy", "ok": True, "fully_recovered": True}
            if path == "/api/health/detail":
                return 200, {**public(reasons), "mailboxes": boxes, "boot_incarnation": "boot-after"}
            return 200, public(reasons)

        def kickstart(cmd, **kwargs):
            kicked.append(cmd)
            return subprocess.CompletedProcess(cmd, 0, "", "")

        self.api.side_effect = api
        driver.time.sleep.side_effect = lambda seconds: clock.__setitem__(0, clock[0] + seconds)
        with patch.object(driver.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(driver.subprocess, "run", side_effect=kickstart):
            steps = [{"restart_dcserver": options}, {"send_prompt": "after health"}]
            _, report, _ = self.main_result(self.scenario("E-X", steps))
        self.assertEqual(len(kicked), 1)
        return report["scenarios"][0]

    def test_restart_passes_only_target_mailbox_queue_within_declared_bound(self):
        e63, e9 = restart_options("E-63-input-order-across-restart"), restart_options("E-9-restart-mid-stream")
        cases = (
            ("e63_recorded_queue", e63, [QUEUED_2], [box(TARGET, 2), box(OTHER, 0)], None),
            ("over_bound", e63, ["provider:claude:pending_queue_depth:3"], [box(TARGET, 3)],
             ["provider:claude:pending_queue_depth:3"]),
            ("queue_on_other_channel", e63, [QUEUED_2], [box(TARGET, 0), box(OTHER, 2)], [QUEUED_2]),
            ("other_provider_on_target", e63, ["provider:codex:pending_queue_depth:1"],
             [box(TARGET, 0), box(TARGET, 1, "codex")], ["provider:codex:pending_queue_depth:1"]),
            ("other_reason", e63, [QUEUED_2, "tui_o:unexpected:7"], [box(TARGET, 2)], ["tui_o:unexpected:7"]),
            ("undeclared_scenario", e9, [QUEUED_2], [box(TARGET, 2)], [QUEUED_2]),
        )
        for name, options, reasons, boxes, blocked in cases:
            with self.subTest(name):
                row = self.restart_row(options, reasons, boxes)
                self.assertEqual(row["status"], "fail" if blocked else "pass", row.get("reason"))
                if blocked:
                    self.assertIn("did not become healthy", row["reason"])
                    self.assertIn(f"degraded_reasons={blocked!r}", row["reason"])


class QueueNoticeDuplicates(unittest.TestCase):
    NOTICE = "큐에 들어갔습니다"

    def window(self, *references, body=None):
        window = assertions.Window("10")
        for index, reference in enumerate(references, 20):
            window.add({"id": str(index), "content": self.NOTICE if body is None else body,
                        "type": 19, "author": {"id": "relay-fixture", "bot": True}, **reference})
        window.reconcile_snapshot(list(window.messages), after_id="10", complete=True)
        return window

    def test_distinct_input_references_pass(self):
        for field, key in (("message_reference", "message_id"), ("referenced_message", "id")):
            with self.subTest(field=field):
                window = self.window({field: {key: "101"}}, {field: {key: "102"}})
                assertions.no_duplicate_content(window)

    def test_same_input_reference_fails(self):
        references = ({"message_reference": {"message_id": "101"}},
                      {"referenced_message": {"id": 101}},
                      {"message_reference": None, "referenced_message": {"id": "101"}})
        for first in references:
            for second in references:
                with self.subTest(first=first, second=second):
                    with self.assertRaisesRegex(assertions.AssertionError, "duplicate Discord relay body"):
                        assertions.no_duplicate_content(self.window(first, second))

    def test_missing_reference_fails(self):
        references = ({}, {"message_reference": None, "referenced_message": None},
                      {"message_reference": {}, "referenced_message": {}},
                      {"message_reference": {"message_id": None}, "referenced_message": {"id": None}})
        for first in references:
            for second in references:
                with self.subTest(first=first, second=second):
                    with self.assertRaisesRegex(assertions.AssertionError, "duplicate Discord relay body"):
                        assertions.no_duplicate_content(self.window(first, second))

    def test_regular_body_duplicate_fails(self):
        for body in ("same answer", f"answer mentioning {self.NOTICE}", f"{self.NOTICE}\nmore answer"):
            for references in (({}, {}), ({"message_reference": {"message_id": "101"}},
                                          {"message_reference": {"message_id": "102"}})):
                with self.subTest(body=body, references=references):
                    with self.assertRaisesRegex(assertions.AssertionError, "duplicate Discord relay body"):
                        assertions.no_duplicate_content(self.window(*references, body=body))

    def test_fetch_and_window_preserve_reply_references(self):
        rows = self.window({"message_reference": {"message_id": "101"}},
                           {"message_reference": None, "referenced_message": {"id": "102"}}).messages
        for capture in (False, True):
            for payload in (rows, {"messages": rows}):
                with self.subTest(capture=capture, envelope=isinstance(payload, dict)):
                    captures = [] if capture else None
                    client = DiscordClient(base_url="http://offline.invalid", captures=captures, capture_after_id="10")
                    with patch("urllib.request.urlopen", return_value=_Response(payload)):
                        fetched = client.fetch_messages(TARGET, after_id="10")
                    window = assertions.Window("10")
                    for row in fetched:
                        window.add({**row, "content": "previous snapshot"})
                        window.add(row)
                    window.reconcile_snapshot(fetched, after_id="10", complete=True)
                    self.assertEqual(window.messages, rows)
                    if capture:
                        self.assertEqual(captures[0]["pages"][0]["messages"], rows)
                    assertions.no_duplicate_content(window)


if __name__ == "__main__":
    unittest.main()
