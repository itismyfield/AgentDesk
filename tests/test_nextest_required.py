"""required-gate, comparison-base and classify-worker of scripts/ci/nextest_required.py."""

import contextlib
import io
import json
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_nextest_ci import SHA, Fixture, junit, nci  # noqa: E402

nreq = sys.modules["nextest_required"]

N = ("rust_build", "rust_tests", "test_evidence")
# Written from the old *_required_context mirrors and design section 1.5, not from the implementation:
# context -> (N gate, {own job: gate}). A gate lists changes outputs that make the job expected; "!x" means x false.
EXPECTED = {
    "Lint": (["rust_tests"], {"lint": ["rust_or_policy", "relay_contract"]}),
    "Script checks": (["rust_tests"], {"scripts": [], "scripts_guards": [], "scripts_contracts": []}),
    "Fast check (ubuntu-latest)": (["rust_tests"], {"lint": ["rust_or_policy", "relay_contract"]}),
    "High-risk recovery": (["rust_tests", "high_risk_recovery"], {}),
    "Dashboard (Node 22)": (["rust_tests"], {"dashboard": ["dashboard"]}),
    "Fast targeted tests (ubuntu-latest)": (["rust_tests"], {"pg15_owner_proof": ["rust_tests"]}),
    "relay-authority-contract": (["rust_tests"], {"relay_authority_mutations": ["!rust_tests_skip"]}),
    "Library test sweep (ubuntu-latest)": (["rust_tests"], {}),
    "Fast check cross OS required context (ubuntu-latest)": (
        ["rust_tests"], {"check_fast_cross_os": ["cross_os_rust"], "check_fast_cross_os_targets": ["cross_os_rust"]}),
}
ON = {"rust_tests": "true", "rust_or_policy": "true", "relay_contract": "true", "high_risk_recovery": "true",
      "dashboard": "true", "cross_os_rust": "true", "rust_tests_skip": "false"}


def jobs_of(name):
    n_gate, own = EXPECTED[name]
    return {**{job: n_gate for job in N}, **own}


def off(outputs, terms):
    """outputs with every term of a gate made false."""
    return {**outputs, **{t.lstrip("!"): ("true" if t.startswith("!") else "false") for t in terms}}


def expected(terms, outputs):
    return not terms or any((outputs[t.lstrip("!")] == "true") != t.startswith("!") for t in terms)


ALL_JOBS = sorted({job for name in EXPECTED for job in jobs_of(name)})


class RequiredGate(unittest.TestCase):
    """`CI required`: the nine contexts, each N AND its own jobs, failing closed like the old mirror."""

    def gate(self, outputs, results, changes="success", pins="success", extra=None):
        """(rc, stdout, summary) of `required-gate`; results override all-success, None drops a need."""
        merged = {**dict.fromkeys(ALL_JOBS, "success"), **results}
        needs = {"changes": {"result": changes, "outputs": outputs},
                 **{job: {"result": r} for job, r in merged.items() if r is not None}, **(extra or {})}
        with tempfile.TemporaryDirectory() as temp, mock.patch.dict(os.environ, {"PINS_OUTCOME": pins}):
            summary = Path(temp) / "summary.md"
            with contextlib.redirect_stdout(out := io.StringIO()):
                rc = nci.main(["required-gate", "--needs-json", json.dumps(needs), "--summary-out", str(summary)])
            return rc, out.getvalue(), summary.read_text() if summary.exists() else ""

    def verdict(self, name, outputs, results, changes="success"):
        """0 when `name` passed inside the gate; a failed context always fails the whole gate."""
        rc, out, _ = self.gate(outputs, results, changes)
        passed = f"{name}: PASS" in out.splitlines()
        if not passed:
            self.assertEqual(rc, 1, out)
        return (0 if passed else 1), out

    def all_success(self, name):
        return {job: "success" for job in jobs_of(name)}

    def test_the_gate_passes_only_with_all_nine_and_reports_each_in_log_and_summary(self):
        self.assertEqual(sorted(nreq.CONTEXTS), sorted(EXPECTED))
        rc, out, summary = self.gate(ON, {})
        self.assertEqual(rc, 0, out)
        self.assertEqual(out.splitlines()[-1], "CI required: PASS")
        self.assertIn("## CI required: PASS", summary)
        for name in EXPECTED:
            self.assertIn(f"{name}: PASS", out.splitlines())
            self.assertIn(f"| {name} | PASS | ", summary)

    def test_every_job_failed_cancelled_missing_or_wrongly_skipped_fails_its_context(self):
        for name in EXPECTED:
            for job in jobs_of(name):
                for result in ("failure", "cancelled", None, "skipped"):
                    rc, out = self.verdict(name, ON, {**self.all_success(name), job: result})
                    self.assertEqual(rc, 1, (name, job, result))
                    self.assertIn(f"::error::{name}: {job}: ", out)

    def test_a_job_whose_gate_is_off_may_skip_but_may_not_fail(self):
        for name in EXPECTED:
            for job, terms in jobs_of(name).items():
                if not terms:
                    continue
                outputs = off(ON, terms)
                results = {j: ("success" if expected(t, outputs) else "skipped") for j, t in jobs_of(name).items()}
                self.assertEqual(self.verdict(name, outputs, results)[0], 0, (name, job))
                self.assertEqual(self.verdict(name, outputs, {**results, job: "cancelled"})[0], 1, (name, job))

    def test_any_single_term_of_an_or_gate_makes_the_job_expected(self):
        for name in EXPECTED:
            for job, terms in jobs_of(name).items():
                for term in terms:
                    outputs = {**off(ON, terms), **{k: v for k, v in ON.items() if k == term.lstrip("!")}}
                    rc, _ = self.verdict(name, outputs, {**self.all_success(name), job: "skipped"})
                    self.assertEqual(rc, 1, (name, job, term))

    def test_missing_or_invalid_gate_output_fails(self):
        for name in EXPECTED:
            for term in {t.lstrip("!") for terms in jobs_of(name).values() for t in terms}:
                for bad in (None, "", "yes"):
                    outputs = {k: v for k, v in {**ON, term: bad}.items() if v is not None}
                    self.assertEqual(self.verdict(name, outputs, self.all_success(name))[0], 1, (name, term, bad))

    def test_pin_outcome_other_than_success_fails_exactly_the_two_pinned_contexts(self):
        for pins in ("failure", "skipped", ""):
            with self.subTest(pins=pins):
                rc, out, _ = self.gate(ON, {}, pins=pins)
                self.assertEqual(rc, 1)
                failed = {name for name in EXPECTED if f"{name}: FAIL" in out.splitlines()}
                self.assertEqual(failed, {"Script checks", "relay-authority-contract"})

    def test_changes_failure_an_unjudged_need_or_bad_json_fails(self):
        for changes in ("failure", "cancelled", "skipped"):
            self.assertEqual(self.gate(ON, {}, changes)[0], 1, changes)
        rc, out, _ = self.gate(ON, {}, extra={"win32_build_token": {"result": "success"}})
        self.assertEqual(rc, 1)
        self.assertIn("::error::CI required: needs job win32_build_token is judged by no context", out)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(nci.main(["required-gate", "--needs-json", "{", "--summary-out", ""]), 1)


class ComparisonBase(Fixture):
    """Which SHA a PR plan is compared with, and that the baseline belongs to it."""

    def git(self, *argv):
        return subprocess.run(["git", "-C", self.dir / "repo", *argv], check=True, capture_output=True,
                              text=True).stdout.strip()

    def commit(self, message):
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def setUp(self):
        super().setUp()
        (self.dir / "repo").mkdir()
        self.git("init", "-q", "-b", "main")
        self.a = self.commit("A")
        self.git("checkout", "-q", "-b", "pr")
        self.p = self.commit("P")
        self.git("checkout", "-q", "main")
        self.b = self.commit("B")
        # Like refs/pull/N/merge: a detached merge of the PR into the base; main itself stays at B.
        self.git("checkout", "-q", "--detach")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "merge", "-q", "--no-ff", "-m", "M", "pr")
        self.m = self.git("rev-parse", "HEAD")

    def base(self, head, *extra):
        out = self.dir / f"out-{len(list(self.dir.glob('out-*')))}"
        rc, _ = self.run_cli("comparison-base", "--repo", self.dir / "repo", "--head", head, "--pr-head-sha", self.p,
                             "--base-ref", "main", "--github-output", out, *extra)
        return rc, dict(line.split("=", 1) for line in out.read_text().split()) if out.exists() else {}

    def test_merge_ref_uses_its_first_parent_and_head_uses_the_merge_base(self):
        self.assertEqual(self.base(self.m), (0, {"base_sha": self.b, "mode": "merge"}))
        self.assertEqual(self.base(self.p), (0, {"base_sha": self.a, "mode": "head"}))
        self.assertEqual(self.base(self.b)[0], 1)

    def test_baseline_must_exist_and_belong_to_the_base_and_scope(self):
        self.assertEqual(self.base(self.p, "--baseline", self.dir / "none.json", "--scope", "linux-lib")[0], 2)
        self.plan_ok(["m::a"], workers=1, out="newer", sha=self.b)
        self.plan_ok(["m::a"], workers=1, out="right", sha=self.a)
        self.assertEqual(self.base(self.p, "--baseline", self.dir / "newer/plan.json", "--scope", "linux-lib")[0], 2)
        self.assertEqual(self.base(self.p, "--baseline", self.dir / "right/plan.json", "--scope", "macos-lib")[0], 2)
        self.assertEqual(self.base(self.p, "--baseline", self.dir / "right/plan.json", "--scope", "linux-lib")[0], 0)


class ClassifyWorker(Fixture):
    """Retry needs every failed worker to be infra with log evidence, on the first attempt only."""

    def setUp(self):
        super().setUp()
        self.p = self.plan_ok(["m::a", "m::b"], workers=2)

    def attempt(self, worker, attempt, failed=False, exit_code="0", record_attempt=None):
        name = nreq.artifact_name(worker, attempt)
        rc, err = self.run_cli("verify-archive", "--plan", self.dir / "plan/plan.json", "--worker", worker,
                               "--archive", self.archive, "--config", self.dir / f"plan/worker-{worker}.toml",
                               "--sha", SHA, "--run-id", "7", "--run-attempt", record_attempt or attempt,
                               "--record", self.dir / "art" / name)
        self.assertEqual(rc, 0, err)
        if exit_code is not None:
            body = '<failure message="x"/>' if failed else ""
            self.write(f"art/{name}/exit-code", exit_code)
            self.write(f"art/{name}/junit.xml", junit([(t[1], body, 0.1) for t in self.p["workers"][worker]["tests"]]))

    def classify(self, attempt, workers=(0,), evidence=()):
        out = self.dir / f"gh-{len(list(self.dir.glob('gh-*')))}"
        argv = ["classify-worker", "--plan", self.dir / "plan/plan.json", "--artifacts", self.dir / "art",
                "--run-attempt", attempt, "--github-output", out]
        argv += [x for w in workers for x in ("--worker", w)] + [x for w in evidence for x in ("--infra-evidence", w)]
        self.assertEqual(self.run_cli(*argv)[0], 0)
        got = dict(line.split("=", 1) for line in out.read_text().split())
        return got["class"], got["retryable"]

    def test_test_failure_is_never_retryable_even_with_infra_evidence(self):
        self.attempt(0, 1, failed=True, exit_code="100")
        self.attempt(0, 2)
        self.assertEqual(self.classify(1, evidence=[0]), ("test", "false"))
        self.assertEqual(self.classify(2), ("pass", "false"))

    def test_missing_results_are_unknown_without_log_evidence(self):
        self.assertEqual(self.classify(1), ("unknown", "false"))
        self.attempt(0, 1, exit_code=None)
        self.assertEqual(self.classify(1), ("unknown", "false"))

    def test_log_evidence_makes_missing_results_infra_retryable_once(self):
        self.assertEqual(self.classify(1, evidence=[0]), ("infra", "true"))
        self.assertEqual(self.classify(2, evidence=[0]), ("infra", "false"))
        self.attempt(0, 1, exit_code=None)
        self.assertEqual(self.classify(1, evidence=[0]), ("infra", "true"))

    def test_one_test_or_unknown_worker_vetoes_the_retry_of_an_infra_worker(self):
        self.attempt(1, 1, failed=True, exit_code="100")
        self.assertEqual(self.classify(1, workers=(0, 1), evidence=[0, 1]), ("test", "false"))
        self.attempt(1, 1, exit_code="1")
        self.assertEqual(self.classify(1, workers=(0, 1), evidence=[0]), ("unknown", "false"))

    def test_another_attempt_or_unclear_evidence_is_unknown_not_infra(self):
        self.attempt(0, 1)
        self.assertEqual(self.classify(2, evidence=[0]), ("infra", "false"))
        self.attempt(0, 2, record_attempt="1")
        self.assertEqual((self.classify(1), self.classify(2, evidence=[0])), (("pass", "false"), ("unknown", "false")))
        self.attempt(0, 1, exit_code="1")
        self.assertEqual(self.classify(1), ("unknown", "false"))
        (self.dir / "art" / nreq.artifact_name(0, 1) / "worker.json").unlink()
        self.assertEqual(self.classify(1, evidence=[0]), ("unknown", "false"))


class CollectAndSelectors(Fixture):
    """The evidence job audits each worker's newest attempt and every targeted selector."""

    def setUp(self):
        super().setUp()
        self.p = self.plan_ok(["m::a", "m::b"], workers=2)

    def record(self, worker, attempt):
        rc, err = self.run_cli("verify-archive", "--plan", self.dir / "plan/plan.json", "--worker", worker,
                               "--archive", self.archive, "--config", self.dir / f"plan/worker-{worker}.toml",
                               "--sha", SHA, "--run-id", "7", "--run-attempt", attempt,
                               "--record", self.dir / "art" / nreq.artifact_name(worker, attempt))
        self.assertEqual(rc, 0, err)

    def collected(self, attempt):
        out = self.dir / f"res-{attempt}"
        self.assertEqual(self.run_cli("collect-results", "--plan", self.dir / "plan/plan.json", "--artifacts",
                                      self.dir / "art", "--run-attempt", attempt, "--output", out)[0], 0)
        return {p.name: json.loads((p / "worker.json").read_text())["run_attempt"] for p in sorted(out.iterdir())}

    def test_newest_attempt_up_to_this_one_is_collected_per_worker(self):
        for worker, attempt in ((0, 1), (0, 2), (1, 1), (1, 3)):
            self.record(worker, attempt)
        self.assertEqual(self.collected(2), {"worker-0": "2", "worker-1": "1"})
        self.assertEqual(self.collected(1), {"worker-0": "1", "worker-1": "1"})

    def test_every_selector_must_match_a_planned_test(self):
        for text, rc in (("[s]\nm::\n=m::a\n", 0), ("[s]\n=m::\n", 1), ("[s]\nm::a\nnowhere\n", 1), ("# none\n", 1)):
            self.write("sel.txt", text)
            got = self.run_cli("selector-audit", "--plan", self.dir / "plan/plan.json", "--selectors", self.dir / "sel.txt")
            self.assertEqual(got[0], rc, text)


if __name__ == "__main__":
    unittest.main()
