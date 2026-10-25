"""Contract tests for the e2e scenario lane wiring.

The expected Python module set is derived from the directory, not listed here,
so a new `scripts/e2e/tui_relay/test_*.py` that nobody wires fails this gate
instead of compiling in review and executing nowhere.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
CI_SCRIPT_CHECKS = REPO_ROOT / "scripts/ci-script-checks.sh"
CI_PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
TUI_RELAY_TESTS = REPO_ROOT / "scripts/e2e/tui_relay"
CENSUS_TARGET = "services::discord::tui_prompt_relay::tests::scenario_census_e2e"
RELAY_MANIFEST = REPO_ROOT / "scripts/relay_authority_contract_targets.json"
# The census runs in the single nextest run; the build job proves its floor on that run's list.
FLOOR_COMMAND = 'python3 scripts/check_relay_authority_contract.py --nextest-list "$NEXTEST_DIR/universe.json"'


def executable_lines(text: str) -> list[str]:
    lines = []
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("#") or line.startswith("echo "):
            continue
        lines.append(line)
    return lines


def discovered_test_modules() -> set[str]:
    return {
        f"scripts.e2e.tui_relay.{path.stem}"
        for path in TUI_RELAY_TESTS.glob("test_*.py")
    }


class E2eScenarioLaneWiring(unittest.TestCase):
    def setUp(self) -> None:
        self.script = CI_SCRIPT_CHECKS.read_text(encoding="utf-8")
        self.workflow = CI_PR_WORKFLOW.read_text(encoding="utf-8")

    def test_every_tui_relay_test_module_is_wired_into_ci_script_checks(self) -> None:
        modules = discovered_test_modules()
        self.assertGreaterEqual(len(modules), 9, "scenario test modules disappeared")
        body = "\n".join(executable_lines(self.script))
        for module in sorted(modules):
            with self.subTest(module=module):
                self.assertIn(module, body, f"{module} is not executed by ci-script-checks.sh")

    def test_scenario_census_is_a_declared_relay_floor_checked_on_the_run(self) -> None:
        import json
        lanes = [lane for lane in json.loads(RELAY_MANIFEST.read_text(encoding="utf-8"))["lanes"]
                 if lane.get("status") == "active" and CENSUS_TARGET in lane.get("command", [])]
        self.assertEqual(len(lanes), 1, f"{CENSUS_TARGET} must be exactly one active relay lane")
        self.assertGreaterEqual(lanes[0]["minimum"], 1)
        jobs = yaml.safe_load(self.workflow)["jobs"]
        floor_steps = [step for step in jobs["rust_build"]["steps"] if FLOOR_COMMAND in str(step.get("run", "")).splitlines()]
        self.assertEqual(len(floor_steps), 1)
        self.assertNotIn("if", floor_steps[0])
        self.assertNotIn("continue-on-error", floor_steps[0])

    def test_census_run_remains_required_by_the_always_judge(self) -> None:
        jobs = yaml.safe_load(self.workflow)["jobs"]
        judge = jobs["required_contexts"]
        self.assertEqual(judge["if"], "${{ !cancelled() }}")
        self.assertLessEqual({"rust_build", "rust_tests", "test_evidence"}, set(judge["needs"]))
        sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
        import nextest_required
        sys.path.pop(0)
        self.assertIn("relay-authority-contract", nextest_required.matrix_branches(judge["strategy"]["matrix"]["context"])[1])

    def test_commented_wiring_does_not_count(self) -> None:
        fixture = '# "$PYTHON" -m unittest scripts.e2e.tui_relay.test_fixtures'
        self.assertNotIn("test_fixtures", "\n".join(executable_lines(fixture)))


if __name__ == "__main__":
    unittest.main()
