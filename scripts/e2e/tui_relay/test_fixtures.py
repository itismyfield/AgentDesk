"""Unit tests for local relay fixture replay primitives (#2971/#2972)."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

from tui_relay import assertions, fixtures  # noqa: E402


class LocalReplayFixtures(unittest.TestCase):
    def test_croncreate_background_delivers_result_without_assistant_text(self):
        result = fixtures.replay_fixture(
            {
                "kind": "cron_create_background",
                "provider": "claude",
                "frames": [
                    {
                        "type": "assistant",
                        "message": {
                            "content": [
                                {
                                    "type": "tool_use",
                                    "id": "toolu_cron",
                                    "name": "CronCreate",
                                }
                            ]
                        },
                    },
                    {
                        "type": "system",
                        "subtype": "task_started",
                        "task_id": "cron-1",
                        "tool_use_id": "toolu_cron",
                        "task_type": "cron",
                    },
                    {
                        "type": "system",
                        "subtype": "task_notification",
                        "task_id": "cron-1",
                        "status": "completed",
                        "source_tool": "CronCreate",
                    },
                    {
                        "type": "result",
                        "subtype": "success",
                        "result": "[E2E:E24:CRON] relayed",
                    },
                ],
            },
            cell="claude-pipe",
            channel_id="42",
            scenario_id="E-24",
            run_id="unit",
        )

        state = result["state"]
        self.assertEqual(state["task_notification_kind"], "Background")
        self.assertEqual(state["task_notification_source"], "CronCreate")
        self.assertFalse(state["assistant_text_seen"])
        self.assertTrue(state["finalized"])
        self.assertEqual(state["active_turn"], "none")
        self.assertEqual(state["result_text_source"], "result.result")
        self.assertIn("[E2E:E24:CRON]", result["messages"][0]["content"])

    def test_croncreate_fixture_rejects_non_cron_source(self):
        with self.assertRaises(assertions.AssertionError) as ctx:
            fixtures.replay_fixture(
                {
                    "kind": "cron_create_background",
                    "frames": [
                        {
                            "type": "system",
                            "subtype": "task_notification",
                            "task_id": "bg-1",
                            "status": "completed",
                            "source_tool": "BackgroundTask",
                        },
                        {"type": "result", "subtype": "success", "result": "done"},
                    ],
                },
                cell="claude-pipe",
                channel_id="42",
                scenario_id="E-24",
                run_id="unit",
            )
        self.assertIn("CronCreate source", str(ctx.exception))

    def test_codex_modern_schema_uses_task_complete_last_agent_message(self):
        result = fixtures.replay_fixture(
            {
                "kind": "codex_modern_schema",
                "provider": "codex",
                "frames": [
                    {"type": "thread.started", "thread_id": "thread-1"},
                    {
                        "type": "response_item",
                        "payload": {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {"type": "output_text", "text": "partial response_item"}
                            ],
                        },
                    },
                    {
                        "type": "event_msg",
                        "payload": {
                            "type": "task_complete",
                            "turn_id": "turn-1",
                            "last_agent_message": "[E2E:E25:FINAL] task complete",
                        },
                    },
                ],
            },
            cell="codex-tui",
            channel_id="42",
            scenario_id="E-25",
            run_id="unit",
        )

        state = result["state"]
        self.assertTrue(state["task_complete_seen"])
        self.assertEqual(state["task_complete_turn_id"], "turn-1")
        self.assertEqual(state["result_text_source"], "task_complete.last_agent_message")
        self.assertEqual(state["deliveries"], ["[E2E:E25:FINAL] task complete"])
        self.assertTrue(state["followup_ready"])

    def test_followup_probe_marks_probe_accepted(self):
        record = {
            "fixture_state": {
                "followup_ready": True,
                "active_turn": "none",
                "queue_depth": 0,
                "pending_discord_callback": False,
            }
        }

        probe = fixtures.probe_followup_ready(record, {"prompt": "next"})

        self.assertTrue(probe["accepted"])
        self.assertTrue(record["fixture_state"]["followup_probe_accepted"])


if __name__ == "__main__":
    unittest.main()
