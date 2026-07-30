"""Contract tests for stall-recovery regression execution lanes."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
STALL_RECOVERY_COMMAND = (
    "cargo test --all-targets stall_recovery -- --skip _pg --skip pg_ --skip postgres"
)
UBUNTU_STALL_RECOVERY_COMMAND = (
    "cargo test --lib services::discord::inflight::stall_recovery_tests -- --test-threads=1"
)


def count_executable_stall_recovery_commands(text: str) -> int:
    executable_lines = {
        STALL_RECOVERY_COMMAND,
        f"nice -n 10 {STALL_RECOVERY_COMMAND}",
    }
    return sum(line.strip() in executable_lines for line in text.splitlines())


class StallRecoveryCiWiringTest(unittest.TestCase):
    def test_stall_recovery_filter_wired_into_every_targeted_non_pg_lane(self) -> None:
        expected_counts = {
            "justfile": 1,
            ".github/workflows/ci-macos-trusted.yml": 2,
        }
        for relative_path, expected_count in expected_counts.items():
            with self.subTest(path=relative_path):
                text = (REPO_ROOT / relative_path).read_text(encoding="utf-8")
                self.assertEqual(
                    count_executable_stall_recovery_commands(text),
                    expected_count,
                    f"{relative_path} must retain exactly {expected_count} stall-recovery lane(s)",
                )

    def test_required_ubuntu_lane_executes_inflight_stall_recovery_module(self) -> None:
        workflow = (REPO_ROOT / ".github/workflows/ci-pr.yml").read_text(
            encoding="utf-8"
        )
        self.assertEqual(
            sum(
                line.strip() == UBUNTU_STALL_RECOVERY_COMMAND
                for line in workflow.splitlines()
            ),
            1,
            "required Ubuntu recovery lane must execute the inflight stall-recovery module",
        )

    def test_ci_script_checks_runs_stall_recovery_wiring_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_stall_recovery_ci_wiring', script
        )


if __name__ == "__main__":
    unittest.main()
