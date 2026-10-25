"""required-context, comparison-base and classify-worker of scripts/ci/nextest_required.py."""

import contextlib
import io
import json
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import unquote

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


class RequiredContext(unittest.TestCase):
    """The nine branch-protection names: N AND each context's own jobs, failing closed like the old mirror."""

    def verdict(self, name, outputs, results, changes="success"):
        needs = {"changes": {"result": changes, "outputs": outputs},
                 **{job: {"result": r} for job, r in results.items() if r is not None}}
        with contextlib.redirect_stdout(out := io.StringIO()):
            return nci.main(["required-context", name, "--needs-json", json.dumps(needs)]), out.getvalue()

    def all_success(self, name):
        return {job: "success" for job in jobs_of(name)}

    def test_the_nine_names_pass_when_their_jobs_succeed(self):
        self.assertEqual(sorted(nreq.CONTEXTS), sorted(EXPECTED))
        for name in EXPECTED:
            self.assertEqual(self.verdict(name, ON, self.all_success(name))[0], 0, name)

    def test_every_job_failed_cancelled_missing_or_wrongly_skipped_fails_its_context(self):
        for name in EXPECTED:
            for job in jobs_of(name):
                for result in ("failure", "cancelled", None, "skipped"):
                    rc, out = self.verdict(name, ON, {**self.all_success(name), job: result})
                    self.assertEqual(rc, 1, (name, job, result))
                    self.assertIn(job, out)

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

    def test_changes_failure_unknown_name_or_bad_json_fails(self):
        for changes in ("failure", "cancelled", "skipped"):
            self.assertEqual(self.verdict("Lint", ON, self.all_success("Lint"), changes)[0], 1, changes)
        self.assertEqual(self.verdict("Unit tests", ON, self.all_success("Lint"))[0], 1)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(nci.main(["required-context", "Lint", "--needs-json", "{"]), 1)


HEAD = "b" * 40
ENV = {"HEAD_SHA": HEAD, "GITHUB_REPOSITORY": "o/r", "GITHUB_RUN_ID": "77", "GITHUB_RUN_ATTEMPT": "1", "PINS_OUTCOME": "success"}
EXTERNAL_ID = f"77:1:{HEAD}"
ALL_JOBS = sorted({job for name in EXPECTED for job in jobs_of(name)})


def stamp(second):
    return f"2026-10-26T00:{second // 60:02d}:{second % 60:02d}Z"


T0 = 600  # our runs start 10 minutes into the hour, leaving room for older ones


class FakeChecks:
    """The Checks REST surface the publisher uses; rejects request bodies the real API would not accept."""

    def __init__(self, fail_at=None, fail_after=False, app_id=nreq.ACTIONS_APP_ID, listing=None):
        self.fail_at, self.fail_after, self.app_id = fail_at, fail_after, app_id
        self.listing = listing or (lambda name, own: [own])  # every check run of `name` on the head
        self.calls, self.runs, self.patches = [], {}, []

    def __call__(self, method, path, body=None):
        k = len(self.calls)
        self.calls.append((method, path, body))
        if self.fail_at is not None and (k == self.fail_at or (self.fail_after and k > self.fail_at)):
            raise nreq.PublishError(f"injected failure at call {k}")
        if method == "POST":
            assert path == "repos/o/r/check-runs", path
            assert set(body) == {"name", "head_sha", "status", "external_id", "output"}, body
            assert body["status"] == "in_progress" and set(body["output"]) == {"title", "summary"}, body
            assert all(isinstance(v, str) and v for v in body["output"].values()), body
            run = {**body, "id": 1000 + len(self.runs), "app": {"id": self.app_id}, "started_at": stamp(T0 + len(self.runs))}
            self.runs[run["id"]] = run
            return dict(run)
        if method == "PATCH":
            run = self.runs[int(path.rsplit("/", 1)[1])]
            assert path == f"repos/o/r/check-runs/{run['id']}", path
            assert set(body) == {"status", "conclusion", "output"} and body["status"] == "completed", body
            assert body["conclusion"] in ("success", "failure") and set(body["output"]) == {"title", "summary"}, body
            assert all(isinstance(v, str) and v for v in body["output"].values()), body
            self.patches.append((run["name"], body["conclusion"], body["output"]["title"], body["output"]["summary"]))
            run.update(status="completed", conclusion=body["conclusion"])
            return dict(run)
        assert method == "GET" and body is None, (method, body)
        route, _, query = path.partition("?")
        params = dict(item.split("=", 1) for item in query.split("&"))
        assert route == f"repos/o/r/commits/{HEAD}/check-runs" and params["filter"] == "all", path
        name = unquote(params["check_name"])
        listing = self.listing(name, dict(next(run for run in self.runs.values() if run["name"] == name)))
        page = int(params["page"]) - 1
        return {"total_count": len(listing), "check_runs": listing[page * 100:(page + 1) * 100]}

    def conclusions(self):
        return {name: conclusion for name, conclusion, _, _ in self.patches}


def needs_with(**results):
    outputs = {"result": "success", "outputs": dict(ON)}
    return json.dumps({"changes": outputs, **{job: {"result": results.get(job, "success")} for job in ALL_JOBS
                                             if results.get(job, "success") is not None}})


# gh api double: keeps created check runs in FAKE_GH_STATE and fails every FAKE_GH_FAIL method.
FAKE_GH = f"""#!{sys.executable}
import json, os, sys
argv = sys.argv[1:]
assert argv[:2] == ["api", "-X"] and argv[4:6] == ["-H", "Accept: application/vnd.github+json"], argv
method, path = argv[2], argv[3]
if method == os.environ.get("FAKE_GH_FAIL"):
    sys.exit(1)
state_path = os.environ["FAKE_GH_STATE"]
state = json.load(open(state_path)) if os.path.exists(state_path) else {{"runs": []}}
body = json.load(sys.stdin) if argv[6:] == ["--input", "-"] else None
if method == "POST":
    run = {{**body, "id": len(state["runs"]) + 1, "app": {{"id": 15368}}, "started_at": "2026-10-26T00:00:%02dZ" % len(state["runs"])}}
    state["runs"].append(run)
elif method == "PATCH":
    run = state["runs"][int(path.rsplit("/", 1)[1]) - 1]
    run.update(body)
else:
    name = path.split("check_name=")[1].split("&")[0].replace("%20", " ").replace("%28", "(").replace("%29", ")")
    run = {{"check_runs": [r for r in state["runs"] if r["name"] == name]}}
json.dump(state, open(state_path, "w"))
print(json.dumps(run))
"""


def nci_path():
    return Path(__file__).resolve().parents[1] / "scripts/ci/nextest_ci.py"


class PublishRequiredContexts(unittest.TestCase):
    """Same-repository PRs: one job judges the nine names and publishes them as check runs on the PR head."""

    def publish(self, api=None, needs=None, **env):
        api = api or FakeChecks()
        out = []
        rc = nreq.publish_required_contexts({**ENV, "NEEDS_JSON": needs or needs_with(), **env}, api=api, print_=out.append)
        return rc, api, "\n".join(out)

    def test_all_nine_open_in_progress_on_the_head_then_close_success(self):
        rc, api, _ = self.publish()
        self.assertEqual(rc, 0)
        posts = [body for method, _, body in api.calls if method == "POST"]
        self.assertEqual([body["name"] for body in posts], list(EXPECTED))
        for body in posts:
            self.assertEqual((body["head_sha"], body["status"], body["external_id"]), (HEAD, "in_progress", EXTERNAL_ID))
        self.assertEqual(api.conclusions(), dict.fromkeys(EXPECTED, "success"))
        self.assertEqual(len(api.patches), 9)
        self.assertEqual({title for _, _, title, _ in api.patches}, {f"{name}: PASS" for name in EXPECTED})

    def test_every_red_job_result_fails_its_contexts_with_no_success(self):
        for job in ALL_JOBS:
            for result in ("failure", "cancelled", None, "skipped"):
                with self.subTest(job=job, result=result):
                    rc, api, out = self.publish(needs=needs_with(**{job: result}))
                    self.assertEqual(rc, 1)
                    red = {name for name in EXPECTED if job in jobs_of(name)}
                    self.assertTrue(red)
                    self.assertEqual({n for n, c in api.conclusions().items() if c == "failure"}, red)
                    self.assertEqual(len(api.patches), 9)
                    self.assertIn(f"{min(red)}: FAIL", out)
        for needs in ("{", json.dumps({"changes": {"result": "failure", "outputs": ON}})):
            with self.subTest(needs=needs):
                rc, api, _ = self.publish(needs=needs)
                self.assertEqual((rc, api.conclusions()), (1, dict.fromkeys(EXPECTED, "failure")))

    def test_pin_outcome_other_than_success_fails_the_two_pinned_contexts(self):
        for outcome in ("failure", "cancelled", "skipped", "", None):
            with self.subTest(outcome=outcome):
                env = {"PINS_OUTCOME": outcome} if outcome is not None else {}
                api = FakeChecks()
                rc = nreq.publish_required_contexts({k: v for k, v in {**ENV, "NEEDS_JSON": needs_with(), **env}.items()
                                                     if outcome is not None or k != "PINS_OUTCOME"}, api=api, print_=lambda _: None)
                self.assertEqual(rc, 1)
                self.assertEqual({n for n, c in api.conclusions().items() if c == "failure"},
                                 {"Script checks", "relay-authority-contract"})

    def test_failures_close_before_any_success(self):
        rc, api, _ = self.publish(needs=needs_with(dashboard="failure"))
        self.assertEqual(rc, 1)
        order = [conclusion for _, conclusion, _, _ in api.patches]
        self.assertEqual(order, ["failure"] + ["success"] * 8)
        self.assertEqual(api.patches[0][:3], ("Dashboard (Node 22)", "failure", "Dashboard (Node 22): FAIL"))

    def test_a_created_run_that_is_not_ours_aborts_without_success(self):
        # An untrusted response id is never patched; the context it names stays unpublished and blocks.
        for label, app_id in (("other app", 99), ("no app", None)):
            with self.subTest(label):
                rc, api, out = self.publish(api=FakeChecks(app_id=app_id))
                self.assertEqual((rc, api.patches, [m for m, _, _ in api.calls]), (1, [], ["POST"]))
                self.assertIn("not an Actions check run", out)

    def test_a_close_the_api_did_not_record_is_closed_again_as_failure(self):
        class Unrecorded(FakeChecks):
            def __call__(self, method, path, body=None):
                done = super().__call__(method, path, body)
                return {**done, "conclusion": None} if method == "PATCH" and len(self.patches) == 1 else done
        rc, api, out = self.publish(api=Unrecorded())
        self.assertEqual(rc, 1)
        self.assertEqual([c for _, c, _, _ in api.patches], ["success"] + ["failure"] * 9)
        self.assertEqual(api.patches[1][:3], (api.patches[0][0], "failure", f"{api.patches[0][0]}: FAIL"))
        self.assertIn("did not record success", out)

    def test_an_api_error_at_any_call_leaves_no_success_after_it_and_closes_what_it_opened(self):
        total = len(self.publish()[1].calls)
        self.assertEqual(total, 27)
        for fail_after in (False, True):
            for k in range(total):
                with self.subTest(call=k, fail_after=fail_after):
                    rc, api, _ = self.publish(api=FakeChecks(fail_at=k, fail_after=fail_after))
                    self.assertEqual(rc, 1)
                    later = [body for method, _, body in api.calls[k + 1:] if method == "PATCH"]
                    self.assertFalse([body for body in later if body["conclusion"] == "success"])
                    opened = {1000 + i for i, (method, _, _) in enumerate(api.calls[:k]) if method == "POST"}
                    closes = {int(path.rsplit("/", 1)[1]) for method, path, _ in api.calls[k + 1:] if method == "PATCH"}
                    self.assertLessEqual(closes, opened)
                    if not fail_after:
                        self.assertEqual({run["id"] for run in api.runs.values() if run["status"] != "completed"}, set())
                    self.assertNotEqual(api.conclusions(), dict.fromkeys(EXPECTED, "success"))

    def test_a_run_not_provably_newest_is_never_published_as_success(self):
        def other(seconds_after_own, run_id=5, **override):
            def runs(own):
                started = nreq._started(own).timestamp() + seconds_after_own
                return {"id": run_id, "name": own["name"], "head_sha": HEAD, "external_id": "78:1:" + HEAD,
                        "app": {"id": nreq.ACTIONS_APP_ID}, "started_at": stamp(int(started) % 3600), **override}
            return runs

        def lint(*extra, own=None):
            return lambda name, mine: ([] if own is False else [{**mine, **(own or {})}]) + (
                [run(mine) for run in extra] if name == "Lint" else [])
        rejected = {
            "later run of another attempt": lint(other(30)),
            "same start": lint(other(0)),
            "same external id": lint(other(-1, external_id=EXTERNAL_ID)),
            "other start missing": lint(other(-1, started_at=None)),
            "one of several is later": lint(other(-1), other(5, run_id=6)),
        }
        for label, listing in rejected.items():
            with self.subTest(label):
                rc, api, out = self.publish(api=FakeChecks(listing=listing))
                self.assertEqual(rc, 1)
                self.assertEqual({n for n, c in api.conclusions().items() if c == "failure"}, {"Lint"})
                self.assertIn("is not provably older", out)
        for label, own in (("own run missing", False), ("own start missing", {"started_at": None}),
                           ("own run of another attempt", {"external_id": "77:2:" + HEAD})):
            with self.subTest(label):
                listing = (lambda name, mine: [] if name == "Lint" else [mine]) if own is False else \
                    (lambda name, mine: [{**mine, **own}] if name == "Lint" else [mine])
                rc, api, out = self.publish(api=FakeChecks(listing=listing))
                self.assertEqual({n for n, c in api.conclusions().items() if c == "failure"}, {"Lint"})
                self.assertIn("own check run is missing", out)
        for label, listing in (("older run", lint(other(-1))), ("other app later", lint(other(30, app={"id": 1}))),
                               ("other head later", lint(other(30, head_sha="c" * 40)))):
            with self.subTest(label):
                self.assertEqual(self.publish(api=FakeChecks(listing=listing))[0], 0)

    def test_an_unreadable_or_paged_listing_is_handled(self):
        class Listing(FakeChecks):
            def __init__(self, body):
                super().__init__()
                self.body = body

            def __call__(self, method, path, body=None):
                if method == "GET" and "check_name=Lint&" in path:
                    self.calls.append((method, path, body))
                    if isinstance(self.body, Exception):
                        raise self.body
                    return self.body
                return super().__call__(method, path, body)
        for label, body in (("no list", {}), ("not a list", {"check_runs": {}}), ("not an object", []),
                            ("empty", {"check_runs": []})):
            with self.subTest(label):
                rc, api, _ = self.publish(api=Listing(body))
                self.assertEqual((rc, api.conclusions()["Lint"]), (1, "failure"))
        rc, api, out = self.publish(api=Listing(nreq.PublishError("503")))
        self.assertEqual((rc, api.conclusions()), (1, dict.fromkeys(EXPECTED, "failure")))
        self.assertIn("publisher aborted", api.patches[0][3])
        # Our own run behind 120 older ones is on the second page.
        older = [{"id": 10 + i, "name": "Lint", "head_sha": HEAD, "external_id": "1:1:" + HEAD,
                  "app": {"id": nreq.ACTIONS_APP_ID}, "started_at": "2026-01-01T00:00:00Z"} for i in range(120)]
        rc, api, _ = self.publish(api=FakeChecks(listing=lambda name, mine: (older if name == "Lint" else []) + [mine]))
        self.assertEqual(rc, 0)
        self.assertTrue(any("check_name=Lint&" in path and path.endswith("page=2") for _, path, _ in api.calls))

    def test_the_workflow_command_publishes_through_gh(self):
        with tempfile.TemporaryDirectory() as temp:
            gh = Path(temp) / "gh"
            gh.write_text(FAKE_GH)
            gh.chmod(0o755)
            env = {**os.environ, **ENV, "NEEDS_JSON": needs_with(), "PATH": f"{temp}{os.pathsep}{os.environ['PATH']}",
                   "FAKE_GH_STATE": str(Path(temp) / "state.json")}
            done = subprocess.run([sys.executable, str(nci_path()), "publish-required-contexts"],
                                  env=env, capture_output=True, text=True)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
            runs = json.loads((Path(temp) / "state.json").read_text())["runs"]
            self.assertEqual({run["name"]: run["conclusion"] for run in runs}, dict.fromkeys(EXPECTED, "success"))
            env["FAKE_GH_FAIL"] = "PATCH"
            done = subprocess.run([sys.executable, str(nci_path()), "publish-required-contexts"],
                                  env=env, capture_output=True, text=True)
            self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
            self.assertIn("gh exited 1", done.stdout)

    def test_bad_inputs_make_no_api_call(self):
        for key, value in (("HEAD_SHA", ""), ("HEAD_SHA", "B" * 40), ("HEAD_SHA", "b" * 39), ("GITHUB_REPOSITORY", ""),
                           ("GITHUB_RUN_ID", "x"), ("GITHUB_RUN_ID", ""), ("GITHUB_RUN_ATTEMPT", "")):
            with self.subTest(key=key, value=value):
                rc, api, out = self.publish(**{key: value})
                self.assertEqual((rc, api.calls), (1, []))
                self.assertIn("publisher inputs missing", out)


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
