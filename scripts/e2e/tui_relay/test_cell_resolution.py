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
        self.assertNotIn("E-4", ids)
        self.assertNotIn("E-10", ids)
        self.assertNotIn("E-12", ids)
        # codex-only scenario excluded
        self.assertNotIn("E-7", ids)

    def test_claude_tui_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="claude-tui")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-1", ids)
        self.assertNotIn("E-13", ids)
        self.assertIn("E-4", ids)
        self.assertIn("E-10", ids)
        self.assertIn("E-12", ids)
        self.assertNotIn("E-7", ids)

    def test_claude_e_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="claude-e")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-1", ids)
        self.assertNotIn("E-13", ids)
        self.assertNotIn("E-4", ids)
        self.assertNotIn("E-7", ids)

    def test_codex_pipe_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="codex-pipe")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-7", ids)
        self.assertNotIn("E-13", ids)
        self.assertNotIn("E-6", ids)
        self.assertNotIn("E-4", ids)

    def test_codex_tui_scenarios(self):
        scenarios = driver.load_scenarios(self.scenarios_dir, cell="codex-tui")
        ids = {str(s.get("id")) for s in scenarios}
        self.assertIn("E-7", ids)
        self.assertIn("E-4", ids)

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
