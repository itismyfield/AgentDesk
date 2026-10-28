import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class DiscordThreadCreateCiWiringTests(unittest.TestCase):
    def test_pr_windows_job_is_compile_only(self) -> None:
        workflow = (ROOT / ".github/workflows/ci-pr.yml").read_text(encoding="utf-8")
        self.assertNotIn("Discord thread-create cross-process lock", workflow)

    def test_nightly_windows_job_has_non_advisory_runtime_step(self) -> None:
        workflow = (ROOT / ".github/workflows/ci-nightly.yml").read_text(
            encoding="utf-8"
        )
        # The native run plans every non-PG Windows lib test, the thread-create lock tests included.
        job = workflow[workflow.index("  full_windows:\n") :]
        job = job[: job.index("\n  rust_build:\n")]
        self.assertNotIn("Discord thread-create cross-process lock", job)
        self.assertIn('CARGO_PROFILE_DEV_DEBUG: "0"', job.split("    steps:\n", 1)[0])
        self.assertIn('CARGO_PROFILE_TEST_DEBUG: "0"', job.split("    steps:\n", 1)[0])
        for name, phase in (("Build and plan non-PG lib tests", "prepare"),
                            ("Run and audit non-PG lib tests", "run")):
            with self.subTest(phase=phase):
                marker = f"      - name: {name}\n"
                start = job.index(marker)
                end = job.index("\n      - name:", start + len(marker))
                step = job[start:end]
                self.assertIn(f"run: bash scripts/ci/nextest-native.sh windows windows-lib pg-classifier {phase}\n", step)
                self.assertNotIn("continue-on-error", step)
                self.assertIn("BASH_ENV: /dev/null", step)

    def test_whole_job_guard_tracks_compile_only_pr_lane(self) -> None:
        guard = (ROOT / "scripts/check-ci-runner-hardening.sh").read_text(encoding="utf-8")
        self.assertNotIn('"Discord thread-create cross-process lock" => {', guard)
        self.assertNotIn('"cargo test (non-PG, targeted subset)" => {', guard)


if __name__ == "__main__":
    unittest.main()
