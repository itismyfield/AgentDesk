from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
DIGEST_SCRIPT = REPO_ROOT / "scripts/relay_mutation_wiring_digest.py"
WORKFLOW = ".github/workflows/ci-pr.yml"
REAL_WORKFLOW = (REPO_ROOT / WORKFLOW).read_text(encoding="utf-8")
GIT_ENV = {
    **os.environ,
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
    "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
}
# A relay job that takes its env from an anchor declared in an unrelated job.
ANCHORED = """\
on: pull_request
jobs:
  other:
    runs-on: ubuntu-latest
    timeout-minutes: 5
    env: &shared
      RELAY_AUTHORITY_MUTATION_SHARD_TOTAL: "3"
    steps: [{run: "true"}]
  relay_authority_mutations:
    runs-on: ubuntu-latest
    env: *shared
    steps: [{run: bash scripts/run_relay_authority_mutations.sh}]
  relay-authority-contract:
    runs-on: ubuntu-latest
    steps: [{run: "true"}]
"""
# A relay job env value written as each side of a pair below.
SCALAR = """\
on: pull_request
jobs:
  relay_authority_mutations:
    runs-on: ubuntu-latest
    env:
      MODE: {value}
    steps: [{{run: bash scripts/run_relay_authority_mutations.sh}}]
  relay-authority-contract:
    runs-on: ubuntu-latest
    steps: [{{run: "true"}}]
"""
# PyYAML's YAML 1.1 typing, or the source text alone for the explicit tags, maps both
# sides of each pair to one value; GitHub reads YAML 1.2, where they differ.
MERGED_BY_YAML_1_1 = (
    ("yes", "true"), ("on", "true"), ("off", "no"), ("055", "45"), ("1.0", "1.00"),
    ("0o17", "'0o17'"), ("1_000", "1000"), ("1:30", "90"),
    ("2026-10-02 00:00:00", "2026-10-02T00:00:00"),
    ("!!str 055", "055"), ('!!int "055"', '"055"'),
)


def edit(text: str, old: str, new: str) -> str:
    """Replace one exact line; a fixture whose anchor drifted must fail, not no-op."""
    lines = text.split("\n")
    assert lines.count(old) == 1, f"{old!r} occurs {lines.count(old)} times"
    return "\n".join(new if line == old else line for line in lines)


class WiringDigestTests(unittest.TestCase):
    """Run the digest step as CI does: cwd at a pull_request merge commit whose
    first parent is the base branch, reading the GITHUB_OUTPUT it appends to."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name) / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")

    def git(self, *args: str) -> None:
        subprocess.run(["git", *args], cwd=self.repo, env=GIT_ENV, check=True,
                       capture_output=True)

    def commit(self, text: str | None, message: str) -> None:
        path = self.repo / WORKFLOW
        path.parent.mkdir(parents=True, exist_ok=True)
        if text is None:
            (self.repo / "README").write_text(message, encoding="utf-8")
        else:
            path.write_text(text, encoding="utf-8")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)

    def merge_pr(self, base: str, head: str) -> None:
        self.commit(base, "base")
        self.git("checkout", "-q", "-b", "pr")
        self.commit(head, "pr")
        self.git("checkout", "-q", "main")
        self.git("merge", "-q", "--no-ff", "-m", "merge", "pr")

    def run_digest(self) -> str:
        output = Path(self.tmp.name) / "github_output"
        result = subprocess.run(
            [sys.executable, str(DIGEST_SCRIPT)], cwd=self.repo, capture_output=True,
            text=True, env={**GIT_ENV, "GITHUB_OUTPUT": str(output)},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return output.read_text(encoding="utf-8")

    def judge(self, base: str, head: str) -> str:
        self.merge_pr(base, head)
        return self.run_digest()

    def test_unrelated_job_edit_does_not_run_the_gate(self) -> None:
        head = edit(REAL_WORKFLOW, "    name: Changed paths", "    name: Changed paths renamed")
        self.assertEqual(self.judge(REAL_WORKFLOW, head), "wiring_changed=false\n")

    def test_mutation_job_step_edit_runs_the_gate(self) -> None:
        line = "        run: bash scripts/run_relay_authority_mutations.sh"
        head = edit(REAL_WORKFLOW, line, line + " || true")
        self.assertEqual(self.judge(REAL_WORKFLOW, head), "wiring_changed=true\n")

    def test_workflow_level_edit_runs_the_gate(self) -> None:
        for old, new in (
            ("  CARGO_TERM_COLOR: always", "  CARGO_TERM_COLOR: never"),
            # PyYAML loads the bare `on:` key as True; a trigger edit must still count.
            ("  pull_request:", "  pull_request:\n    types: [opened]"),
        ):
            with self.subTest(edit=old.strip()):
                self.setUp()
                self.assertEqual(self.judge(REAL_WORKFLOW, edit(REAL_WORKFLOW, old, new)),
                                 "wiring_changed=true\n")

    def test_mirror_job_edit_runs_the_gate(self) -> None:
        head = edit(REAL_WORKFLOW, "          UPSTREAM_JOB_NAME: relay_authority_mutations",
                    "          UPSTREAM_JOB_NAME: relay_authority_targets")
        self.assertEqual(self.judge(REAL_WORKFLOW, head), "wiring_changed=true\n")

    def test_anchor_edit_outside_the_relay_job_is_read_through_the_alias(self) -> None:
        shared = edit(ANCHORED, '      RELAY_AUTHORITY_MUTATION_SHARD_TOTAL: "3"',
                      '      RELAY_AUTHORITY_MUTATION_SHARD_TOTAL: "1"')
        self.assertEqual(self.judge(ANCHORED, shared), "wiring_changed=true\n")
        # The same unrelated job, edited outside the anchor, leaves the relay job alone.
        self.setUp()
        local = edit(ANCHORED, "    timeout-minutes: 5", "    timeout-minutes: 9")
        self.assertEqual(self.judge(ANCHORED, local), "wiring_changed=false\n")

    def test_scalars_yaml_1_1_merges_still_count_as_changed(self) -> None:
        for old, new in MERGED_BY_YAML_1_1:
            with self.subTest(old=old, new=new):
                self.setUp()
                self.assertEqual(self.judge(SCALAR.format(value=old), SCALAR.format(value=new)),
                                 "wiring_changed=true\n")

    def test_workflow_step_without_pyyaml_runs_the_gate_and_never_fails_the_job(self) -> None:
        """Run the step's own `run:` block under bash -e, as Actions does, after an unrelated
        edit: with PyYAML it says false; with PyYAML unimportable and its install failing
        it must still exit 0 and say true."""
        step = next(s for s in yaml.safe_load(REAL_WORKFLOW)["jobs"]["relay_authority_mutations"]["steps"]
                    if s.get("id") == "mutation_wiring")
        self.merge_pr(REAL_WORKFLOW, edit(REAL_WORKFLOW, "    name: Changed paths",
                                          "    name: Changed paths renamed"))
        (self.repo / "scripts").mkdir()
        shutil.copy2(DIGEST_SCRIPT, self.repo / "scripts" / DIGEST_SCRIPT.name)
        tmp = Path(self.tmp.name)
        (tmp / "bin").mkdir()
        (tmp / "bin/python3").write_text(f'#!/bin/sh\nexec "{sys.executable}" "$@"\n')
        (tmp / "bin/python3").chmod(0o755)
        (tmp / "shadow/yaml").mkdir(parents=True)
        (tmp / "shadow/yaml/__init__.py").write_text("raise ImportError('PyYAML hidden by the test')\n")
        # Shadows `python3 -m pip` too, so the install fails whatever the host has installed.
        (tmp / "shadow/pip").mkdir()
        (tmp / "shadow/pip/__init__.py").write_text("")
        (tmp / "shadow/pip/__main__.py").write_text("raise SystemExit('pip unavailable in this test')\n")
        for case, extra, expected in (
            ("PyYAML available", {}, "wiring_changed=false\n"),
            ("PyYAML missing", {"PYTHONPATH": str(tmp / "shadow")}, "wiring_changed=true\n"),
        ):
            with self.subTest(case=case):
                output = tmp / f"output-{len(extra)}"
                env = {**GIT_ENV, "PATH": f"{tmp / 'bin'}{os.pathsep}{os.environ['PATH']}",
                       "PIP_NO_INDEX": "1", "GITHUB_OUTPUT": str(output), **extra}
                result = subprocess.run(
                    ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
                    cwd=self.repo, env=env, capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(output.read_text(encoding="utf-8"), expected,
                                 result.stdout + result.stderr)

    def test_missing_parent_or_unreadable_workflow_runs_the_gate(self) -> None:
        with self.subTest(case="no HEAD^1"):
            self.commit(REAL_WORKFLOW, "root")
            self.assertEqual(self.run_digest(), "wiring_changed=true\n")
        with self.subTest(case="YAML parse error"):
            self.setUp()
            self.assertEqual(self.judge(REAL_WORKFLOW, REAL_WORKFLOW + "\n  bad: [\n"),
                             "wiring_changed=true\n")
        with self.subTest(case="workflow absent at HEAD^1"):
            self.setUp()
            self.commit(None, "base without workflow")
            self.git("checkout", "-q", "-b", "pr")
            self.commit(REAL_WORKFLOW, "pr")
            self.git("checkout", "-q", "main")
            self.git("merge", "-q", "--no-ff", "-m", "merge", "pr")
            self.assertEqual(self.run_digest(), "wiring_changed=true\n")


if __name__ == "__main__":
    unittest.main()
