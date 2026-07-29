"""Contract tests for destructive-cancel regression execution lanes."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
CANCEL_COMMAND = (
    "cargo test --all-targets cancel -- --skip _pg --skip pg_ --skip postgres"
)


def count_executable_cancel_commands(text: str) -> int:
    executable_lines = {
        CANCEL_COMMAND,
        f"nice -n 10 {CANCEL_COMMAND}",
    }
    return sum(line.strip() in executable_lines for line in text.splitlines())


class DestructiveCancelCiWiringTest(unittest.TestCase):
    def test_cancel_filter_wired_into_every_targeted_non_pg_lane(self) -> None:
        expected_counts = {
            "justfile": 1,
            ".github/workflows/ci-pr.yml": 0,
            ".github/workflows/ci-macos-trusted.yml": 2,
        }
        for relative_path, expected_count in expected_counts.items():
            with self.subTest(path=relative_path):
                text = (REPO_ROOT / relative_path).read_text(encoding="utf-8")
                self.assertEqual(
                    count_executable_cancel_commands(text),
                    expected_count,
                    f"{relative_path} must retain exactly {expected_count} cancel lane(s)",
                )

    def test_ci_script_checks_runs_destructive_cancel_wiring_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_destructive_cancel_ci_wiring', script
        )


if __name__ == "__main__":
    unittest.main()
