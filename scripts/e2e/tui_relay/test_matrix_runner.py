"""Unit tests for the multi-provider E2E matrix runner."""

from __future__ import annotations

import sys
import tempfile
import threading
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

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

    def test_reset_before_each_boolean_flags(self):
        with patch("run_multi_provider_matrix.sys.argv", ["matrix"]):
            self.assertTrue(matrix.parse_args().reset_before_each)
        with patch(
            "run_multi_provider_matrix.sys.argv",
            ["matrix", "--no-reset-before-each"],
        ):
            self.assertFalse(matrix.parse_args().reset_before_each)
        with patch(
            "run_multi_provider_matrix.sys.argv",
            ["matrix", "--reset-before-each"],
        ):
            self.assertTrue(matrix.parse_args().reset_before_each)

    def test_run_cell_passes_required_coverage_class_to_driver(self):
        args = Namespace(
            base_url="http://agentdesk.test",
            scenarios="tests/e2e/tui_relay/scenarios",
            filter="E-25",
            dry_run=True,
            required_agent_mode=None,
            required_coverage_class="live",
            allow_destructive=False,
            reset_before_each=True,
            hard_reset_session_each=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            turn_start_timeout_s=5,
        )
        captured: dict[str, list[str]] = {}

        def fake_run(cmd, check=False, text=True):  # noqa: ARG001
            captured["cmd"] = list(cmd)
            out_index = cmd.index("--output") + 1
            report_dir = Path(cmd[out_index])
            report_dir.mkdir(parents=True, exist_ok=True)
            (report_dir / "report.codex-tui.json").write_text(
                """
{
  "totals": {"pass": 0, "fail": 1, "skipped": 0},
  "agent_mode_totals": {"none": 1, "controlled": 0, "real_live": 0},
  "coverage_class_totals": {"live": 0, "fixture": 1, "unsupported-known-gap": 0},
  "coverage_class_violations": [
    {"id": "E-25", "cell": "codex-tui", "coverage_class": "fixture"}
  ],
  "real_provider_contacted": false
}
""",
                encoding="utf-8",
            )
            return Namespace(returncode=1)

        with tempfile.TemporaryDirectory() as tmp, patch(
            "run_multi_provider_matrix.subprocess.run",
            side_effect=fake_run,
        ):
            result = matrix.run_cell(
                cell="codex-tui",
                channel_id="555",
                args=args,
                output_dir=Path(tmp),
                pass_index=1,
            )

        self.assertIn("--required-coverage-class", captured["cmd"])
        self.assertIn("live", captured["cmd"])
        self.assertEqual(
            result["coverage_class_totals"],
            {"live": 0, "fixture": 1, "unsupported-known-gap": 0},
        )
        self.assertEqual(result["coverage_class_violations"][0]["id"], "E-25")

    def test_matrix_coverage_totals_and_violations_aggregate_nested_reports(self):
        results = [
            {
                "kind": "cell",
                "coverage_class_totals": {
                    "live": 2,
                    "fixture": 1,
                    "unsupported-known-gap": 0,
                },
                "coverage_class_violations": [
                    {"id": "E-24", "cell": "claude-pipe"}
                ],
            },
            {
                "kind": "cross_channel",
                "id": "E-11",
                "coverage_class": "live",
            },
            {
                "kind": "cross_channel",
                "id": "E-X",
                "coverage_class": "unsupported-known-gap",
                "coverage_class_actual": "unsupported-known-gap",
                "failure_attribution": {"source": "coverage_class_gate"},
                "reason": "coverage_class gate requires live",
            },
        ]

        self.assertEqual(
            matrix._matrix_coverage_class_totals(results),  # noqa: SLF001
            {"live": 3, "fixture": 1, "unsupported-known-gap": 1},
        )
        violations = matrix._matrix_coverage_class_violations(results)  # noqa: SLF001
        self.assertEqual([item["id"] for item in violations], ["E-24", "E-X"])


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

    def test_cross_channel_non_leak_aggregates_bidirectional_failures(self):
        participants = matrix.resolve_cross_channel_participants(
            self.e11,
            channel_ids=self.channel_ids,
            selected_cells=["claude-tui", "codex-tui"],
        )
        windows = {}
        for participant in participants:
            key = matrix._participant_key(participant)  # noqa: SLF001
            window = assertions.Window(setup_marker_id="setup")
            sibling_marker = next(
                sibling["marker"]
                for sibling in participants
                if sibling["cell"] != participant["cell"]
            )
            window.add(
                _relay_msg(
                    10 + int(participant["channel_id"][-1]),
                    f"own {participant['marker']} sibling {sibling_marker}",
                )
            )
            windows[key] = window

        with self.assertRaises(matrix.CrossChannelAssertionError) as ctx:
            matrix._assert_cross_channel_non_leak(  # noqa: SLF001
                participants=participants,
                windows=windows,
            )

        message = str(ctx.exception)
        self.assertIn("claude-tui:111", message)
        self.assertIn("codex-tui:222", message)
        failed_by_channel = {
            key: [record for record in records if not record["passed"]]
            for key, records in ctx.exception.records.items()
        }
        self.assertGreaterEqual(len(failed_by_channel["claude-tui:111"]), 1)
        self.assertGreaterEqual(len(failed_by_channel["codex-tui:222"]), 1)

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
        self.assertTrue(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "real_live")
        self.assertTrue(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["coverage_class_actual"], "live")
        self.assertTrue(result["coverage_class_contract"]["satisfied"])
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

    def test_cross_channel_dispatch_failure_before_send_does_not_report_provider_contact(self):
        class FailingClient:
            def __init__(self, **kwargs):
                self.base_url = kwargs["base_url"]

            def send_control(self, channel_id, content):  # noqa: ARG002
                return {"id": "1"}

            def fetch_messages(self, channel_id, *, limit=50, after_id=None):  # noqa: ARG002
                return []

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):  # noqa: ARG002
                raise RuntimeError("provider send unavailable")

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
                FailingClient,
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
        self.assertFalse(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "none")
        self.assertFalse(result["agent_mode_contract"]["satisfied"])
        self.assertEqual(result["coverage_class_actual"], "unsupported-known-gap")
        self.assertFalse(result["coverage_class_contract"]["satisfied"])
        self.assertIn("cross-channel dispatch failed", result["reason"])

    def test_cross_channel_required_real_live_fails_missing_selected_cells(self):
        args = Namespace(
            base_url="http://agentdesk.test",
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            reset_before_each=False,
            required_agent_mode="real_live",
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

        self.assertEqual(result["status"], "fail")
        self.assertFalse(result["ok"])
        self.assertEqual(result["agent_mode_actual"], "none")
        self.assertEqual(result["failure_attribution"]["source"], "agent_mode_gate")
        self.assertIn("observed agent_mode_actual=none", result["reason"])

    def test_cross_channel_required_live_coverage_fails_known_gap_row(self):
        scenario = dict(self.e11)
        scenario["id"] = "E-X"
        scenario["coverage_class"] = "unsupported-known-gap"
        scenario["skip_reason"] = "known gap"
        args = Namespace(
            base_url="http://agentdesk.test",
            dry_run=True,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            reset_before_each=False,
            required_coverage_class="live",
            turn_start_timeout_s=5,
        )

        result = matrix.run_cross_channel_scenario(
            scenario,
            args=args,
            run_id="run-1",
            channel_ids=self.channel_ids,
            selected_cells=["claude-tui", "codex-tui"],
            pass_index=1,
        )

        self.assertEqual(result["status"], "fail")
        self.assertFalse(result["ok"])
        self.assertEqual(result["failure_attribution"]["source"], "coverage_class_gate")
        self.assertIn("declares unsupported-known-gap", result["reason"])

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
        failed_records = [
            assertion
            for channel in result["channels"]
            for assertion in channel["assertions"]
            if not assertion["passed"]
        ]
        self.assertTrue(failed_records)


def _restart_guard_health_detail() -> dict:
    return {
        "status": "unhealthy",
        "ok": False,
        "fully_recovered": False,
        "global_active": 1,
        "global_finalizing": 0,
        "mailboxes": [
            _busy_mailbox("111", provider="claude"),
            _idle_mailbox("222", provider="codex"),
        ],
    }


class RestartGuardOrchestration(unittest.TestCase):
    """#3797: E-17 holds a foreign turn active and proves the restart guard refuses."""

    def setUp(self):
        self.scenarios_dir = ROOT / "tests" / "e2e" / "tui_relay" / "scenarios"
        self.e17 = next(
            scenario
            for scenario in matrix.load_restart_guard_scenarios(self.scenarios_dir)
            if scenario.get("id") == "E-17"
        )
        self.channel_ids = {
            "claude-pipe": "101",
            "claude-tui": "111",
            "claude-e": "102",
            "codex-pipe": "201",
            "codex-tui": "222",
        }

    def _args(self, **overrides):
        base = dict(
            base_url="http://agentdesk.test",
            dry_run=False,
            queue_runtime_root="/tmp/agentdesk-e2e-test-runtime",
            reset_before_each=False,
            turn_start_timeout_s=5,
            allow_destructive=True,
            required_agent_mode=None,
            required_coverage_class=None,
        )
        base.update(overrides)
        return Namespace(**base)

    @staticmethod
    def _fake_client_class():
        sends: list[tuple[str, str, str]] = []

        class FakeClient:
            instances: list["FakeClient"] = []

            def __init__(self, *, base_url, timeout_s, handoff_to_agent, handoff_from_agent):
                self.base_url = base_url
                self.handoff_to_agent = handoff_to_agent
                FakeClient.instances.append(self)

            def send_prompt(self, channel_id, content, *, channel_kind="cc"):
                sends.append((str(channel_id), channel_kind, content))
                return {"id": "5001"}

        FakeClient.sends = sends
        return FakeClient

    def _run(
        self,
        *,
        guard,
        args=None,
        selected_cells=None,
        health_detail=None,
        allow_destructive_env=True,
    ):
        fake_client = self._fake_client_class()
        restart = MagicMock(name="restart_dcserver_for_e2e")
        cancel = MagicMock(return_value={"ok": True, "queued_remaining": 0})
        env = {"AGENTDESK_E2E_ALLOW_DESTRUCTIVE": "1"} if allow_destructive_env else {}
        with (
            patch.dict("os.environ", env, clear=False),
            patch(
                "run_multi_provider_matrix.cell_driver.discord.DiscordClient",
                fake_client,
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.reset_channel_state",
                return_value={"actions": []},
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.turn_identity_from_send_response",
                return_value={"channel_id": "111"},
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.wait_for_provider_hold_state",
                return_value={
                    "classification": "provider_hold_observed",
                    "ok_marker_seen": True,
                },
            ),
            patch(
                "run_multi_provider_matrix.cell_driver._read_health_detail",
                return_value=health_detail or _restart_guard_health_detail(),
            ),
            patch(
                "run_multi_provider_matrix.cell_driver._guard_no_foreign_active_turns",
                side_effect=guard,
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.cancel_turn",
                cancel,
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.assert_cell_idle",
                return_value={"status": "idle", "queue_files_clear": True},
            ),
            patch(
                "run_multi_provider_matrix.cell_driver.restart_dcserver_for_e2e",
                restart,
            ),
            patch("run_multi_provider_matrix.time.sleep", return_value=None),
        ):
            result = matrix.run_foreign_active_restart_guard_scenario(
                self.e17,
                args=args or self._args(),
                run_id="run-1",
                channel_ids=self.channel_ids,
                selected_cells=selected_cells or ["claude-tui", "codex-tui"],
                pass_index=1,
            )
        return result, fake_client, cancel, restart

    @staticmethod
    def _refuse_naming_foreign(*_args, **_kwargs):
        raise assertions.AssertionError(
            "refusing to restart dcserver: live mailbox state outside cell codex-tui "
            "(channel=222). Active: ['claude:111 [agent_turn_status=active, "
            "has_cancel_token=true, inflight_state_present=true]']."
        )

    def test_loads_e17_as_restart_guard_scenario(self):
        ids = {
            str(scenario.get("id"))
            for scenario in matrix.load_restart_guard_scenarios(self.scenarios_dir)
        }
        self.assertIn("E-17", ids)
        self.assertEqual(matrix.cell_driver.scenario_agent_mode(self.e17), "real_live")
        self.assertEqual(matrix.cell_driver.scenario_coverage_class(self.e17), "live")
        self.assertTrue(self.e17.get("destructive"))

    def test_resolve_roles_maps_foreign_hold_and_restart_attempt(self):
        roles = matrix.resolve_restart_guard_roles(
            self.e17,
            channel_ids=self.channel_ids,
            selected_cells=["claude-tui", "codex-tui"],
        )
        self.assertEqual(roles["foreign_hold"]["cell"], "claude-tui")
        self.assertEqual(roles["foreign_hold"]["channel_id"], "111")
        self.assertEqual(roles["foreign_hold"]["provider"], "claude")
        self.assertEqual(roles["restart_attempt"]["cell"], "codex-tui")
        self.assertEqual(roles["restart_attempt"]["channel_id"], "222")
        self.assertEqual(roles["restart_attempt"]["provider"], "codex")

    def test_required_cells_rejects_identical_cells(self):
        scenario = {
            "id": "E-X",
            "restart_guard": {
                "foreign_hold": {"cell": "codex-tui"},
                "restart_attempt": {"cell": "codex-tui"},
            },
        }
        with self.assertRaises(ValueError):
            matrix._restart_guard_required_cells(scenario)  # noqa: SLF001

    def test_passes_when_guard_refuses_naming_foreign_mailbox(self):
        result, fake_client, cancel, restart = self._run(
            guard=self._refuse_naming_foreign
        )

        self.assertEqual(result["status"], "pass", result.get("reason"))
        self.assertTrue(result["ok"])
        self.assertTrue(result["real_provider_contacted"])
        self.assertEqual(result["agent_mode_actual"], "real_live")
        self.assertEqual(result["coverage_class_actual"], "live")
        # The foreign hold prompt was dispatched to the claude-tui channel.
        self.assertEqual(len(fake_client.sends), 1)
        self.assertEqual(fake_client.sends[0][0], "111")
        self.assertIn("[E2E:E17:HOLD-OK]", fake_client.sends[0][2])
        # The /api/health/detail excerpt naming the foreign mailbox is captured.
        self.assertEqual(result["foreign_mailbox_evidence"]["channel_id"], "111")
        self.assertEqual(result["foreign_mailbox_evidence"]["provider"], "claude")
        self.assertTrue(result["foreign_mailbox_evidence"]["busy_reasons"])
        self.assertIn("claude:111", result["restart_refusal"])
        # The hold was released and both mailboxes asserted idle.
        cancel.assert_called_once()
        self.assertEqual(cancel.call_args.kwargs["channel_id"], "111")
        self.assertIn("foreign_hold", result["idle_checks"])
        self.assertIn("restart_attempt", result["idle_checks"])
        # SAFETY: the real restart is never invoked.
        restart.assert_not_called()

    def test_fails_when_guard_allows_restart_during_foreign_hold(self):
        # #2935 regression: the guard does not refuse despite an active foreign
        # mailbox — the scenario must fail and still never restart.
        result, _fake_client, cancel, restart = self._run(guard=lambda *a, **k: None)

        self.assertEqual(result["status"], "fail")
        self.assertIn("did NOT refuse", result["reason"])
        self.assertEqual(result["failure_attribution"]["source"], "assertion")
        restart.assert_not_called()
        # The foreign hold is still released on the failure path.
        cancel.assert_called_once()

    def test_fails_when_refusal_does_not_name_foreign_mailbox(self):
        def refuse_without_label(*_args, **_kwargs):
            raise assertions.AssertionError(
                "refusing to restart dcserver: something is busy"
            )

        result, _fake_client, cancel, restart = self._run(guard=refuse_without_label)

        self.assertEqual(result["status"], "fail")
        self.assertIn("did not name foreign mailbox", result["reason"])
        restart.assert_not_called()
        cancel.assert_called_once()

    def test_destructive_gate_skips_without_allow_destructive(self):
        result, fake_client, cancel, restart = self._run(
            guard=self._refuse_naming_foreign,
            args=self._args(allow_destructive=False),
            allow_destructive_env=False,
        )

        self.assertEqual(result["status"], "skipped")
        self.assertTrue(result["ok"])
        self.assertIn("destructive", result["reason"])
        self.assertEqual(fake_client.sends, [])
        restart.assert_not_called()
        cancel.assert_not_called()

    def test_destructive_gate_skip_fails_required_live_coverage(self):
        result, _fake_client, _cancel, _restart = self._run(
            guard=self._refuse_naming_foreign,
            args=self._args(allow_destructive=False, required_coverage_class="live"),
            allow_destructive_env=False,
        )

        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["failure_attribution"]["source"], "coverage_class_gate")

    def test_skips_cleanly_when_required_cell_not_selected(self):
        args = self._args(dry_run=True)
        result = matrix.run_foreign_active_restart_guard_scenario(
            self.e17,
            args=args,
            run_id="run-1",
            channel_ids=self.channel_ids,
            selected_cells=["claude-tui"],
            pass_index=1,
        )
        self.assertEqual(result["status"], "skipped")
        self.assertTrue(result["ok"])
        self.assertIn("codex-tui", result["reason"])

    def test_dry_run_reports_pass_without_contacting_provider(self):
        fake_client = self._fake_client_class()
        with (
            patch.dict("os.environ", {"AGENTDESK_E2E_ALLOW_DESTRUCTIVE": "1"}, clear=False),
            patch(
                "run_multi_provider_matrix.cell_driver.discord.DiscordClient", fake_client
            ),
        ):
            result = matrix.run_foreign_active_restart_guard_scenario(
                self.e17,
                args=self._args(dry_run=True),
                run_id="run-1",
                channel_ids=self.channel_ids,
                selected_cells=["claude-tui", "codex-tui"],
                pass_index=1,
            )
        self.assertEqual(result["status"], "pass")
        self.assertTrue(result["ok"])
        self.assertEqual(result["coverage_class_actual"], "live")
        self.assertFalse(result["real_provider_contacted"])
        self.assertEqual(fake_client.sends, [])


if __name__ == "__main__":
    unittest.main()
