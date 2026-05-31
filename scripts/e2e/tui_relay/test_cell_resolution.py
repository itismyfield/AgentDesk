"""Unit tests for cell helpers and scenario filtering.

Run with: python3 -m pytest scripts/e2e/tui_relay/test_cell_resolution.py
Or:       python3 scripts/e2e/tui_relay/test_cell_resolution.py
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_tui_relay as driver  # noqa: E402


class CellHelpers(unittest.TestCase):
    def test_supported_cells_cover_provider_runtime_matrix(self):
        self.assertEqual(
            set(driver.SUPPORTED_CELLS),
            {"claude-pipe", "claude-tui", "claude-e", "codex-pipe", "codex-tui"},
        )

    def test_cell_provider(self):
        self.assertEqual(driver.cell_provider("claude-pipe"), "claude")
        self.assertEqual(driver.cell_provider("claude-tui"), "claude")
        self.assertEqual(driver.cell_provider("claude-e"), "claude")
        self.assertEqual(driver.cell_provider("codex-pipe"), "codex")
        self.assertEqual(driver.cell_provider("codex-tui"), "codex")

    def test_cell_runtime(self):
        self.assertEqual(driver.cell_runtime("claude-pipe"), "pipe")
        self.assertEqual(driver.cell_runtime("claude-tui"), "tui")
        self.assertEqual(driver.cell_runtime("claude-e"), "e")
        self.assertEqual(driver.cell_runtime("codex-pipe"), "pipe")
        self.assertEqual(driver.cell_runtime("codex-tui"), "tui")

    def test_session_name_uses_cell_workspace(self):
        self.assertEqual(
            driver.cell_session_name("claude-pipe"),
            "AgentDesk-claude-adk-claude-pipe-e2e",
        )
        self.assertEqual(
            driver.cell_session_name("codex-tui"),
            "AgentDesk-codex-adk-codex-tui-e2e",
        )

    def test_default_agent_id(self):
        self.assertEqual(driver.cell_default_agent("claude-pipe"), "adk-claude-pipe-e2e")
        self.assertEqual(driver.cell_default_agent("codex-tui"), "adk-codex-tui-e2e")

    def test_channel_kind_matches_provider(self):
        self.assertEqual(driver.cell_channel_kind("claude-pipe"), "cc")
        self.assertEqual(driver.cell_channel_kind("claude-tui"), "cc")
        self.assertEqual(driver.cell_channel_kind("claude-e"), "cc")
        self.assertEqual(driver.cell_channel_kind("codex-pipe"), "cdx")
        self.assertEqual(driver.cell_channel_kind("codex-tui"), "cdx")

    def test_workspace_substring_safety(self):
        for cell in driver.SUPPORTED_CELLS:
            sub = driver.cell_workspace_substring(cell)
            self.assertTrue(sub.startswith("adk-"))
            self.assertIn("e2e", sub)
            # legacy adk-dash workspaces must never match a new cell substring.
            self.assertNotIn("dash", sub)


class ScenarioFilter(unittest.TestCase):
    def setUp(self):
        self.scenarios_dir = ROOT / "tests" / "e2e" / "tui_relay" / "scenarios"

    def test_claude_pipe_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="claude-pipe")
        ids = {str(s.get("id")) for s in scenarios}
        # claude-pipe gets the basic + compact + restart scenarios but NOT
        # TUI-keystroke ones (E-4, E-10, E-12).
        self.assertIn("E-1", ids)
        self.assertIn("E-6", ids)
        self.assertIn("E-13", ids)
        self.assertIn("E-8", ids)
        self.assertIn("E-18", ids)
        self.assertIn("E-20", ids)
        self.assertIn("E-22", ids)
        self.assertIn("E-23", ids)
        self.assertIn("E-24", ids)
        self.assertNotIn("E-4", ids)
        self.assertNotIn("E-10", ids)
        self.assertNotIn("E-12", ids)
        # codex-only scenario excluded
        self.assertNotIn("E-7", ids)

    def test_claude_tui_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="claude-tui")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-1", ids)
        self.assertIn("E-16", ids)
        self.assertIn("E-18", ids)
        self.assertIn("E-19", ids)
        self.assertIn("E-20", ids)
        self.assertIn("E-21", ids)
        self.assertIn("E-22", ids)
        self.assertIn("E-23", ids)
        self.assertNotIn("E-13", ids)
        self.assertIn("E-4", ids)
        self.assertIn("E-10", ids)
        self.assertIn("E-12", ids)
        e16 = next(s for s in scenarios if s.get("id") == "E-16")
        self.assertIn("skip_reason", e16)
        self.assertIn("acceptance_criteria", e16)
        self.assertNotIn("E-7", ids)

    def test_claude_e_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="claude-e")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-1", ids)
        self.assertIn("E-20", ids)
        self.assertIn("E-23", ids)
        self.assertNotIn("E-13", ids)
        self.assertNotIn("E-4", ids)
        self.assertNotIn("E-7", ids)
        self.assertNotIn("E-22", ids)
        self.assertNotIn("E-18", ids)

    def test_codex_pipe_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="codex-pipe")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-7", ids)
        self.assertIn("E-18", ids)
        self.assertIn("E-20", ids)
        self.assertIn("E-25", ids)
        self.assertNotIn("E-22", ids)
        self.assertNotIn("E-23", ids)
        self.assertNotIn("E-13", ids)
        self.assertNotIn("E-6", ids)
        self.assertNotIn("E-4", ids)

    def test_codex_tui_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="codex-tui")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-7", ids)
        self.assertIn("E-4", ids)
        self.assertIn("E-17", ids)
        self.assertIn("E-18", ids)
        self.assertIn("E-19", ids)
        self.assertIn("E-20", ids)
        self.assertIn("E-21", ids)
        self.assertIn("E-25", ids)
        self.assertNotIn("E-22", ids)
        self.assertNotIn("E-23", ids)
        e17 = next(s for s in scenarios if s.get("id") == "E-17")
        self.assertIn("skip_reason", e17)
        self.assertIn("acceptance_criteria", e17)
        e18 = next(s for s in scenarios if s.get("id") == "E-18")
        self.assertNotIn("skip_reason", e18)
        self.assertIn("acceptance_criteria", e18)

    def test_e18_cancel_turn_scope_is_relay_backed_non_claude_e(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell in {"claude-pipe", "claude-tui", "codex-pipe", "codex-tui"}:
                self.assertIn("E-18", ids)
            else:
                self.assertNotIn("E-18", ids)

    def test_e18_is_unskipped_and_uses_provider_hold_fixture(self):
        for cell in {"claude-pipe", "claude-tui", "codex-pipe", "codex-tui"}:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            e18 = next(s for s in scenarios if s.get("id") == "E-18")
            self.assertNotIn("skip_reason", e18)
            hold_steps = [
                step["send_provider_hold_prompt"]
                for step in e18["steps"]
                if "send_provider_hold_prompt" in step
            ]
            self.assertEqual(len(hold_steps), 1)
            self.assertEqual(hold_steps[0]["ok_marker"], "[E2E:E18:OK]")
            self.assertEqual(hold_steps[0]["late_marker"], "[E2E:E18:LATE]")
            wait_steps = [
                step["wait_for_provider_hold_state"]
                for step in e18["steps"]
                if "wait_for_provider_hold_state" in step
            ]
            self.assertEqual(len(wait_steps), 1)
            self.assertEqual(wait_steps[0]["ok_marker"], "[E2E:E18:OK]")
            self.assertEqual(wait_steps[0]["late_marker"], "[E2E:E18:LATE]")
            health_steps = [
                step["assert_health"] for step in e18["steps"] if "assert_health" in step
            ]
            self.assertEqual(health_steps[0]["global_active_max"], 0)
            self.assertEqual(health_steps[0]["global_finalizing_max"], 0)

    def test_e8_health_assertion_waits_for_restart_finalizing_drain(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            e8 = next(s for s in scenarios if s.get("id") == "E-8")
            health_steps = [
                step["assert_health"] for step in e8["steps"] if "assert_health" in step
            ]
            self.assertEqual(len(health_steps), 1)
            self.assertGreaterEqual(health_steps[0]["timeout_s"], 30)
            self.assertLessEqual(health_steps[0]["poll_interval_s"], 2)
            self.assertEqual(health_steps[0]["global_active_max"], 0)
            self.assertEqual(health_steps[0]["global_finalizing_max"], 0)

    def test_e19_session_continuity_scope_is_tui_only(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell in {"claude-tui", "codex-tui"}:
                self.assertIn("E-19", ids)
                e19 = next(s for s in scenarios if s.get("id") == "E-19")
                prompt_text = "\n".join(
                    str(step.get("send_prompt", ""))
                    for step in e19["steps"]
                    if "send_prompt" in step
                )
                self.assertIn("E19_SECRET_ALPHA_5AF3C2", prompt_text)
                self.assertIn(
                    {"text_present": "[E2E:E19:POST] E19_SECRET_ALPHA_5AF3C2"},
                    e19["assertions"],
                )
                health_steps = [
                    step["assert_health"]
                    for step in e19["steps"]
                    if "assert_health" in step
                ]
                self.assertEqual(len(health_steps), 1)
                self.assertGreaterEqual(health_steps[0]["timeout_s"], 60)
                self.assertLessEqual(health_steps[0]["poll_interval_s"], 2)
                self.assertEqual(health_steps[0]["global_active_max"], 0)
                self.assertEqual(health_steps[0]["global_finalizing_max"], 0)
            else:
                self.assertNotIn("E-19", ids)

    def test_e20_concurrent_dispatch_covers_all_cells(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            self.assertIn("E-20", ids)

    def test_e21_direct_control_strip_scope_is_tui_only(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell in {"claude-tui", "codex-tui"}:
                self.assertIn("E-21", ids)
                e21 = next(s for s in scenarios if s.get("id") == "E-21")
                self.assertIn("acceptance_criteria", e21)
                self.assertNotIn("skip_reason", e21)
            else:
                self.assertNotIn("E-21", ids)

    def test_e22_tool_use_text_completeness_scope_is_claude_relay_backed(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell not in {"claude-pipe", "claude-tui"}:
                self.assertNotIn("E-22", ids)
                continue
            e22 = next(s for s in scenarios if s.get("id") == "E-22")
            self.assertIn("acceptance_criteria", e22)
            self.assertNotIn("skip_reason", e22)
            wait_steps = [
                step["wait_for_provider_hold_state"]
                for step in e22["steps"]
                if "wait_for_provider_hold_state" in step
            ]
            self.assertEqual(len(wait_steps), 1)
            self.assertEqual(wait_steps[0]["ok_marker"], "[E2E:E22:PRE]")
            self.assertEqual(wait_steps[0]["late_marker"], "[E2E:E22:HEAD]")
            self.assertIn({"provider_hold_marker_seen": "[E2E:E22:PRE]"}, e22["assertions"])
            self.assertIn(
                {
                    "completion_chrome_after_body": {
                        "body_marker": "[E2E:E22:TAIL]",
                        "required": True,
                    }
                },
                e22["assertions"],
            )

    def test_e23_premature_completion_guard_covers_claude_tool_capable_cells(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell not in {"claude-pipe", "claude-tui", "claude-e"}:
                self.assertNotIn("E-23", ids)
                continue
            e23 = next(s for s in scenarios if s.get("id") == "E-23")
            self.assertIn("acceptance_criteria", e23)
            self.assertNotIn("skip_reason", e23)
            self.assertIn(
                {
                    "completion_chrome_after_body": {
                        "body_marker": "[E2E:E23:BODY-END]",
                        "required": True,
                    }
                },
                e23["assertions"],
            )

    def test_e24_croncreate_fixture_scope_and_contract(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell == "claude-pipe":
                self.assertIn("E-24", ids)
                e24 = next(s for s in scenarios if s.get("id") == "E-24")
                self.assertEqual(e24.get("execution"), "fixture")
                self.assertIn(
                    {
                        "fixture_task_notification": {
                            "kind": "Background",
                            "source": "CronCreate",
                            "status": "completed",
                        }
                    },
                    e24["assertions"],
                )
                self.assertTrue(driver.is_local_fixture_scenario(e24))
            else:
                self.assertNotIn("E-24", ids)

    def test_e25_codex_modern_schema_fixture_scope_and_contract(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell in {"codex-pipe", "codex-tui"}:
                self.assertIn("E-25", ids)
                e25 = next(s for s in scenarios if s.get("id") == "E-25")
                self.assertEqual(e25.get("execution"), "fixture")
                self.assertIn(
                    {
                        "fixture_task_complete_finalized": {
                            "turn_id": "codex-modern-e25-turn",
                            "result_text_source": "task_complete.last_agent_message",
                        }
                    },
                    e25["assertions"],
                )
                self.assertTrue(driver.is_local_fixture_scenario(e25))
            else:
                self.assertNotIn("E-25", ids)

    def test_e11_excluded_everywhere(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            self.assertNotIn(
                "E-11",
                ids,
                f"E-11 (cross-cell concurrency) should be excluded from cell {cell}",
            )

    def test_e9_restart_waits_for_deterministic_end_marker(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="codex-tui")
        e9 = next(s for s in scenarios if s.get("id") == "E-9")
        prompt = e9["steps"][0]["send_prompt"]
        waits = [
            step["wait_for_discord_text"]
            for step in e9["steps"]
            if "wait_for_discord_text" in step
        ]

        self.assertIn("[E2E:E9:STREAM_OK]", prompt)
        self.assertIn("[E2E:E9:END]", prompt)
        self.assertEqual(waits, ["[E2E:E9:STREAM_OK]", "[E2E:E9:END]"])
        self.assertNotIn("E-9", waits)
        self.assertIn({"text_present": "[E2E:E9:END]"}, e9["assertions"])

    def test_no_legacy_adk_dash_residue_in_scenarios(self):
        """Scenarios must not embed the legacy `adk-dash` reverify substring.

        New session names are AgentDesk-{provider}-adk-{cell}-e2e, so any
        kill_pane / destructive guard that reverifies against `adk-dash`
        would fail closed under the cell driver.
        """
        for path in sorted(self.scenarios_dir.glob("*.yaml")):
            text = path.read_text(encoding="utf-8")
            self.assertNotIn(
                "adk-dash",
                text,
                f"{path.name} still references legacy adk-dash workspace",
            )


if __name__ == "__main__":
    unittest.main()
