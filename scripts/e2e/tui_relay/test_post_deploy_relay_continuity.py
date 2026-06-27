"""Unit tests for the post-deploy relay continuity smoke wrapper."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import post_deploy_relay_continuity as smoke  # noqa: E402


class ConfigResolution(unittest.TestCase):
    def test_load_channel_id_from_agentdesk_yaml_shape(self):
        yaml = """
agents:
  - id: adk-claude-tui-e2e
    channels:
      claude: {id: "222"}
  - id: adk-codex-tui-e2e
    channels:
      codex: {id: "555"}
"""
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "agentdesk.yaml"
            path.write_text(yaml, encoding="utf-8")

            self.assertEqual(
                smoke.load_channel_id_from_config(path, "claude-tui"),
                "222",
            )
            self.assertEqual(
                smoke.load_channel_id_from_config(path, "codex-tui"),
                "555",
            )

    def test_strict_self_check_requires_restart_boundary(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "agentdesk.yaml"
            config.write_text(
                """
agents:
  - id: adk-claude-tui-e2e
    channels:
      claude: {id: "222"}
""",
                encoding="utf-8",
            )
            args = smoke.parse_args(
                [
                    "--self-check",
                    "--strict-live",
                    "--config",
                    str(config),
                    "--scenarios",
                    str(ROOT / "tests" / "e2e" / "tui_relay" / "scenarios"),
                ]
            )

            report = smoke.run_self_check(args, strict_live=True)

        self.assertFalse(report["ok"])
        restart_check = next(
            check for check in report["checks"] if check["name"] == "restart_boundary"
        )
        self.assertFalse(restart_check["ok"])
        self.assertTrue(restart_check["fatal"])

    def test_filter_must_include_required_restart_scenarios(self):
        args = smoke.parse_args(["--filter", "E-9"])

        with self.assertRaises(smoke.SmokeConfigError):
            smoke.required_scenario_ids(args)

    def test_self_check_rejects_deploy_command_and_restart_script_together(self):
        args = smoke.parse_args(
            [
                "--channel-id",
                "222",
                "--deploy-command",
                "scripts/deploy-release.sh --skip-review",
                "--restart-script",
                "/tmp/restart-release.sh",
            ]
        )

        report = smoke.run_self_check(args, strict_live=True)

        self.assertFalse(report["ok"])
        exclusive = next(
            check
            for check in report["checks"]
            if check["name"] == "restart_boundary_exclusive"
        )
        self.assertFalse(exclusive["ok"])
        self.assertTrue(exclusive["fatal"])


class FixtureValidation(unittest.TestCase):
    def test_builtin_pass_fixture_has_no_violations(self):
        evidence = smoke._load_fixture("pass")  # noqa: SLF001

        self.assertEqual(smoke.validate_fixture_evidence(evidence), [])

    def test_relay_missing_fixture_distinguishes_local_output_from_discord_gap(self):
        evidence = smoke._load_fixture("relay-missing")  # noqa: SLF001

        violations = smoke.validate_fixture_evidence(evidence)

        self.assertIn(
            "local output exists after restart but Discord post-restart marker is missing",
            violations,
        )

    def test_bad_state_fixture_reports_known_relay_bad_states(self):
        evidence = smoke._load_fixture("bad-state")  # noqa: SLF001

        violations = smoke.validate_fixture_evidence(evidence)

        self.assertTrue(
            any("ownerless_inflight" in violation for violation in violations),
            violations,
        )
        self.assertTrue(
            any("relay_stall_state=tmux_alive_relay_dead" in violation for violation in violations),
            violations,
        )
        self.assertTrue(
            any("stale_thread_proof=true" in violation for violation in violations),
            violations,
        )


class DriverReportValidation(unittest.TestCase):
    def test_driver_report_requires_e9_and_e19_evidence(self):
        report = {
            "cell": "claude-tui",
            "channel_id": "222",
            "totals": {"pass": 2, "fail": 0, "skipped": 0},
            "scenarios": [
                {
                    "id": "E-9",
                    "status": "pass",
                    "relay_count": 2,
                    "post_scenario_idle": {"status": "idle"},
                },
                {
                    "id": "E-19",
                    "status": "pass",
                    "post_scenario_idle": {"status": "idle"},
                    "session_preserved": {
                        "before_restart": {"session_name": "AgentDesk-claude"}
                    },
                },
            ],
        }

        self.assertEqual(smoke.validate_driver_report(report), [])

    def test_driver_report_fails_when_e19_lacks_session_preservation(self):
        report = {
            "totals": {"pass": 2, "fail": 0, "skipped": 0},
            "scenarios": [
                {
                    "id": "E-9",
                    "status": "pass",
                    "relay_count": 2,
                    "post_scenario_idle": {"status": "idle"},
                },
                {
                    "id": "E-19",
                    "status": "pass",
                    "post_scenario_idle": {"status": "idle"},
                },
            ],
        }

        violations = smoke.validate_driver_report(report)

        self.assertIn("E-19 missing tmux session preservation evidence", violations)


if __name__ == "__main__":
    unittest.main()
