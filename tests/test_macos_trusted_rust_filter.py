"""Contract for the CI macOS Trusted push path filter."""

from __future__ import annotations

import importlib.util
import os
import resource
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/ci/macos-trusted-rust-filter.py"
WORKFLOW = ROOT / ".github/workflows/ci-macos-trusted.yml"

spec = importlib.util.spec_from_file_location("macos_trusted_rust_filter", SCRIPT)
assert spec is not None and spec.loader is not None
flt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(flt)

# One path per pattern that no other pattern matches, so dropping any
# pattern from RUST_INPUTS flips its sample to "skip".
PATTERN_SAMPLES = {
    "**/*.rs": "src/services/discord/mod.rs",
    "**/Cargo.toml": "Cargo.toml",
    "**/Cargo.lock": "Cargo.lock",
    "rust-toolchain*": "rust-toolchain.toml",
    "**/.gitattributes": ".gitattributes",
    ".cargo/**": ".cargo/config.toml",
    "sqlx-data.json": "sqlx-data.json",
    ".sqlx/**": ".sqlx/query-abc.json",
    "migrations/**": "migrations/postgres/0101_canonical_discord_session_identity.sql",
    "defaults.json": "defaults.json",
    "policies/**": "policies/default-pipeline.yaml",
    "tests/fixtures/**": "tests/fixtures/auto-queue-preflight/basic.json",
    "tests/e2e/**": "tests/e2e/tui_relay/scenarios/E-1-single-prompt.yaml",
    "routines/**": "routines/sample/routine.js",
    "docs/relay-state-contract.md": "docs/relay-state-contract.md",
    "scripts/relay_authority_rollout_report.py": "scripts/relay_authority_rollout_report.py",
    "scripts/ci-timeout.py": "scripts/ci-timeout.py",
    "scripts/ci-macos-fresh-user-smoke.sh": "scripts/ci-macos-fresh-user-smoke.sh",
    "scripts/operator-init-portable.py": "scripts/operator-init-portable.py",
    ".github/workflows/ci-macos-trusted.yml": ".github/workflows/ci-macos-trusted.yml",
    "scripts/ci/macos-trusted-rust-filter.py": "scripts/ci/macos-trusted-rust-filter.py",
}

# PR #6109: scripts, workflows and justfile only, zero Rust.
PR_6109_PATHS = [
    ".github/workflows/ci-nightly.yml",
    ".github/workflows/ci-pr.yml",
    "justfile",
    "scripts/check-ci-runner-hardening.sh",
    "scripts/check_pg_test_lane_membership.py",
    "scripts/check_test_lane_coverage.py",
    "scripts/ci/non-pg-test-filter.sh",
    "scripts/pg_test_lane_baseline.txt",
    "scripts/test_lane_coverage_baseline.txt",
    "tests/test_check_pg_test_lane_membership.py",
    "tests/test_fast_check_ci_wiring.py",
    "tests/test_nightly_repair.py",
    "tests/test_non_pg_test_filter.py",
    "tests/test_pg_boundary_diagnostics.py",
    "tests/test_test_lane_coverage.py",
]


class PatternTests(unittest.TestCase):
    def test_every_pattern_has_a_sample(self) -> None:
        self.assertEqual(set(PATTERN_SAMPLES), set(flt.RUST_INPUTS))

    def test_each_sample_runs(self) -> None:
        for pattern, path in PATTERN_SAMPLES.items():
            with self.subTest(pattern=pattern):
                self.assertTrue(flt.is_rust_input(path), path)

    def test_nested_rust_files_run(self) -> None:
        for path in (
            "build.rs",
            "tests/fixtures/crate/src/lib.rs",
            "tools/x/Cargo.toml",
            "src/.gitattributes",
        ):
            with self.subTest(path=path):
                self.assertTrue(flt.is_rust_input(path))

    def test_non_rust_paths_skip(self) -> None:
        for path in (
            *PR_6109_PATHS,
            "README.md",
            "docs/ci/sccache-setup.md",
            "dashboard/src/App.tsx",
            "package.json",
            "skills/foo/SKILL.md",
            "src.rs.bak",
            "migrations.md",
        ):
            with self.subTest(path=path):
                self.assertFalse(flt.is_rust_input(path))

    def test_decide(self) -> None:
        self.assertEqual(flt.decide(PR_6109_PATHS)[0], False)
        self.assertEqual(flt.decide(PR_6109_PATHS + ["src/lib.rs"])[0], True)
        self.assertEqual(flt.decide([])[0], True)
        self.assertEqual(flt.decide([""])[0], True)


class CliTests(unittest.TestCase):
    def run_cli(self, *args: str, stdin: str = "", cwd: Path | None = None) -> str:
        result = subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            input=stdin.encode(),
            cwd=cwd,
            capture_output=True,
            check=True,
        )
        return result.stdout.decode()

    def test_non_push_events_always_run(self) -> None:
        for event in ("merge_group", "workflow_dispatch"):
            with self.subTest(event=event):
                out = self.run_cli("--event", event, "--paths-from-stdin0", stdin="README.md\0")
                self.assertEqual(out, "run=true\n")

    def test_stdin_paths(self) -> None:
        self.assertEqual(
            self.run_cli("--event", "push", "--paths-from-stdin0", stdin="\0".join(PR_6109_PATHS)),
            "run=false\n",
        )

    def test_stdin_takes_nul_separated_git_paths(self) -> None:
        stdin = "docs/a.md\0src/한글.rs\0migrations/a\nb.sql\0"
        self.assertEqual(self.run_cli("--event", "push", "--paths-from-stdin0", stdin=stdin), "run=true\n")
        stdin = "docs/a.md\0docs/b\nc.md\0"
        self.assertEqual(self.run_cli("--event", "push", "--paths-from-stdin0", stdin=stdin), "run=false\n")

    def test_truncated_output_fails_the_step(self) -> None:
        # The workflow trusts `run=false` only from a successful step.
        with tempfile.TemporaryFile() as out:
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--event", "push", "--paths-from-stdin0"],
                input=b"README.md\0", stdout=out, stderr=subprocess.PIPE,
                preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_FSIZE, (9, 9)),
            )
            out.seek(0)
            self.assertEqual(out.read(), b"run=false")
        self.assertNotEqual(result.returncode, 0)

    def test_git_failure_runs(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            self.assertEqual(
                self.run_cli("--event", "push", "--base-ref", "origin/main", cwd=Path(temp)),
                "run=true\n",
            )


class GitMergeBaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.repo = Path(self._temp.name)
        self.env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.com",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.com",
        }
        self.git("init", "-q", "-b", "main")
        self.commit("README.md")

    def tearDown(self) -> None:
        self._temp.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args], cwd=self.repo, env=self.env, check=True,
            capture_output=True, text=True,
        ).stdout

    def commit(self, path: str) -> None:
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(f"{file.read_text() if file.exists() else ''}x\n")
        self.git("add", path)
        self.git("commit", "-q", "-m", path)

    def decide(self) -> str:
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--event", "push", "--base-ref", "main"],
            cwd=self.repo, env=self.env, check=True, capture_output=True, text=True,
        ).stdout

    def test_branch_without_rust_skips_even_if_main_moved_rust(self) -> None:
        self.git("checkout", "-q", "-b", "topic")
        self.commit("docs/a.md")
        self.git("checkout", "-q", "main")
        self.commit("src/lib.rs")
        self.git("checkout", "-q", "topic")
        self.assertEqual(self.decide(), "run=false\n")

    def test_rust_anywhere_on_branch_runs(self) -> None:
        self.git("checkout", "-q", "-b", "topic")
        self.commit("src/lib.rs")
        self.commit("docs/a.md")
        self.assertEqual(self.decide(), "run=true\n")

    def test_rename_out_of_rust_path_runs(self) -> None:
        self.commit("src/old.rs")
        self.git("checkout", "-q", "-b", "topic")
        (self.repo / "notes").mkdir()
        self.git("mv", "src/old.rs", "notes/old.txt")
        self.git("commit", "-q", "-m", "mv")
        self.assertEqual(self.decide(), "run=true\n")

    def test_gitattributes_only_change_runs(self) -> None:
        self.commit("src/lib.rs")
        self.git("checkout", "-q", "-b", "topic")
        (self.repo / ".gitattributes").write_text("*.rs working-tree-encoding=UTF-16\n")
        self.git("add", ".gitattributes")
        self.git("commit", "-q", "-m", "attrs")
        self.assertEqual(self.decide(), "run=true\n")

    def test_branch_equal_to_base_runs(self) -> None:
        self.git("checkout", "-q", "-b", "topic")
        self.assertEqual(self.decide(), "run=true\n")

    def assert_branch_change_runs(self, path: str) -> None:
        self.git("checkout", "-q", "-b", "topic")
        self.commit("docs/a.md")
        self.commit(path)
        self.assertEqual(self.decide(), "run=true\n")

    def test_non_ascii_rust_path_runs(self) -> None:
        # Without -z git quotes non-ASCII paths, hiding the `.rs` suffix.
        self.assert_branch_change_runs("src/한글.rs")

    def test_rust_path_with_space_runs(self) -> None:
        self.assert_branch_change_runs("src/my module.rs")

    def test_rust_path_with_newline_in_directory_runs(self) -> None:
        self.assert_branch_change_runs("src/dir\nname/lib.rs")

    def test_migration_with_newline_in_name_runs(self) -> None:
        self.assert_branch_change_runs("migrations/a\nb.sql")

    def test_both_macos_jobs_skip_docs_only_push(self) -> None:
        # Overflow can send a docs-only push to the hosted job instead.
        self.git("checkout", "-q", "-b", "topic")
        self.commit("docs/a.md")
        self.git("update-ref", "refs/remotes/origin/main", "main")
        jobs = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        for job in ("macos_hosted", "macos_self_hosted"):
            with self.subTest(job=job):
                step = next(s for s in jobs[job]["steps"] if s.get("id") == "rust_filter")
                output = self.repo.parent / f"{self.repo.name}-{job}.out"
                subprocess.run(
                    ["bash", "-c", step["run"].replace("scripts/", f"{ROOT}/scripts/")],
                    cwd=self.repo, check=True, capture_output=True, text=True,
                    env={**self.env, **step["env"], "EVENT_NAME": "push", "GITHUB_OUTPUT": str(output)},
                )
                self.assertEqual(output.read_text(), "run=false\n")
                output.unlink()


if __name__ == "__main__":
    unittest.main()
