"""Unit tests for the multi-provider E2E matrix runner."""

from __future__ import annotations

import sys
import tempfile
import threading
import unittest
from pathlib import Path
from argparse import Namespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_multi_provider_matrix as matrix  # noqa: E402
from tui_relay import assertions  # noqa: E402


class MatrixConfig(unittest.TestCase):
    def test_load_channel_ids_from_agentdesk_yaml_shape(self):
        yaml = """
agents:
  - id: adk-claude-pipe-e2e
    channels:
      claude: {id: "111"}
  - id: adk-claude-tui-e2e
    channels:
      claude: {id: "222"}
  - id: adk-claude-e-e2e
    channels:
      claude: {id: "333"}
  - id: adk-codex-pipe-e2e
    channels:
      codex: {id: "444"}
  - id: adk-codex-tui-e2e
    channels:
      codex: {id: "555"}
"""
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "agentdesk.yaml"
            path.write_text(yaml, encoding="utf-8")
            resolved = matrix.load_channel_ids(path)

        self.assertEqual(resolved["claude-pipe"], "111")
        self.assertEqual(resolved["claude-tui"], "222")
        self.assertEqual(resolved["claude-e"], "333")
        self.assertEqual(resolved["codex-pipe"], "444")
        self.assertEqual(resolved["codex-tui"], "555")

    def test_parse_cells_rejects_unknown_cell(self):
        with self.assertRaises(ValueError):
            matrix.parse_cells("claude-pipe,unknown")


def _relay_msg(msg_id: int, content: str) -> dict:
    return {
        "id": str(msg_id),
        "content": content,
        "author": {"id": "999", "bot": True},
        "type": 0,
        "timestamp": "2026-05-31T00:00:00Z",
    }


def _idle_mailbox(channel_id: str = "42", provider: str = "codex") -> dict:
    return {
        "provider": provider,
        "channel_id": channel_id,
        "agent_turn_status": "idle",
        "has_cancel_token": False,
        "queue_depth": 0,
        "recovery_started": False,
        "active_user_message_id": None,
        "inflight_state_present": False,
        "active_dispatch_present": False,
        "relay_stall_state": "healthy",
        "relay_health": {
            "active_turn": "none",
            "bridge_inflight_present": False,
            "mailbox_has_cancel_token": False,
            "mailbox_active_user_msg_id": None,
            "queue_depth": 0,
            "pending_discord_callback_msg_id": None,
            "pending_thread_proof": False,
            "stale_thread_proof": False,
            "desynced": False,
        },
    }


def _busy_mailbox(channel_id: str = "99", provider: str = "claude") -> dict:
    mailbox = _idle_mailbox(channel_id=channel_id, provider=provider)
    mailbox.update(
        {
            "agent_turn_status": "active",
            "has_cancel_token": True,
            "queue_depth": 1,
            "active_user_message_id": 1510404747188768880,
            "inflight_state_present": True,
            "relay_stall_state": "tmux_alive_relay_dead",
        }
    )
    mailbox["relay_health"].update(
        {
            "active_turn": "foreground",
            "bridge_inflight_present": True,
            "mailbox_has_cancel_token": True,
            "mailbox_active_user_msg_id": 1510404747188768880,
            "queue_depth": 1,
            "pending_discord_callback_msg_id": 1510408165194203269,
            "desynced": True,
        }
    )
    return mailbox


class CrossChannelMatrix(unittest.TestCase):
    def setUp(self):
        self.scenarios_dir = ROOT / "tests" / "e2e" / "tui_relay" / "scenarios"
        self.e11 = next(
            scenario
            for scenario in matrix.load_cross_channel_scenarios(self.scenarios_dir)
            if scenario.get("id") == "E-11"
        )
        self.channel_ids = {
            "claude-pipe": "101",
            "claude-tui": "111",
            "claude-e": "102",
            "codex-pipe": "201",
            "codex-tui": "222",
        }

    def test_loads_e11_as_cross_channel_not_cell_scenario(self):
        scenarios = matrix.load_cross_channel_scenarios(self.scenarios_dir)
        ids = {str(scenario.get("id")) for scenario in scenarios}
        self.assertIn("E-11", ids)

        participants = matrix.resolve_cross_channel_participants(
            self.e11,
            channel_ids=self.channel_ids,
            selected_cells=["claude-tui", "codex-tui"],
        )

        self.assertEqual(
            [(p["cell"], p["channel_id"], p["handoff_to_agent"]) for p in participants],
            [
                ("claude-tui", "111", "adk-claude-tui-e2e"),
                ("codex-tui", "222", "adk-codex-tui-e2e"),
            ],
        )

    def test_cross_channel_guard_blocks_foreign_live_mailbox(self):
        participants = matrix.resolve_cross_channel_participants(
            self.e11,
            channel_ids=self.channel_ids,
            selected_cells=["claude-tui", "codex-tui"],
        )
        detail = {
            "status": "unhealthy",
            "ok": False,
            "fully_recovered": False,
            "global_finalizing": 0,
            "mailboxes": [
                _idle_mailbox("111", provider="claude"),
                _idle_mailbox("222", provider="codex"),
                _busy_mailbox("999", provider="claude"),
            ],
        }

        with (
            patch("run_multi_provider_matrix.cell_driver._read_health_detail", return_value=detail),
            patch("run_multi_provider_matrix._active_sessions_payload", return_value=[]),
        ):
            with self.assertRaises(assertions.AssertionError) as ctx:
                matrix.guard_no_foreign_live_state(
                    base_url="http://agentdesk.test",
                    participants=participants,
                )

        message = str(ctx.exception)
        self.assertIn("outside the selected configured e2e cells", message)
        self.assertIn("claude:999", message)

    def test_cross_channel_scenario_skips_cleanly_when_pair_not_selected(self):
        args = Namespace(
            base_url="http://agentdesk.test",
            dry_run=True,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            reset_before_each=True,
            turn_start_timeout_s=5,
        )

        result = matrix.run_cross_channel_scenario(
            self.e11,
            args=args,
            run_id="run-1",
            channel_ids=self.channel_ids,
            selected_cells=["claude-tui"],
            pass_index=1,
        )

        self.assertEqual(result["status"], "skipped")
        self.assertTrue(result["ok"])
        self.assertIn("codex-tui", result["reason"])

    def test_cross_channel_dispatch_is_concurrent_and_asserts_non_leak(self):
        class FakeClient:
            lock = threading.Lock()
            active = 0
            max_active = 0
            sent: list[tuple[str | None, str, str, str]] = []
            controls: list[tuple[str, str]] = []

            def __init__(
                self,
                *,
                base_url: str,
                timeout_s: float,
                handoff_to_agent: str | None,
                handoff_from_agent: str | None,
            ):
                self.base_url = base_url
                self.timeout_s = timeout_s
                self.handoff_to_agent = handoff_to_agent
                self.handoff_from_agent = handoff_from_agent

            def send_control(self, channel_id, content):
                self.controls.append((str(channel_id), content))
                return {"id": str(len(self.controls))}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):
                cls = type(self)
                with cls.lock:
                    cls.active += 1
                    cls.max_active = max(cls.max_active, cls.active)
                    cls.sent.append(
                        (self.handoff_to_agent, str(channel_id), channel_kind, content)
                    )
                threading.Event().wait(0.05)
                with cls.lock:
                    cls.active -= 1
                return {"id": str(len(cls.sent))}

            def wait_for_message(
                self,
                channel_id,
                *,
                predicate,
                after_id=None,  # noqa: ARG002
                timeout_s=120.0,  # noqa: ARG002
                poll_interval_s=5.0,  # noqa: ARG002
                debug_label=None,  # noqa: ARG002
            ):
                marker = (
                    "[E2E:E11:CC-MARKER]"
                    if str(channel_id) == "111"
                    else "[E2E:E11:CDX-MARKER]"
                )
                message = _relay_msg(10 + len(type(self).sent), f"answer {marker}")
                return (message if predicate(message) else None), [message]

        args = Namespace(
            base_url="http://agentdesk.test",
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            reset_before_each=True,
            turn_start_timeout_s=5,
        )

        with (
            patch(
                "run_multi_provider_matrix.cell_driver.discord.DiscordClient",
                FakeClient,
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.reset_channel_state",
                return_value={"actions": []},
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
            patch(
                "run_multi_provider_matrix.guard_no_foreign_live_state",
                return_value={"status": "isolated"},
            ),
            patch("run_multi_provider_matrix.time.sleep", return_value=None),
        ):
            result = matrix.run_cross_channel_scenario(
                self.e11,
                args=args,
                run_id="run-1",
                channel_ids=self.channel_ids,
                selected_cells=["claude-tui", "codex-tui"],
                pass_index=1,
            )

        self.assertEqual(result["status"], "pass")
        self.assertTrue(result["ok"])
        self.assertEqual(FakeClient.max_active, 2)
        self.assertIn(
            (
                "adk-claude-tui-e2e",
                "111",
                "cc",
                "응답에 정확히 한 줄로 [E2E:E11:CC-MARKER] 만 출력해줘.",
            ),
            FakeClient.sent,
        )
        self.assertIn(
            (
                "adk-codex-tui-e2e",
                "222",
                "cdx",
                "응답에 정확히 한 줄로 [E2E:E11:CDX-MARKER] 만 출력해줘.",
            ),
            FakeClient.sent,
        )
        all_specs = [
            assertion["spec"]
            for channel in result["channels"]
            for assertion in channel["assertions"]
        ]
        self.assertIn(
            {"marker_absent": {"marker": "[E2E:E11:CDX-MARKER]", "surface": "raw"}},
            all_specs,
        )
        self.assertIn(
            {"marker_absent": {"marker": "[E2E:E11:CC-MARKER]", "surface": "relay"}},
            all_specs,
        )

    def test_cross_channel_non_leak_fails_on_sibling_marker(self):
        class LeakyClient:
            def __init__(self, **kwargs):
                self.base_url = kwargs["base_url"]

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                return {"id": str(channel_id)}

            def wait_for_message(
                self,
                channel_id,
                *,
                predicate,
                after_id=None,  # noqa: ARG002
                timeout_s=120.0,  # noqa: ARG002
                poll_interval_s=5.0,  # noqa: ARG002
                debug_label=None,  # noqa: ARG002
            ):
                if str(channel_id) == "111":
                    content = "answer [E2E:E11:CC-MARKER] leak [E2E:E11:CDX-MARKER]"
                else:
                    content = "answer [E2E:E11:CDX-MARKER]"
                message = _relay_msg(30 + int(str(channel_id)[-1]), content)
                return (message if predicate(message) else None), [message]

        args = Namespace(
            base_url="http://agentdesk.test",
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            reset_before_each=False,
            turn_start_timeout_s=5,
        )

        with (
            patch(
                "run_multi_provider_matrix.cell_driver.discord.DiscordClient",
                LeakyClient,
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.assert_cell_idle",
                return_value={"status": "idle", "mailboxes_seen": 1},
            ),
            patch(
                "run_multi_provider_matrix.guard_no_foreign_live_state",
                return_value={"status": "isolated"},
            ),
            patch("run_multi_provider_matrix.time.sleep", return_value=None),
        ):
            result = matrix.run_cross_channel_scenario(
                self.e11,
                args=args,
                run_id="run-1",
                channel_ids=self.channel_ids,
                selected_cells=["claude-tui", "codex-tui"],
                pass_index=1,
            )

        self.assertEqual(result["status"], "fail")
        self.assertIn("unexpected marker", result["reason"])


if __name__ == "__main__":
    unittest.main()
