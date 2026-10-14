"""Contract tests for the e2e scenario lane wiring.

The expected Python module set is derived from the directory, not listed here,
so a new `scripts/e2e/tui_relay/test_*.py` that nobody wires fails this gate
instead of compiling in review and executing nowhere.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
CI_SCRIPT_CHECKS = REPO_ROOT / "scripts/ci-script-checks.sh"
CI_PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
TUI_RELAY_TESTS = REPO_ROOT / "scripts/e2e/tui_relay"
UNCONDITIONAL_JOB = "relay_authority_targets"
CENSUS_TARGET = "services::discord::tui_prompt_relay::tests::scenario_census_e2e"
# The census may wait on the shared lib producer only behind this exact cancellation guard.
PRODUCER_NEEDS_LINE = "    needs: lib_test_prepare"
CANCELLATION_GUARD_LINE = "    if: ${{ !cancelled() }}"


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


def activation_lines(block: str) -> list[str]:
    return [line for line in block.splitlines() if line.startswith(("    if:", "    needs:"))]


def census_job_stays_unconditional(block: str) -> bool:
    # Absent, or the exact guarded producer edge (as test_pr_cap_ci_wiring permits);
    # needs without the guard would skip the census whenever the producer fails.
    return sorted(activation_lines(block)) in (
        [],
        [CANCELLATION_GUARD_LINE],
        [CANCELLATION_GUARD_LINE, PRODUCER_NEEDS_LINE],
    )


def with_activation(block: str, *lines: str) -> str:
    body = [line for line in block.splitlines() if line not in activation_lines(block)]
    return "\n".join(body[:1] + list(lines) + body[1:])


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
        self.assertTrue(
            census_job_stays_unconditional(block),
            f"{UNCONDITIONAL_JOB} must stay unconditional; found {activation_lines(block)!r}",
        )

    def test_the_original_unconditional_census_job_is_accepted(self) -> None:
        block = job_block(self.workflow, UNCONDITIONAL_JOB)
        self.assertTrue(census_job_stays_unconditional(with_activation(block)))

    def test_other_census_job_activation_fails(self) -> None:
        block = job_block(self.workflow, UNCONDITIONAL_JOB)
        for lines in (
            ("    if: false",),
            ("    if: ${{ false }}",),
            ("    if: needs.changes.outputs.relay == 'true'",),
            (PRODUCER_NEEDS_LINE, "    if: ${{ needs.changes.outputs.relay == 'true' }}"),
            (PRODUCER_NEEDS_LINE,
             "    if: ${{ !cancelled() && needs.changes.outputs.relay == 'true' }}"),
            (PRODUCER_NEEDS_LINE, "    if: ${{ always() }}"),
            (PRODUCER_NEEDS_LINE, "    if: ${{!cancelled()}}"),
            (PRODUCER_NEEDS_LINE,),
            ("    needs: changes", CANCELLATION_GUARD_LINE),
            ("    needs: [lib_test_prepare, changes]", CANCELLATION_GUARD_LINE),
            ("    needs: [lib_test_prepare]", CANCELLATION_GUARD_LINE),
            ("    needs:", CANCELLATION_GUARD_LINE),
            (PRODUCER_NEEDS_LINE, CANCELLATION_GUARD_LINE, "    if: false"),
        ):
            with self.subTest(lines=lines):
                self.assertFalse(census_job_stays_unconditional(with_activation(block, *lines)))

    def test_census_target_remains_required_by_the_always_publisher(self) -> None:
        jobs = yaml.safe_load(self.workflow)["jobs"]
        publisher = jobs["relay-authority-contract"]
        self.assertEqual(publisher["if"], "${{ !cancelled() }}")
        self.assertEqual(publisher["needs"], [UNCONDITIONAL_JOB, "relay_authority_mutations"])
        census_steps = [step for step in jobs[UNCONDITIONAL_JOB]["steps"]
                        if CENSUS_TARGET in step.get("run", "")]
        self.assertEqual(len(census_steps), 1)
        self.assertNotIn("if", census_steps[0])
        self.assertNotIn("continue-on-error", census_steps[0])

    def test_commented_wiring_does_not_count(self) -> None:
        fixture = '# "$PYTHON" -m unittest scripts.e2e.tui_relay.test_fixtures'
        self.assertNotIn("test_fixtures", "\n".join(executable_lines(fixture)))


if __name__ == "__main__":
    unittest.main()
