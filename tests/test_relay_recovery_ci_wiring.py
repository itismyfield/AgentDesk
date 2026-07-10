"""Contract tests for the non-PG relay-recovery execution proof."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
RELAY_RECOVERY_COMMAND = (
    "env -u AGENTDESK_ROOT_DIR cargo test --lib relay_recovery -- "
    "--skip _pg --skip pg_ --skip postgres"
)


def count_executable_relay_recovery_commands(text: str) -> int:
    executable_lines = {
        RELAY_RECOVERY_COMMAND,
        f"nice -n 10 {RELAY_RECOVERY_COMMAND}",
    }
    return sum(
        line.strip() in executable_lines
        for line in text.splitlines()
    )


class RelayRecoveryCiWiringTest(unittest.TestCase):
    def test_relay_recovery_filter_wired_into_every_targeted_non_pg_lane(self) -> None:
        expected_counts = {
            "justfile": 1,
            ".github/workflows/ci-pr.yml": 1,
            ".github/workflows/ci-macos-trusted.yml": 2,
        }
        for relative_path, expected_count in expected_counts.items():
            with self.subTest(path=relative_path):
                text = (REPO_ROOT / relative_path).read_text(encoding="utf-8")
                self.assertEqual(
                    count_executable_relay_recovery_commands(text),
                    expected_count,
                    f"{relative_path} must run the relay_recovery non-PG filter "
                    f"in all {expected_count} targeted lane(s)",
                )

    def test_commented_or_echoed_commands_do_not_count_as_executable_wiring(self) -> None:
        fixture = "\n".join(
            (
                f"# {RELAY_RECOVERY_COMMAND}",
                f"    {RELAY_RECOVERY_COMMAND}",
                f"    nice -n 10 {RELAY_RECOVERY_COMMAND}",
                f'    echo "{RELAY_RECOVERY_COMMAND}"',
            )
        )
        self.assertEqual(count_executable_relay_recovery_commands(fixture), 2)

    def test_ci_script_checks_runs_relay_recovery_wiring_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_relay_recovery_ci_wiring', script
        )


if __name__ == "__main__":
    unittest.main()
