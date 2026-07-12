import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FOCUSED_COMMAND = "cargo test --lib discord_thread_create -- --test-threads=1"


class DiscordThreadCreateCiWiringTests(unittest.TestCase):
    def test_required_ubuntu_recipe_runs_focused_suite_once(self) -> None:
        justfile = (ROOT / "justfile").read_text(encoding="utf-8")
        self.assertEqual(justfile.count(FOCUSED_COMMAND), 1)
        self.assertNotIn("cargo test --lib cli::discord_thread_create", justfile)

    def test_windows_job_has_non_advisory_runtime_step(self) -> None:
        workflow = (ROOT / ".github/workflows/ci-pr.yml").read_text(encoding="utf-8")
        marker = "      - name: Discord thread-create cross-process lock\n"
        start = workflow.index(marker)
        end = workflow.index("\n      - name:", start + len(marker))
        step = workflow[start:end]
        self.assertIn(f"run: {FOCUSED_COMMAND}", step)
        self.assertNotIn("--lib cli::discord_thread_create", step)
        self.assertNotIn("continue-on-error", step)
        self.assertIn("BASH_ENV: /dev/null", step)
        self.assertIn('CARGO_PROFILE_DEV_DEBUG: "0"', step)
        self.assertIn('CARGO_PROFILE_TEST_DEBUG: "0"', step)

    def test_whole_job_guard_registers_the_runtime_step(self) -> None:
        guard = (ROOT / "scripts/check-ci-runner-hardening.sh").read_text(encoding="utf-8")
        self.assertIn('"Discord thread-create cross-process lock" => {', guard)
        self.assertIn(f'"commands" => ["{FOCUSED_COMMAND}"]', guard)


if __name__ == "__main__":
    unittest.main()
