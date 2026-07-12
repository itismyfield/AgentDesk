"""Contract tests for scheduled-message PostgreSQL CI path coverage."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci-pr.yml"
SCHEDULER_SUBMODULE_GLOB = "- 'src/services/scheduled_messages/**'"


def filter_block(workflow: str, name: str, next_name: str) -> str:
    start_marker = f"            {name}:\n"
    end_marker = f"            {next_name}:\n"
    start = workflow.index(start_marker) + len(start_marker)
    end = workflow.index(end_marker, start)
    return workflow[start:end]


class ScheduledMessagesCiWiringTest(unittest.TestCase):
    def test_scheduler_submodules_trigger_pg_and_high_risk_lanes(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        for filter_name, next_filter in (
            ("high_risk_recovery", "pg_db"),
            ("pg_db", "rust_or_policy"),
        ):
            with self.subTest(filter=filter_name):
                block = filter_block(workflow, filter_name, next_filter)
                self.assertIn(
                    SCHEDULER_SUBMODULE_GLOB,
                    block,
                    f"{filter_name} must cover scheduled_messages submodules",
                )

    def test_ci_script_checks_runs_scheduler_filter_contract(self) -> None:
        script = (REPO_ROOT / "scripts" / "ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_scheduled_messages_ci_wiring', script
        )


if __name__ == "__main__":
    unittest.main()
