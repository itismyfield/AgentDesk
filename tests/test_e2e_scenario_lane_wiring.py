"""Contract tests for the #5997 e2e scenario lane wiring.

The expected Python module set is derived from the directory, not listed here,
so a new `scripts/e2e/tui_relay/test_*.py` that nobody wires fails this gate
instead of compiling in review and executing nowhere.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
CI_SCRIPT_CHECKS = REPO_ROOT / "scripts/ci-script-checks.sh"
CI_PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
TUI_RELAY_TESTS = REPO_ROOT / "scripts/e2e/tui_relay"
UNCONDITIONAL_JOB = "relay-authority-contract"
CENSUS_TARGET = "services::discord::tui_prompt_relay::tests::scenario_census_e2e"


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


def job_block(text: str, job: str) -> str:
    lines = text.splitlines()
    header = re.compile(r"^  [A-Za-z0-9_-]+:$")
    start = None
    for index, line in enumerate(lines):
        if line == f"  {job}:":
            start = index
            break
    if start is None:
        raise AssertionError(f"{job} job not found")
    end = len(lines)
    for index in range(start + 1, len(lines)):
        if header.match(lines[index]):
            end = index
            break
    return "\n".join(lines[start:end])


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

    def test_scenario_census_runner_is_wired_into_the_unconditional_job(self) -> None:
        block = job_block(self.workflow, UNCONDITIONAL_JOB)
        self.assertIn(CENSUS_TARGET, block, f"{CENSUS_TARGET} must run in {UNCONDITIONAL_JOB}")
        self.assertEqual(
            self.workflow.count(CENSUS_TARGET),
            1,
            "the census runner must have exactly one call site in ci-pr.yml",
        )

    def test_the_census_job_carries_no_path_filter(self) -> None:
        block = job_block(self.workflow, UNCONDITIONAL_JOB)
        for line in block.splitlines():
            self.assertFalse(
                line.startswith("    if:") or line.startswith("    needs:"),
                f"{UNCONDITIONAL_JOB} must stay unconditional; found {line.strip()!r}",
            )

    def test_commented_wiring_does_not_count(self) -> None:
        fixture = '# "$PYTHON" -m unittest scripts.e2e.tui_relay.test_fixtures'
        self.assertNotIn("test_fixtures", "\n".join(executable_lines(fixture)))


if __name__ == "__main__":
    unittest.main()
