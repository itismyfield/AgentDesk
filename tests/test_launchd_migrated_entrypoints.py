from pathlib import Path
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
ROUTINE_DIR = REPO_ROOT / "routines" / "migrated-launchd"
ENTRYPOINT_DIR = REPO_ROOT / "scripts" / "launchd-migrated"
SPECIFIC_USER_HOME = "/Users/itismyfield"

MIGRATED_SHELL_JOBS = [
    "ai-integrated-briefing",
    "banchan-day-reminder-cook",
    "banchan-day-reminder-prep",
    "cookingheart-daily-briefing",
    "family-morning-briefing-obujang",
    "family-morning-briefing-yohoejang",
    "memento-daily-report",
    "memento-hygiene",
    "memory-merge",
    "token-daily-report",
]

OWNERS = {
    "memento-daily-report": "personal-obiseo",
    "memento-hygiene": "personal-obiseo",
    "memory-merge": "project-agentdesk",
}


class LaunchdMigratedEntrypointTests(unittest.TestCase):
    def test_migrated_routines_invoke_release_deployed_entrypoints(self) -> None:
        for name in MIGRATED_SHELL_JOBS:
            routine = ROUTINE_DIR / f"{name}.js"
            script = ENTRYPOINT_DIR / f"{name}.sh"

            text = routine.read_text(encoding="utf-8")

            self.assertTrue(script.exists(), f"missing repo entrypoint for {name}")
            self.assertIn(f"scripts/launchd-migrated/{name}.sh", text)
            self.assertIn("AGENTDESK_ROOT_DIR", text)
            self.assertNotIn(SPECIFIC_USER_HOME, text)

    def test_migrated_entrypoints_do_not_call_local_bin_helpers(self) -> None:
        for path in ENTRYPOINT_DIR.iterdir():
            if path.is_file():
                text = path.read_text(encoding="utf-8")
                self.assertNotIn(
                    SPECIFIC_USER_HOME,
                    text,
                    f"{path.name} still depends on an operator-specific home path",
                )

    def test_issue_2396_memory_jobs_have_concrete_owners(self) -> None:
        docs = (
            REPO_ROOT / "docs" / "launchd-to-routine-migration-plan.md"
        ).read_text(encoding="utf-8")
        self.assertNotIn("TODO agent_id", docs)

        for name, owner in OWNERS.items():
            routine = (ROUTINE_DIR / f"{name}.js").read_text(encoding="utf-8")
            self.assertIn(f"// Agent: {owner}", routine)
            self.assertIn(f'"agent_id": "{owner}"', routine)
            self.assertIn(
                f"| `com.itismyfield.{name}` | `migrated-launchd/{name}.js`",
                docs,
            )
            self.assertIn(f"| `{owner}` | cutover (stage-paused) |", docs)

    def test_token_report_helper_is_repo_deployed_with_shell_entrypoint(self) -> None:
        token_shell = (ENTRYPOINT_DIR / "token-daily-report.sh").read_text(
            encoding="utf-8"
        )

        self.assertTrue((ENTRYPOINT_DIR / "token-daily-report.py").exists())
        self.assertIn("$SCRIPT_DIR/token-daily-report.py", token_shell)

    def test_queue_stability_entrypoint_is_release_packaged(self) -> None:
        self.assertTrue((REPO_ROOT / "scripts" / "queue-stability-batch.sh").exists())
        for path in (
            REPO_ROOT / "scripts" / "build-release.sh",
            REPO_ROOT / "scripts" / "deploy-release.sh",
        ):
            text = path.read_text(encoding="utf-8")
            self.assertIn("queue-stability-batch.sh", text)
            self.assertIn("_defaults.sh", text)

    def test_output_files_use_unique_tmp_paths(self) -> None:
        helper = (ENTRYPOINT_DIR / "run-claude-message-job.sh").read_text(
            encoding="utf-8"
        )
        token_shell = (ENTRYPOINT_DIR / "token-daily-report.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("mktemp", helper)
        self.assertIn("mktemp", token_shell)
        self.assertNotIn("/tmp/claude-job-output-${SOURCE//[:\\/]/-}.txt", helper)
        self.assertNotIn("/tmp/token-daily-report-output.txt", token_shell)


if __name__ == "__main__":
    unittest.main()
