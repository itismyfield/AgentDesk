"""Behavioral contracts for the sharded PR Script checks runners.

Every check in ``scripts/ci-script-checks.sh`` must belong to a shard that a PR
job runs, and the ``Script checks`` required context must fail unless every
shard job succeeds.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/ci-script-checks.sh"
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
MIRROR_JOB = "scripts_required_context"
NEEDS_RESULT = re.compile(r"\$\{\{\s*needs\.([A-Za-z0-9_-]+)\.result\s*\}\}")


def clean_env(**extra: str) -> dict[str, str]:
    # This suite itself runs inside a sharded job; never inherit that selection.
    env = {k: v for k, v in os.environ.items() if not k.startswith("SCRIPT_CHECK_")}
    env.update(extra)
    return env


def scalar(value: object) -> str:
    # Actions renders YAML booleans as lowercase strings.
    return str(value).lower() if isinstance(value, bool) else str(value)


def load_jobs() -> dict:
    return yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]


def workflow_shards(jobs: dict) -> dict[str, str]:
    """Map each job that runs the aggregate script to its SCRIPT_CHECK_SHARD."""
    shards: dict[str, str] = {}
    for job_id, job in jobs.items():
        for step in job.get("steps", []):
            if "scripts/ci-script-checks.sh" in str(step.get("run", "")):
                env = {**job.get("env", {}), **step.get("env", {})}
                shards[job_id] = str(env.get("SCRIPT_CHECK_SHARD", ""))
    return shards


def list_checks(script: Path, shard: str = "") -> subprocess.CompletedProcess[str]:
    env = clean_env(SCRIPT_CHECK_LIST="1", SCRIPT_CHECK_SHARD=shard)
    return subprocess.run(
        ["bash", str(script)], cwd=REPO_ROOT, env=env,
        capture_output=True, text=True, timeout=60,
    )


def listed(result: subprocess.CompletedProcess[str]) -> list[tuple[str, str]]:
    return [tuple(line.split("\t", 1)) for line in result.stdout.splitlines()]


class ScriptCheckShardOwnership(unittest.TestCase):
    def test_every_check_is_owned_by_a_shard_that_a_pr_job_runs(self) -> None:
        shards = workflow_shards(load_jobs())
        self.assertGreaterEqual(len(shards), 2, shards)
        self.assertNotIn("", shards.values(), shards)
        self.assertEqual(len(set(shards.values())), len(shards), shards)

        everything = list_checks(SCRIPT)
        self.assertEqual(everything.returncode, 0, everything.stderr)
        full = listed(everything)
        self.assertTrue(full)
        titles = [title for _, title in full]
        self.assertEqual(len(titles), len(set(titles)), "duplicate check titles")
        # A shard with checks but no PR job would silently drop its checks.
        self.assertEqual({shard for shard, _ in full}, set(shards.values()))

        union: list[tuple[str, str]] = []
        for job_id, shard in sorted(shards.items()):
            with self.subTest(job=job_id, shard=shard):
                result = list_checks(SCRIPT, shard)
                self.assertEqual(result.returncode, 0, result.stderr)
                selected = listed(result)
                self.assertTrue(selected)
                self.assertEqual({owner for owner, _ in selected}, {shard})
                union.extend(selected)
        self.assertEqual(sorted(union), sorted(full))

    def test_sharded_run_rejects_a_check_without_a_shard(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        fixtures = {
            "bare banner": source + '\nbanner "unassigned fixture"\ntrue\n',
            "unknown shard": source
            + '\nif run_check nowhere "unassigned fixture"; then\ntrue\nfi\n',
        }
        shards = sorted(set(workflow_shards(load_jobs()).values()))
        with tempfile.TemporaryDirectory(prefix="script-check-shards-") as tmp:
            for label, text in fixtures.items():
                script = Path(tmp) / "ci-script-checks.sh"
                script.write_text(text, encoding="utf-8")
                for shard in ["", *shards]:
                    with self.subTest(fixture=label, shard=shard or "all"):
                        result = list_checks(script, shard)
                        self.assertNotEqual(result.returncode, 0, result.stdout)
                        self.assertIn("unassigned fixture", result.stderr)


class ScriptChecksMirrorAggregation(unittest.TestCase):
    """Replay the required-context job against upstream shard results."""

    def run_mirror(self, job: dict, results: dict[str, str]) -> str:
        needs = job.get("needs", [])
        needs = [needs] if isinstance(needs, str) else list(needs)
        declared = {name: results[name] for name in needs}
        if job.get("if") != "always()" and any(r != "success" for r in declared.values()):
            return "skipped"

        def expand(value: object) -> str:
            text = NEEDS_RESULT.sub(lambda m: declared.get(m.group(1), ""), scalar(value))
            if "${{" in text:
                raise AssertionError(f"unmodelled expression in mirror step: {text}")
            return text

        for step in job["steps"]:
            raw = f"{step.get('env', {})} {step.get('run', '')}"
            # Steps that never read upstream results cannot turn a failed shard green.
            if "run" not in step or "needs." not in raw:
                continue
            condition = scalar(step.get("if", "success()")).replace(" ", "")
            if condition in ("false", "${{false}}"):
                continue
            if condition not in ("success()", "always()", "${{always()}}"):
                raise AssertionError(f"unmodelled step condition: {condition}")
            env = {k: expand(v) for k, v in step.get("env", {}).items()}
            done = subprocess.run(
                ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", expand(step["run"])],
                cwd=REPO_ROOT, env={**clean_env(), **env},
                capture_output=True, text=True, timeout=60,
            )
            if done.returncode != 0 and not step.get("continue-on-error", False):
                return "failure"
        return "success"

    def test_required_context_passes_only_when_every_shard_succeeds(self) -> None:
        jobs = load_jobs()
        job = jobs[MIRROR_JOB]
        shard_jobs = sorted(workflow_shards(jobs))
        baseline = {name: "success" for name in ["changes", *shard_jobs]}
        self.assertEqual(self.run_mirror(job, baseline), "success")
        for shard_job in shard_jobs:
            for outcome in ("failure", "cancelled", "skipped", ""):
                with self.subTest(job=shard_job, result=outcome or "<empty>"):
                    results = {**baseline, shard_job: outcome}
                    self.assertEqual(self.run_mirror(job, results), "failure")


if __name__ == "__main__":
    unittest.main()
