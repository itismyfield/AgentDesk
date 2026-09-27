"""Contracts for the macOS hosted-overflow routing in ci-macos-trusted.yml."""

from __future__ import annotations

import importlib.util
import io
import itertools
import json
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
runner_ids = itertools.count(1)


def runner(name: str, *, busy: bool, status: str = "online", labels=LABELS) -> dict:
    return {
        "id": next(runner_ids),
        "name": name,
        "status": status,
        "busy": busy,
        "labels": [{"name": label} for label in labels],
    }


def page(total: int, runners: list) -> io.BytesIO:
    return io.BytesIO(json.dumps({"total_count": total, "runners": runners}).encode())


def run_main(**urlopen_kwargs) -> tuple[str, mock.Mock]:
    """Run main() against a mocked urlopen; return the printed mode and the mock."""
    env = {"MACOS_RUNNER": LABELS_JSON, "GITHUB_REPOSITORY": "o/r", "RUNNER_QUERY_TOKEN": "t"}
    with mock.patch.dict(os.environ, env), mock.patch.object(
        overflow.urllib.request, "urlopen", **urlopen_kwargs
    ) as urlopen, mock.patch("builtins.print") as printed:
        overflow.main()
    return printed.call_args_list[-1].args[0], urlopen


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
            "OVERFLOW_CHECKOUT": "",
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

    def test_labels_match_case_insensitively_like_github(self) -> None:
        runners = [runner("mini", busy=True), runner("book", busy=False, labels=["self-hosted", "macos", "AgentDesk-macOS"])]
        mode, _ = overflow.decide(runners, LABELS)
        self.assertEqual(mode, "self-hosted")

    def main_with_response(self, **kwargs) -> str:
        return run_main(**kwargs)[0]

    def test_query_failure_keeps_self_hosted(self) -> None:
        self.assertEqual(self.main_with_response(side_effect=OSError("403 rate limited")), "self-hosted")

    def test_well_formed_json_with_unrecognized_runner_state_keeps_self_hosted(self) -> None:
        missing_status = [runner("mini", busy=True), runner("book", busy=True)]
        for entry in missing_status:
            del entry["status"]
        string_busy = [runner("mini", busy=True), {**runner("book", busy=False), "busy": "false"}]
        for case, runners in (("status missing", missing_status), ("busy as string", string_busy)):
            with self.subTest(case=case):
                body = io.BytesIO(json.dumps({"total_count": 2, "runners": runners}).encode())
                self.assertEqual(self.main_with_response(return_value=body), "self-hosted")

    def test_malformed_runner_entry_keeps_self_hosted(self) -> None:
        bad_labels = {**runner("book", busy=True), "labels": "macOS"}
        for case, entry in (("labels not a list", bad_labels), ("entry not an object", "book")):
            with self.subTest(case=case):
                body = page(2, [runner("mini", busy=True), entry])
                self.assertEqual(self.main_with_response(return_value=body), "self-hosted")

    def test_complete_page_with_an_idle_runner_keeps_self_hosted(self) -> None:
        body = page(2, [runner("mini", busy=True), runner("book", busy=False)])
        self.assertEqual(self.main_with_response(return_value=body), "self-hosted")

    def test_complete_saturated_response_routes_hosted(self) -> None:
        runners = [runner("mini", busy=True), runner("book", busy=True)]
        body = io.BytesIO(json.dumps({"total_count": 2, "runners": runners}).encode())
        self.assertEqual(self.main_with_response(return_value=body), "hosted")

    def test_incomplete_runner_list_keeps_self_hosted(self) -> None:
        # Only the listed runner is known busy; the unlisted one may be idle.
        cases = (
            ("total_count exceeds listed", {"total_count": 2}),
            ("total_count below listed", {"total_count": 0}),
            ("total_count missing", {}),
            ("total_count as string", {"total_count": "1"}),
        )
        for case, extra in cases:
            with self.subTest(case=case):
                body = io.BytesIO(json.dumps({**extra, "runners": [runner("mini", busy=True)]}).encode())
                self.assertEqual(self.main_with_response(side_effect=[body, page(2, [])]), "self-hosted")


class PaginationTests(unittest.TestCase):
    @staticmethod
    def two_pages(*, last_busy: bool = True) -> tuple[list, list]:
        first = [runner(f"mac-{i}", busy=True) for i in range(overflow.PER_PAGE)]
        return first, [runner("mac-last", busy=last_busy)]

    def test_two_complete_saturated_pages_route_hosted(self) -> None:
        first, second = self.two_pages()
        mode, urlopen = run_main(side_effect=[page(101, first), page(101, second)])
        self.assertEqual(mode, "hosted")
        self.assertEqual(urlopen.call_count, 2)
        urls = [call.args[0].full_url for call in urlopen.call_args_list]
        self.assertEqual(
            urls,
            [f"https://api.github.com/repos/o/r/actions/runners?per_page=100&page={n}" for n in (1, 2)],
        )

    def test_idle_runner_on_second_page_keeps_self_hosted(self) -> None:
        first, second = self.two_pages(last_busy=False)
        mode, urlopen = run_main(side_effect=[page(101, first), page(101, second)])
        self.assertEqual((mode, urlopen.call_count), ("self-hosted", 2))

    def test_second_page_network_error_keeps_self_hosted(self) -> None:
        first, _ = self.two_pages()
        mode, urlopen = run_main(side_effect=[page(101, first), OSError("connection reset")])
        self.assertEqual((mode, urlopen.call_count), ("self-hosted", 2))

    def test_total_count_changing_between_pages_keeps_self_hosted(self) -> None:
        first, second = self.two_pages()
        mode, _ = run_main(side_effect=[page(101, first), page(102, second)])
        self.assertEqual(mode, "self-hosted")

    def test_runner_repeated_across_pages_keeps_self_hosted(self) -> None:
        # Churn can shift a runner onto the next page while another slips out unseen.
        first, _ = self.two_pages()
        mode, _ = run_main(side_effect=[page(101, first), page(101, [first[-1]])])
        self.assertEqual(mode, "self-hosted")

    def test_short_second_page_keeps_self_hosted(self) -> None:
        first, _ = self.two_pages()
        mode, urlopen = run_main(side_effect=[page(102, first), page(102, []), page(102, [])])
        self.assertEqual((mode, urlopen.call_count), ("self-hosted", 2))

    def test_runner_list_beyond_page_limit_keeps_self_hosted_without_more_requests(self) -> None:
        first, _ = self.two_pages()
        mode, urlopen = run_main(side_effect=[page(overflow.PER_PAGE * overflow.MAX_PAGES + 1, first)])
        self.assertEqual((mode, urlopen.call_count), ("self-hosted", 1))


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
            {"MACOS_RUNNER": LABELS_JSON, "RUNNER_QUERY_TOKEN": "t", "OVERFLOW_CHECKOUT": "success"},
            overflow_stub="print('hosted')\n",
        )
        self.assertEqual((out["mode"], out["labels"]), ("hosted", '["macos-15"]'))
        self.assertIn("macOS route: hosted", out["_summary"])

    def test_token_and_crashing_overflow_script_keeps_self_hosted(self) -> None:
        out = run_resolve(
            {"MACOS_RUNNER": LABELS_JSON, "RUNNER_QUERY_TOKEN": "t", "OVERFLOW_CHECKOUT": "success"},
            overflow_stub="raise SystemExit(3)\n",
        )
        self.assertEqual((out["mode"], out["labels"]), ("self-hosted", LABELS_JSON))


    def test_failed_overflow_checkout_keeps_self_hosted(self) -> None:
        out = run_resolve(
            {"MACOS_RUNNER": LABELS_JSON, "RUNNER_QUERY_TOKEN": "t", "OVERFLOW_CHECKOUT": "failure"},
            overflow_stub="print('hosted')\n",
        )
        self.assertEqual((out["mode"], out["labels"]), ("self-hosted", LABELS_JSON))


class OverflowCheckoutWiringTests(unittest.TestCase):
    def setUp(self) -> None:
        self.job = jobs()["resolve_macos_runner"]
        self.checkout = next(s for s in self.job["steps"] if s.get("id") == "overflow_checkout")

    def test_checkout_runs_only_when_overflow_can_run(self) -> None:
        self.assertEqual(self.checkout["if"], "env.MACOS_RUNNER != '' && env.RUNNER_QUERY_TOKEN != ''")
        self.assertEqual(self.job["env"]["RUNNER_QUERY_TOKEN"], "${{ secrets.RUNNER_QUERY_TOKEN }}")
        self.assertEqual(self.job["env"]["MACOS_RUNNER"], "${{ vars.MACOS_RUNNER }}")

    def test_checkout_failure_does_not_fail_routing(self) -> None:
        self.assertIs(self.checkout["continue-on-error"], True)
        resolve = next(s for s in self.job["steps"] if s.get("id") == "resolve")
        self.assertNotIn("if", resolve)
        self.assertEqual(resolve["env"]["OVERFLOW_CHECKOUT"], "${{ steps.overflow_checkout.outcome }}")


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
        self.assertEqual(sum(1 for job in all_jobs.values() if job.get("runs-on") == "macos-15"), 1, runs_on)

    def test_h2_measurement_job_pins_an_arm64_label(self) -> None:
        # H2 r9 §3: the hosted job running h2_measure.sh must not float on macos-latest.
        all_jobs = jobs()
        for name, job in all_jobs.items():
            if any("h2_measure.sh" in str(step.get("run", "")) for step in job.get("steps", [])):
                with self.subTest(job=name):
                    self.assertNotIn("macos-latest", str(job["runs-on"]))
        self.assertEqual(all_jobs["macos_hosted"]["runs-on"], "macos-15")
        self.assertNotIn("macos-latest", WORKFLOW.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
