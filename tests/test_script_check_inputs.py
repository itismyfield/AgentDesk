"""Exercise the PR skip selector through its real CLI and fixture metadata."""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import yaml

from scripts.ci import script_check_inputs as selector


ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts/ci/script_check_inputs.py"


class ScriptCheckInputSelection(unittest.TestCase):
    def env(self, **extra: str) -> dict[str, str]:
        env = {key: value for key, value in os.environ.items()
               if not key.startswith("SCRIPT_CHECK_")
               and key not in {"GITHUB_EVENT_NAME", "GFP_EVENT_NAME"}}
        env.update(GITHUB_EVENT_NAME="pull_request", SCRIPT_CHECK_CHANGES_RESULT="success",
                   SCRIPT_CHECK_CHANGED_FILES=json.dumps(["src/unrelated.rs"]))
        env.update(extra)
        return env

    def cli(self, **extra: str) -> dict[str, tuple[str, ...]]:
        done = subprocess.run([sys.executable, str(HELPER)], cwd=ROOT,
                              env=self.env(**extra), capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(done.stderr, "")
        return {title: tuple(inputs.split())
                for title, inputs in (line.split("\t", 1) for line in done.stdout.splitlines())}

    def test_unrelated_pr_reports_every_self_test_with_its_inputs(self) -> None:
        skipped = self.cli()
        self.assertTrue(skipped)
        self.assertEqual(set(skipped), set(selector.SELF_TEST_INPUTS))
        for title, inputs in selector.SELF_TEST_INPUTS.items():
            self.assertEqual(skipped[title], selector.COMMON_INPUTS + inputs)

    def test_self_test_inventory_is_nonempty_and_independent_of_pr_selection(self) -> None:
        done = subprocess.run([sys.executable, str(HELPER), "--list-self-tests"], cwd=ROOT,
                              env=self.env(GITHUB_EVENT_NAME="schedule"),
                              capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(done.stdout.strip())
        self.assertEqual(done.stdout.splitlines(), list(selector.SELF_TEST_INPUTS))

    def test_every_bundles_own_input_change_selects_it(self) -> None:
        for title, inputs in selector.SELF_TEST_INPUTS.items():
            with self.subTest(title=title):
                changed = inputs[0].replace("**", "nested/changed.py")
                self.assertNotIn(title, self.cli(SCRIPT_CHECK_CHANGED_FILES=json.dumps([changed])))

    def test_common_input_changes_select_every_bundle(self) -> None:
        for path in ("scripts/ci-script-checks.sh", "scripts/ci/script_check_inputs.py",
                     "tests/test_script_check_inputs.py", ".github/workflows/nested/ci.yml"):
            with self.subTest(path=path):
                self.assertEqual(self.cli(SCRIPT_CHECK_CHANGED_FILES=json.dumps([path])), {})

    def test_live_source_fixture_changes_select_their_bundles(self) -> None:
        cases = {
            "src/services/discord/tui_prompt_relay/tests/scenario_census_e2e.rs":
                "Comment-only change checker self-tests",
            "src/services/discord/tui_prompt_relay/injected_prompt_policy.rs":
                "TUI relay e2e harness unit tests (#5065/#5997)",
            "src/services/discord/commands/control/native.rs":
                "TUI relay e2e harness unit tests (#5065/#5997)",
            ".config/nextest.toml":
                "PR infrastructure failure rerun classifier (#4392/#5207)",
            ".github/actions/ensure-swap/nested/helper.sh":
                "Lib test build and clippy swap + memory probe contract",
        }
        for path, title in cases.items():
            with self.subTest(path=path):
                self.assertNotIn(title, self.cli(SCRIPT_CHECK_CHANGED_FILES=json.dumps([path])))

    def test_added_deleted_and_renamed_paths_need_no_file_to_exist(self) -> None:
        fixture = {"alpha": ("tools/alpha/**",), "beta": ("tools/beta/**",)}
        with patch.object(selector, "SELF_TEST_INPUTS", fixture):
            for paths in (["tools/alpha/new/deep.sh"], ["tools/alpha/deleted.sh"],
                          ["tools/alpha/old.sh", "renamed/new.sh"]):
                with self.subTest(paths=paths):
                    skipped = selector.skipped_checks(self.env(SCRIPT_CHECK_CHANGED_FILES=json.dumps(paths)))
                    self.assertEqual(set(skipped), {"beta"})

    def test_main_nightly_dispatch_unknown_and_missing_events_run_every_bundle(self) -> None:
        for event in ("push", "schedule", "workflow_dispatch", "pull_request_target", "", "unknown"):
            with self.subTest(event=event):
                self.assertEqual(self.cli(GITHUB_EVENT_NAME=event), {})

    def test_unavailable_changes_results_run_every_bundle(self) -> None:
        for result in ("", "failure", "skipped", "cancelled", "unknown", "Success"):
            with self.subTest(result=result):
                self.assertEqual(self.cli(SCRIPT_CHECK_CHANGES_RESULT=result), {})

    def test_missing_empty_or_malformed_changed_files_run_every_bundle(self) -> None:
        for raw in ("", "not-json", "null", "{}", '"src/a.rs"', "[]", "[1]", "[true]",
                    '[""]', '["/src/a.rs"]', '["../src/a.rs"]', '["src//a.rs"]',
                    '["src/./a.rs"]', '["src/a\\nb.rs"]', '["src\\\\a.rs"]',
                    '["src/unrelated.rs", null]'):
            with self.subTest(raw=raw):
                self.assertEqual(self.cli(SCRIPT_CHECK_CHANGED_FILES=raw), {})

    def test_event_fallback_and_github_event_precedence(self) -> None:
        self.assertEqual(set(self.cli(GITHUB_EVENT_NAME="", GFP_EVENT_NAME="pull_request")),
                         set(selector.SELF_TEST_INPUTS))
        self.assertEqual(self.cli(GITHUB_EVENT_NAME="push", GFP_EVENT_NAME="pull_request"), {})

    def test_every_self_test_is_registered_with_a_real_runner_shard(self) -> None:
        done = subprocess.run(["/bin/bash", str(ROOT / "scripts/ci-script-checks.sh")],
                              cwd=ROOT, env=self.env(PYTHON=sys.executable, SCRIPT_CHECK_LIST="1"),
                              capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        titles = [line.split("\t", 1)[1] for line in done.stdout.splitlines()]
        self.assertEqual(set(selector.SELF_TEST_INPUTS) - set(titles), set())
        self.assertTrue(set(titles) - set(selector.SELF_TEST_INPUTS))

    def test_pr_workflow_passes_all_changed_paths_and_the_changes_outcome(self) -> None:
        from tests.test_high_risk_recovery_path_filter import select

        jobs = yaml.safe_load((ROOT / ".github/workflows/ci-pr.yml").read_text())["jobs"]
        changes = jobs["changes"]
        step = next(step for step in changes["steps"] if step.get("id") == "filter")
        self.assertEqual(step["with"]["list-files"], "json")
        filters = yaml.safe_load(step["with"]["filters"])
        for path in ("README.md", ".gitignore", "src/new/mod.rs", ".new/nested/deep/file",
                     "new-top-level/a", "new dir/file with spaces.sh"):
            with self.subTest(path=path):
                self.assertIn("script_check_files", select(filters, [path]))
        self.assertEqual(changes["outputs"]["script_check_files"],
                         "${{ steps.filter.outputs.script_check_files_files }}")
        for job_id in ("scripts", "scripts_guards", "scripts_contracts"):
            job = jobs[job_id]
            self.assertEqual(job["needs"], "changes")
            self.assertEqual(job["if"], "${{ !cancelled() }}")
            run = next(step for step in job["steps"] if step.get("name") == "Run script checks")
            self.assertNotIn("if", run)
            self.assertEqual(run["env"]["SCRIPT_CHECK_CHANGES_RESULT"], "${{ needs.changes.result }}")
            self.assertEqual(run["env"]["SCRIPT_CHECK_CHANGED_FILES"],
                             "${{ needs.changes.outputs.script_check_files }}")
            self.assertNotIn("SCRIPT_CHECK_SELF_TESTS_ONLY", run["env"])

    def test_main_and_nightly_call_every_self_test_without_pr_path_inputs(self) -> None:
        for name in ("ci-main.yml", "ci-nightly.yml"):
            jobs = yaml.safe_load((ROOT / ".github/workflows" / name).read_text())["jobs"]
            runners = [(job, step) for job in jobs.values() for step in job.get("steps", [])
                       if step.get("run") == "./scripts/ci-script-checks.sh"]
            self.assertTrue(runners)
            for job, step in runners:
                self.assertNotIn("if", job)
                self.assertNotIn("needs", job)
                self.assertNotIn("if", step)
                env = step["env"]
                self.assertEqual(env["GFP_EVENT_NAME"], "${{ github.event_name }}")
                self.assertNotIn("SCRIPT_CHECK_CHANGES_RESULT", env)
                self.assertNotIn("SCRIPT_CHECK_CHANGED_FILES", env)
            if name == "ci-nightly.yml":
                self.assertEqual(len(runners), 1)
                self.assertEqual(runners[0][1]["env"]["SCRIPT_CHECK_SELF_TESTS_ONLY"], "1")

    def fixture_runner(self, *, remove_helper: bool = False, child_probe: bool = False,
                       **extra: str) -> subprocess.CompletedProcess[str]:
        aggregate = (ROOT / "scripts/ci-script-checks.sh").read_text()
        marker = 'if run_check guards "shellcheck scripts"; then'
        self.assertIn(marker, aggregate)
        prefix = aggregate.split(marker, 1)[0]
        bodies = []
        for title in ("fixture current source", *selector.SELF_TEST_INPUTS):
            child = ('/bin/bash -c \'test -z "${SCRIPT_CHECK_SELF_TESTS_ONLY:-}"\'\n'
                     if child_probe else "")
            bodies.append(f'if run_check guards "{title}"; then\n'
                          f'{child}printf "%s\\n" {shlex.quote("executed: " + title)}\nfi\n')
        with tempfile.TemporaryDirectory(prefix="script-check-selector-runner-") as directory:
            root = Path(directory)
            (root / "scripts/ci").mkdir(parents=True)
            helper = root / "scripts/ci/script_check_inputs.py"
            if not remove_helper:
                helper.write_bytes(HELPER.read_bytes())
            runner = root / "scripts/ci-script-checks.sh"
            runner.write_text(prefix + "\n".join(bodies))
            return subprocess.run(["/bin/bash", str(runner)], cwd=root,
                                  env=self.env(PYTHON=sys.executable, SCRIPT_CHECK_SHARD="guards", **extra),
                                  capture_output=True, text=True, timeout=30)

    def executed(self, done: subprocess.CompletedProcess[str]) -> set[str]:
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        return {line.removeprefix("executed: ") for line in done.stdout.splitlines()
                if line.startswith("executed: ")}

    def test_runner_keeps_source_checks_and_logs_every_unrelated_skip(self) -> None:
        done = self.fixture_runner()
        self.assertEqual(self.executed(done), {"fixture current source"})
        self.assertEqual(done.stdout.count("skipped: 입력 변경 없음 ("), len(selector.SELF_TEST_INPUTS))

    def test_runner_changed_fixture_input_executes_only_its_self_test_bundle(self) -> None:
        done = self.fixture_runner(SCRIPT_CHECK_CHANGED_FILES=json.dumps([
            "src/services/discord/tui_prompt_relay/injected_prompt_policy.rs",
        ]))
        self.assertEqual(self.executed(done), {
            "fixture current source", "TUI relay e2e harness unit tests (#5065/#5997)",
        })
        self.assertEqual(done.stdout.count("skipped: 입력 변경 없음 ("), len(selector.SELF_TEST_INPUTS) - 1)

    def test_runner_main_nightly_dispatch_and_unknown_changes_execute_every_bundle(self) -> None:
        cases = ({"GITHUB_EVENT_NAME": event} for event in ("push", "schedule", "workflow_dispatch"))
        for case in (*cases, {"SCRIPT_CHECK_CHANGES_RESULT": "skipped"},
                     {"SCRIPT_CHECK_CHANGED_FILES": "[]"}, {"SCRIPT_CHECK_CHANGED_FILES": "malformed"}):
            with self.subTest(case=case):
                done = self.fixture_runner(**case)
                self.assertEqual(self.executed(done), {"fixture current source", *selector.SELF_TEST_INPUTS})
                self.assertNotIn("skipped: 입력 변경 없음", done.stdout)

    def test_runner_helper_failure_executes_every_bundle(self) -> None:
        done = self.fixture_runner(remove_helper=True)
        self.assertEqual(self.executed(done), {"fixture current source", *selector.SELF_TEST_INPUTS})
        self.assertIn("Script check inputs unavailable; running every check", done.stdout)

    def test_runner_nightly_self_tests_only_executes_all_registered_bundles(self) -> None:
        done = self.fixture_runner(child_probe=True, GITHUB_EVENT_NAME="schedule", SCRIPT_CHECK_SELF_TESTS_ONLY="1")
        self.assertEqual(self.executed(done), set(selector.SELF_TEST_INPUTS))
        self.assertIn("skipped: current-source check in self-test-only run (fixture current source)", done.stdout)
        self.assertNotIn("skipped: 입력 변경 없음", done.stdout)

    def test_runner_list_mode_preserves_inventory_without_selection_helper(self) -> None:
        done = self.fixture_runner(remove_helper=True, SCRIPT_CHECK_LIST="1")
        self.assertEqual(self.executed(done), set())
        self.assertEqual(done.stderr, "")
        self.assertEqual(done.stdout.splitlines(),
                         [f"guards\t{title}" for title in ("fixture current source", *selector.SELF_TEST_INPUTS)])


if __name__ == "__main__":
    unittest.main()
