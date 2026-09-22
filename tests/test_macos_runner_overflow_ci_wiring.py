"""Contracts for the macOS hosted-overflow routing in ci-macos-trusted.yml."""

from __future__ import annotations

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github/workflows/ci-macos-trusted.yml"
SCRIPT = REPO_ROOT / "scripts/ci/macos-runner-overflow.py"
LABELS = ["self-hosted", "macOS", "agentdesk-macos"]
LABELS_JSON = '["self-hosted","macOS","agentdesk-macos"]'

spec = importlib.util.spec_from_file_location("macos_runner_overflow", SCRIPT)
overflow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(overflow)


def runner(name: str, *, busy: bool, status: str = "online", labels=LABELS) -> dict:
    return {"name": name, "status": status, "busy": busy, "labels": [{"name": label} for label in labels]}


def jobs() -> dict:
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))["jobs"]


def resolve_script() -> str:
    step = next(s for s in jobs()["resolve_macos_runner"]["steps"] if s.get("id") == "resolve")
    return step["run"]


def run_resolve(env: dict[str, str], *, overflow_stub: str | None = None) -> dict[str, str]:
    """Run the resolve step's bash and return its GITHUB_OUTPUT as a dict."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp_path = Path(tmp)
        script_dir = tmp_path / "scripts/ci"
        script_dir.mkdir(parents=True)
        target = script_dir / "macos-runner-overflow.py"
        if overflow_stub is None:
            target.write_text(SCRIPT.read_text(encoding="utf-8"), encoding="utf-8")
        else:
            target.write_text(overflow_stub, encoding="utf-8")
        output = tmp_path / "output"
        summary = tmp_path / "summary"
        full_env = {
            "PATH": os.environ["PATH"],
            "GITHUB_OUTPUT": str(output),
            "GITHUB_STEP_SUMMARY": str(summary),
            # Actions injects unset `vars.*` / `secrets.*` as empty strings.
            "MACOS_RUNNER": "",
            "MACOS_RUNNER_GROUP": "",
            "RUNNER_QUERY_TOKEN": "",
            **env,
        }
        subprocess.run(
            ["bash", "-c", resolve_script()], cwd=tmp, env=full_env, check=True, capture_output=True, text=True
        )
        pairs = [line.split("=", 1) for line in output.read_text(encoding="utf-8").splitlines()]
        result = dict(pairs)
        result["_summary"] = summary.read_text(encoding="utf-8")
        return result


class DecideTests(unittest.TestCase):
    def test_all_matching_runners_busy_routes_hosted(self) -> None:
        mode, _ = overflow.decide([runner("mini", busy=True), runner("book", busy=True)], LABELS)
        self.assertEqual(mode, "hosted")

    def test_busy_plus_offline_routes_hosted(self) -> None:
        mode, _ = overflow.decide([runner("mini", busy=True), runner("book", busy=False, status="offline")], LABELS)
        self.assertEqual(mode, "hosted")

    def test_one_idle_runner_keeps_self_hosted(self) -> None:
        mode, _ = overflow.decide([runner("mini", busy=True), runner("book", busy=False)], LABELS)
        self.assertEqual(mode, "self-hosted")

    def test_idle_runner_without_the_labels_does_not_count(self) -> None:
        runners = [runner("mini", busy=True), runner("linux", busy=False, labels=["self-hosted", "Linux"])]
        mode, _ = overflow.decide(runners, LABELS)
        self.assertEqual(mode, "hosted")

    def test_no_matching_runner_keeps_self_hosted(self) -> None:
        mode, _ = overflow.decide([], LABELS)
        self.assertEqual(mode, "self-hosted")

    def test_query_failure_keeps_self_hosted(self) -> None:
        env = {"MACOS_RUNNER": LABELS_JSON, "GITHUB_REPOSITORY": "o/r", "RUNNER_QUERY_TOKEN": "t"}
        with mock.patch.dict(os.environ, env), mock.patch.object(
            overflow.urllib.request, "urlopen", side_effect=OSError("403 rate limited")
        ), mock.patch("builtins.print") as printed:
            overflow.main()
        self.assertEqual(printed.call_args_list[-1].args, ("self-hosted",))


class ResolveStepTests(unittest.TestCase):
    def test_without_token_and_without_var_routes_hosted_as_before(self) -> None:
        out = run_resolve({})
        self.assertEqual((out["mode"], out["group"], out["labels"]), ("hosted", "", "[]"))

    def test_without_token_routes_self_hosted_as_before(self) -> None:
        # The overflow script must not even run without the secret.
        out = run_resolve({"MACOS_RUNNER": LABELS_JSON}, overflow_stub="raise SystemExit('invoked')\n")
        self.assertEqual((out["mode"], out["group"], out["labels"]), ("self-hosted", "", LABELS_JSON))
        self.assertIn("macOS route: self-hosted", out["_summary"])

    def test_token_and_saturated_runners_route_hosted(self) -> None:
        out = run_resolve(
            {"MACOS_RUNNER": LABELS_JSON, "RUNNER_QUERY_TOKEN": "t"}, overflow_stub="print('hosted')\n"
        )
        self.assertEqual((out["mode"], out["labels"]), ("hosted", '["macos-latest"]'))
        self.assertIn("macOS route: hosted", out["_summary"])

    def test_token_and_crashing_overflow_script_keeps_self_hosted(self) -> None:
        out = run_resolve(
            {"MACOS_RUNNER": LABELS_JSON, "RUNNER_QUERY_TOKEN": "t"}, overflow_stub="raise SystemExit(3)\n"
        )
        self.assertEqual((out["mode"], out["labels"]), ("self-hosted", LABELS_JSON))


class HostedJobWiringTests(unittest.TestCase):
    def test_overflow_reuses_existing_hosted_job_without_sccache(self) -> None:
        all_jobs = jobs()
        hosted = all_jobs["macos_hosted"]
        self.assertEqual(hosted["name"], "Trusted macOS check (hosted)")
        self.assertEqual(hosted["if"], "needs.resolve_macos_runner.outputs.mode == 'hosted'")
        disable = next(s for s in hosted["steps"] if s.get("name") == "Disable sccache on hosted macOS")
        self.assertIn('echo "RUSTC_WRAPPER="', disable["run"])
        self.assertEqual(all_jobs["macos_self_hosted"]["name"], "Trusted macOS check (self-hosted)")
        runs_on = {job.get("runs-on") for job in all_jobs.values()}
        self.assertEqual(sum(1 for job in all_jobs.values() if job.get("runs-on") == "macos-latest"), 1, runs_on)


if __name__ == "__main__":
    unittest.main()
