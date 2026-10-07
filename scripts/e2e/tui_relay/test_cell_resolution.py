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
            {"claude-pipe", "claude-tui", "codex-pipe", "codex-tui", "claude-herdr", "codex-herdr"},
        )

    def test_cell_provider(self):
        self.assertEqual(driver.cell_provider("claude-pipe"), "claude")
        self.assertEqual(driver.cell_provider("claude-tui"), "claude")
        self.assertEqual(driver.cell_provider("codex-pipe"), "codex")
        self.assertEqual(driver.cell_provider("codex-tui"), "codex")

    def test_cell_runtime(self):
        self.assertEqual(driver.cell_runtime("claude-pipe"), "pipe")
        self.assertEqual(driver.cell_runtime("claude-tui"), "tui")
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
        # #3797: E-16 is now an executable live scenario, not a #2935 stub.
        self.assertNotIn("skip_reason", e16)
        self.assertIn("acceptance_criteria", e16)
        self.assertEqual(driver.scenario_agent_mode(e16), "real_live")
        self.assertEqual(driver.scenario_coverage_class(e16), "live")
        self.assertTrue(e16.get("steps"))
        self.assertNotIn("E-7", ids)

    def test_retired_claude_e_has_no_scenarios(self):
        self.assertEqual(
            driver.load_scenarios(self.scenarios_dir, cell="claude-e"), [],
        )

    def test_all_scenario_cells_are_supported(self):
        paths = sorted(self.scenarios_dir.glob("*.yaml"))
        self.assertTrue(paths)
        for path in paths:
            with self.subTest(path=path.name):
                scenario = driver.yaml.safe_load(path.read_text(encoding="utf-8"))
                self.assertLessEqual(set(scenario["cells"]), set(driver.SUPPORTED_CELLS))

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
        # #3797: E-17 is now an orchestrator-owned restart-guard scenario
        # (cells: []), so the single-cell codex-tui driver no longer loads it
        # — same convention as the E-11 cross-channel scenario.
        self.assertNotIn("E-17", ids)
        self.assertNotIn("E-11", ids)
        self.assertIn("E-18", ids)
        self.assertIn("E-19", ids)
        self.assertIn("E-20", ids)
        self.assertIn("E-21", ids)
        self.assertIn("E-25", ids)
        self.assertNotIn("E-22", ids)
        self.assertNotIn("E-23", ids)
        e18 = next(s for s in scenarios if s.get("id") == "E-18")
        self.assertNotIn("skip_reason", e18)
        self.assertIn("acceptance_criteria", e18)

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
            assertion_kinds = {
                next(iter(assertion.keys())) for assertion in e18["assertions"]
            }
            self.assertNotIn("relay_latency_within", assertion_kinds)
            self.assertIn(
                {"raw_message_count_between_markers": {"min": 0, "max": 36}},
                e18["assertions"],
            )
            self.assertIn(
                {"provider_hold_marker_seen": "[E2E:E18:OK]"},
                e18["assertions"],
            )
            self.assertIn(
                {
                    "marker_absent": {
                        "marker": "[E2E:E18:LATE]",
                        "surface": "relay",
                    }
                },
                e18["assertions"],
            )

    def test_e8_health_assertion_waits_for_restart_finalizing_drain(self):
        expected_e8_cells = {
            "claude-pipe",
            "claude-tui",
            "codex-pipe",
            "codex-tui",
            "claude-herdr", "codex-herdr",
        }
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell not in expected_e8_cells:
                self.assertNotIn("E-8", ids)
                continue
            self.assertIn("E-8", ids)
            e8 = next(s for s in scenarios if s.get("id") == "E-8")
            health_steps = [
                step["assert_health"] for step in e8["steps"] if "assert_health" in step
            ]
            self.assertEqual(len(health_steps), 1)
            self.assertGreaterEqual(health_steps[0]["timeout_s"], 30)
            self.assertLessEqual(health_steps[0]["poll_interval_s"], 2)
            self.assertEqual(health_steps[0]["global_active_max"], 0)
            self.assertEqual(health_steps[0]["global_finalizing_max"], 0)

    def test_e10_health_assertion_waits_for_stranded_draft_finalizing_drain(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell not in {"claude-tui", "codex-tui"}:
                self.assertNotIn("E-10", ids)
                continue
            self.assertIn("E-10", ids)
            e10 = next(s for s in scenarios if s.get("id") == "E-10")
            health_steps = [
                step["assert_health"] for step in e10["steps"] if "assert_health" in step
            ]
            self.assertEqual(len(health_steps), 1)
            self.assertGreaterEqual(health_steps[0]["timeout_s"], 30)
            self.assertLessEqual(health_steps[0]["poll_interval_s"], 2)
            self.assertIn(
                "global_active_counter_out_of_bounds",
                health_steps[0]["forbid_degraded_reasons"],
            )
            self.assertEqual(health_steps[0]["global_active_max"], 0)
            self.assertEqual(health_steps[0]["global_finalizing_max"], 0)

    def test_e19_session_continuity_scope_is_tui_and_herdr(self):
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            ids = {str(s.get("id")) for s in scenarios}
            if cell in {"claude-tui", "codex-tui", "claude-herdr", "codex-herdr"}:
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
            if cell not in {"claude-pipe", "claude-tui", "claude-herdr"}:
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
            if cell not in {"claude-pipe", "claude-tui"}:
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
                self.assertEqual(e24.get("coverage_class"), "fixture")
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
                self.assertEqual(e25.get("coverage_class"), "fixture")
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

    def test_known_gap_rows_are_machine_readable_and_non_live(self):
        expected = {
            "E-26": {"claude-pipe"},
            "E-27": {"codex-pipe", "codex-tui"},
            "E-28": {"codex-pipe", "codex-tui"},
        }
        for cell in driver.SUPPORTED_CELLS:
            scenarios = driver.load_scenarios(self.scenarios_dir, cell=cell)
            by_id = {str(s.get("id")): s for s in scenarios}
            for scenario_id, cells in expected.items():
                if cell in cells:
                    scenario = by_id[scenario_id]
                    self.assertEqual(
                        scenario.get("coverage_class"),
                        "unsupported-known-gap",
                    )
                    self.assertIn("skip_reason", scenario)
                    self.assertIn("acceptance_criteria", scenario)
                else:
                    self.assertNotIn(scenario_id, by_id)

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


class HerdrCells(unittest.TestCase):
    def test_resolution_and_no_tmux_session(self):
        for provider in ('claude', 'codex'):
            cell = provider + '-herdr'
            self.assertEqual(driver.cell_provider(cell), provider)
            self.assertEqual(driver.cell_runtime(cell), 'herdr')
            self.assertEqual(driver.cell_default_agent(cell), f'adk-{provider}-tui-e2e')
            with self.assertRaisesRegex(ValueError, 'no tmux'):
                driver.cell_session_name(cell)

    def test_required_scenarios(self):
        for cell in ('claude-herdr', 'codex-herdr'):
            ids = {s['id'] for s in driver.load_scenarios(ROOT / 'tests/e2e/tui_relay/scenarios', cell=cell)}
            self.assertTrue({'E-1', 'E-2', 'E-3', 'E-12', 'E-15', 'E-18', 'E-19', 'E-35', 'E-36'} <= ids)
            self.assertIn('E-50' if cell == 'claude-herdr' else 'E-51', ids)
            self.assertTrue({'E-4', 'E-10', 'E-14', 'E-21', 'E-31'}.isdisjoint(ids))

    def test_all_herdr_dry_runs_never_touch_tmux_network_or_runtime(self):
        from unittest.mock import patch
        import io
        from contextlib import redirect_stdout
        class Forbidden:
            def __getattr__(self, name):
                raise AssertionError('forbidden side effect: ' + name)
        for cell in ('claude-herdr', 'codex-herdr'):
            with patch.object(driver.sys, 'argv', ['driver', '--cell', cell, '--channel-id', '41', '--base-url', 'http://unused.test', '--dry-run']):
                args = driver.parse_args()
            args.reset_before_each = True
            args.hard_reset_session_each = True
            for scenario in driver.load_scenarios(ROOT / 'tests/e2e/tui_relay/scenarios', cell=cell):
                with self.subTest(cell=cell, scenario=scenario['id']), \
                     patch.object(driver, 'tmux', Forbidden()), \
                     patch.object(driver.urllib.request, 'urlopen', side_effect=AssertionError('network')), \
                     patch.object(driver.subprocess, 'run', side_effect=AssertionError('process')), \
                     patch.object(driver, 'reset_channel_state', side_effect=AssertionError('reset')), \
                     patch.object(driver, 'hard_reset_provider_session', side_effect=AssertionError('kill')), \
                     redirect_stdout(io.StringIO()) as output:
                    result = driver.run_scenario(scenario, args=args, client=Forbidden(), run_id='dry')
                self.assertEqual(result['status'], 'dry_run')
                self.assertFalse(result['real_provider_contacted'])
                self.assertIn('planned', output.getvalue())
                self.assertFalse(any(a.get('passed') for a in result['assertions']))

    def test_restart_uses_readonly_row_nonce_without_tmux(self):
        from unittest.mock import Mock, patch
        from argparse import Namespace
        import json
        scenario = next(s for s in driver.load_scenarios(ROOT / 'tests/e2e/tui_relay/scenarios', cell='claude-herdr') if s['id'] == 'E-19')
        before = {'channel': '41', 'provider': 'claude', 'state': 'bound', 'nonce': 'same',
                  'pane': 'provider_running', 'launch_evidence': 'recorded', 'input_hold': None}
        health = {'status': 'healthy', 'herdr': {'admission': 'open', 'restart_required': False,
                  'configured_channels': ['41'], 'endpoints': {'test': {'local': True}}, 'input_holds': 0,
                  'reconnect': dict(channels=1, published=1, withheld=0, unknown=0, pending=0)}}
        args = Namespace(cell='claude-herdr', channel_id='41', base_url='http://unused.test', dry_run=False,
                         hard_reset_session_each=False, reset_before_each=False, allow_destructive=True,
                         restart_script=None, restart_target_override='dev', herdr_isolated_server=True,
                         herdr_endpoint='test', herdr_status_bin='/test/agentdesk', final_refetches=1)
        rows = []
        client = Mock()
        client.send_control.return_value = {'id': '100'}
        def wait(channel, **kwargs):
            marker = '[E2E:E19:PRE]' if not rows else '[E2E:E19:POST] E19_SECRET_ALPHA_5AF3C2'
            row = {'id': str(102 + len(rows)), 'content': marker, 'author': {'id': '42', 'bot': True}}
            rows.append(row)
            return row, list(rows)
        client.wait_for_message.side_effect = wait
        client.fetch_messages.side_effect = lambda *a, **k: list(rows)
        with patch.object(driver, 'tmux', None), patch.object(driver, '_read_api_json', return_value=(200, health)) as get, \
             patch.object(driver.herdr.subprocess, 'run', return_value=Namespace(stdout=json.dumps({'executions': [before]}))) as run, \
             patch.object(driver.herdr.time, 'sleep'), patch.dict('os.environ', {'AGENTDESK_E2E_ALLOW_DESTRUCTIVE': '1'}):
            result = driver.run_scenario(scenario, args=args, client=client, run_id='restart')
        self.assertEqual(result['status'], 'not_applicable', result)
        self.assertEqual([c.args[0][:2] for c in run.call_args_list],
                         [['/test/agentdesk', 'herdr'], ['launchctl', 'kickstart'], ['/test/agentdesk', 'herdr']])
        self.assertTrue(all(c.args[1] == '/api/health' for c in get.call_args_list))
        self.assertEqual(result['herdr_observations'][1]['row']['nonce'], 'same')

    def test_e36_executes_twelve_normal_discord_prompts_without_legacy_evidence(self):
        from unittest.mock import Mock, patch
        from argparse import Namespace
        scenario = next(s for s in driver.load_scenarios(ROOT / 'tests/e2e/tui_relay/scenarios', cell='claude-herdr') if s['id'] == 'E-36')
        args = Namespace(cell='claude-herdr', channel_id='41', base_url='http://unused.test', dry_run=False,
                         hard_reset_session_each=False, reset_before_each=False, allow_destructive=False,
                         phase_deadline_s=3540, final_refetches=1)
        rows, sent = [], []
        client = Mock()
        client.send_control.return_value = {'id': '100'}
        def add(marker):
            row = {'id': str(102 + len(rows)), 'content': marker.replace('{run_id}', 'phase'), 'author': {'id': '42', 'bot': True}}
            rows.append(row)
        def send(channel, prompt):
            step = scenario['steps'][len(sent)]
            sent.append(prompt)
            if step['request_key'] == 'QB':
                add(scenario['steps'][-2]['body_marker'])
            add(step.get('hold_marker', step['body_marker']))
            return {'id': '101'}
        client.send.side_effect = send
        client.wait_for_message.side_effect = lambda channel, **k: (next((r for r in rows if k['predicate'](r)), None), list(rows))
        client.fetch_messages.side_effect = lambda *a, **k: list(rows)
        with patch.object(driver, 'tmux', None), patch.object(driver.herdr, 'observe', return_value={'herdr': {}}), \
             patch.object(driver.normal_intake_evidence, 'validate', side_effect=AssertionError('legacy intake')):
            result = driver.run_scenario(scenario, args=args, client=client, run_id='phase')
        self.assertEqual(result['status'], 'not_applicable', result)
        self.assertEqual(client.send.call_count, 12)
        client.send_prompt.assert_not_called()
        self.assertEqual(result['e36_acceptance']['native_intake_queue_join'], 'not_applicable')
