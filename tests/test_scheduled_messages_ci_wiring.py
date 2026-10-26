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
        self.assertIn(SCHEDULER_SUBMODULE_GLOB, filter_block(workflow, "high_risk_recovery", "rust_or_policy"),
                      "high_risk_recovery must cover scheduled_messages submodules")
        # The PostgreSQL tests run in the single Rust test run that every src/ change selects.
        self.assertIn("- 'src/**'", filter_block(workflow, "rust_or_policy", "relay_contract"))

    def test_route_submodules_trigger_high_risk_lane(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertIn("- 'src/server/routes/scheduled_messages/**'",
                      filter_block(workflow, "high_risk_recovery", "rust_or_policy"))

    def test_image_outbox_children_trigger_high_risk_lane(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        block = filter_block(workflow, "high_risk_recovery", "rust_or_policy")
        for path in ("src/services/message_outbox/**", "src/server/outbox_worker.rs", "src/server/outbox_worker/**"):
            with self.subTest(path=path):
                self.assertIn(f"- '{path}'", block)


if __name__ == "__main__":
    unittest.main()
