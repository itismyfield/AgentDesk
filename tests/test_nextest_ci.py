"""Exercise scripts/ci/nextest_ci.py on fixture nextest lists and JUnit reports, without nextest."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import random
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/ci/nextest_ci.py"
spec = importlib.util.spec_from_file_location("nextest_ci", SCRIPT)
nci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nci)
BASE_CONFIG = (ROOT / ".config/nextest.toml").read_text()
SHA = "a" * 40
BIN = "agentdesk"
FLAKY = "voice::flaky_test"


def nextest_list(names, ignored=(), order=None):
    cases = {n: {"ignored": False, "filter-match": {"status": "matches"}} for n in names}
    cases.update({n: {"ignored": True, "filter-match": {"status": "mismatch", "reason": "ignored"}}
                  for n in ignored})
    keys = list(cases)
    if order is not None:
        random.Random(order).shuffle(keys)
    return {"test-count": len(names), "rust-suites": {BIN: {
        "binary-id": BIN, "kind": "lib", "status": "listed",
        "testcases": {k: cases[k] for k in keys}}}}


def junit(cases):
    """cases: (name, body-xml, seconds) triples under one agentdesk testsuite."""
    rows = "".join(f'<testcase name="{n}" classname="{BIN}" time="{s}">{body}</testcase>'
                   for n, body, s in cases)
    return f'<testsuites name="nextest-run"><testsuite name="{BIN}">{rows}</testsuite></testsuites>'


POLICY = f"""version = 1
[[flaky]]
scope = "linux-lib"
binary-id = "{BIN}"
test = "{FLAKY}"
issue = "#1"
expires = 2026-12-31
reason = "fixture"
"""


class Fixture(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="nextest-ci-")
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.archive = self.dir / "lib-tests.tar.zst"
        self.archive.write_bytes(b"archive-bytes")
        (self.dir / "base.toml").write_text(BASE_CONFIG)
        self.write("policy.toml", POLICY)
        self.write("empty.toml", "version = 1\n")

    def write(self, name, text):
        path = self.dir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text if isinstance(text, str) else json.dumps(text))
        return path

    def run_cli(self, *argv):
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            rc = nci.main([str(a) for a in argv])
        return rc, err.getvalue()

    def plan(self, names, workers=2, ignored=(), timings=None, out="plan", policy=None,
             order=None, today="2026-10-24", base="base.toml"):
        self.write("list.json", nextest_list(names, ignored, order))
        policy = policy or ("policy.toml" if FLAKY in names else "empty.toml")
        argv = ["plan", "--universe", self.dir / "list.json", "--archive", self.archive,
                "--workers", workers, "--sha", SHA, "--scope", "linux-lib", "--run-id", "7",
                "--base-config", self.dir / base, "--retry-policy", self.dir / policy,
                "--today", today, "--output", self.dir / out]
        for n, path in enumerate(timings or []):
            argv += ["--timings", self.write(f"t{n}.xml", path)]
        rc, err = self.run_cli(*argv)
        if rc:
            return None, err
        return json.loads((self.dir / out / "plan.json").read_text()), err

    def plan_ok(self, *args, **kw):
        plan, err = self.plan(*args, **kw)
        self.assertIsNotNone(plan, err)
        return plan

    def worker_tests(self, plan, index):
        return [t[1] for t in plan["workers"][index]["tests"]]

    def worker_artifacts(self, plan, results="results", overrides=None):
        """Write passing artifacts for every planned worker; overrides[index] replaces the cases."""
        for w in plan["workers"]:
            i = w["index"]
            rc, err = self.run_cli("verify-archive", "--plan", self.dir / "plan/plan.json", "--worker", i,
                                   "--archive", self.archive, "--config", self.dir / f"plan/worker-{i}.toml",
                                   "--sha", SHA, "--run-id", "7", "--record", self.dir / results / f"worker-{i}")
            self.assertEqual(rc, 0, err)
            cases = (overrides or {}).get(i, [(t[1], "", 0.5) for t in w["tests"]])
            self.write(f"{results}/worker-{i}/exit-code", "0\n")
            self.write(f"{results}/worker-{i}/junit.xml", junit(cases))

    def audit(self, results="results"):
        rc, err = self.run_cli("audit", "--plan", self.dir / "plan/plan.json", "--results", self.dir / results,
                               "--json-out", self.dir / "verdict.json", "--summary-out", self.dir / "summary.md")
        if not (self.dir / "verdict.json").exists():
            return rc, None, err
        return rc, json.loads((self.dir / "verdict.json").read_text()), err

    def kinds(self, verdict):
        return {p["kind"] for p in verdict["problems"]}


class Plan(Fixture):
    NAMES = [f"m::t{i}" for i in range(9)] + [FLAKY]

    def test_same_input_gives_byte_identical_plan_and_configs(self):
        self.plan_ok(self.NAMES, workers=3, out="a", order=1)
        self.plan_ok(self.NAMES, workers=3, out="b", order=2)
        for name in ["plan.json", "worker-0.toml", "worker-1.toml", "worker-2.toml"]:
            self.assertEqual((self.dir / "a" / name).read_bytes(), (self.dir / "b" / name).read_bytes(), name)
        ids = [(BIN, f"m::t{i}") for i in range(9)]
        forward = nci.lpt({t: 1.0 for t in ids}, 3)
        self.assertEqual(nci.lpt({t: 1.0 for t in reversed(ids)}, 3), forward)

    def test_slow_tests_are_spread_by_duration_not_count(self):
        times = {"m::slow": 100, "m::a": 30, "m::b": 30, "m::c": 30, "m::d": 1, "m::e": 1}
        plan = self.plan_ok(list(times), workers=2,
                            timings=[junit([(n, "", s) for n, s in times.items()])])
        self.assertEqual(self.worker_tests(plan, 0), ["m::slow"])
        self.assertEqual(sorted(self.worker_tests(plan, 1)), ["m::a", "m::b", "m::c", "m::d", "m::e"])
        self.assertEqual(plan["timing"]["mode"], "timed")

    def test_unknown_duration_uses_conservative_fallback_and_is_still_planned(self):
        known = {f"m::k{i}": float(i) for i in range(10)}
        plan = self.plan_ok(list(known) + ["m::new"], workers=2,
                            timings=[junit([(n, "", s) for n, s in known.items()])])
        self.assertEqual((plan["timing"]["known"], plan["timing"]["fallback"]), (10, 1))
        self.assertEqual(plan["timing"]["fallback_seconds"], 9.0)
        planned = {t[1] for w in plan["workers"] for t in w["tests"]}
        self.assertIn("m::new", planned)

    def test_no_timing_data_falls_back_to_uniform_weights(self):
        plan = self.plan_ok([f"m::t{i}" for i in range(6)], workers=3)
        self.assertEqual(plan["timing"]["mode"], "uniform")
        self.assertEqual([len(w["tests"]) for w in plan["workers"]], [2, 2, 2])

    def test_invalid_timing_records_are_ignored_not_trusted(self):
        plan = self.plan_ok(["m::a", "m::b"], workers=1,
                            timings=[junit([("m::a", "", "nan"), ("m::b", "", "-3")])])
        self.assertEqual((plan["timing"]["mode"], plan["timing"]["invalid_records"]), ("uniform", 2))

    def test_zero_durations_still_fill_every_worker(self):
        names = [f"m::z{i}" for i in range(7)]
        plan = self.plan_ok(names, workers=3, timings=[junit([(n, "", 0) for n in names])])
        self.assertEqual(sorted(len(w["tests"]) for w in plan["workers"]), [2, 2, 3])

    def test_worker_count_bounds(self):
        one = self.plan_ok(self.NAMES, workers=1)
        self.assertEqual(len(one["workers"][0]["tests"]), len(self.NAMES))
        for workers, needle in [(0, "between 1"), (33, "between 1"), (11, "every worker needs a test")]:
            plan, err = self.plan(self.NAMES, workers=workers)
            self.assertIsNone(plan)
            self.assertIn(needle, err)

    def test_empty_or_ignored_only_universe_is_rejected(self):
        plan, err = self.plan([], ignored=["m::manual"])
        self.assertIsNone(plan)
        self.assertIn("no runnable tests", err)

    def test_old_or_filtered_list_formats_are_rejected(self):
        self.write("list.json", {"rust-suites": {BIN: {"binary-id": BIN, "testcases": ["m::a"]}}})
        with self.assertRaisesRegex(nci.PlanError, "no testcases object"):
            nci.load_universe(self.dir / "list.json")
        self.write("list.json", {"rust-binaries": {}})
        with self.assertRaisesRegex(nci.PlanError, "no rust-suites"):
            nci.load_universe(self.dir / "list.json")
        doc = nextest_list(["m::a", "m::b"])
        doc["rust-suites"][BIN]["testcases"]["m::b"]["filter-match"] = {"status": "mismatch", "reason": "string"}
        self.write("list.json", doc)
        with self.assertRaisesRegex(nci.PlanError, "was filtered"):
            nci.load_universe(self.dir / "list.json")

    def test_worker_config_selects_exactly_its_tests_and_policy_retries(self):
        plan = self.plan_ok(self.NAMES, workers=2)
        import tomllib
        for w in plan["workers"]:
            ci = tomllib.loads((self.dir / f"plan/worker-{w['index']}.toml").read_text())["profile"]["ci"]
            selected = set(ci["default-filter"].split("test(=")[1:])
            self.assertEqual({s.split(")")[0] for s in selected}, {t[1] for t in w["tests"]})
            retry = [o for o in ci["overrides"] if "retries" in o]
            self.assertEqual(retry, [{"filter": f"binary_id(={BIN}) & (test(={FLAKY}))",
                                      "retries": 1, "flaky-result": "pass"}])
            self.assertEqual((ci["retries"], ci["test-threads"], ci["flaky-result"]), (0, 1, "fail"))

    def test_base_config_may_not_carry_retries(self):
        self.write("bad.toml", BASE_CONFIG + '\n[[profile.ci.overrides]]\nfilter = "all()"\nretries = 2\n')
        plan, err = self.plan(self.NAMES, base="bad.toml")
        self.assertIsNone(plan)
        self.assertIn("may not set retries", err)
        self.write("bad.toml", BASE_CONFIG.replace('default-filter = "all()"\nretries = 0', 'default-filter = "all()"\nretries = 1'))
        self.assertNotEqual(BASE_CONFIG, (self.dir / "bad.toml").read_text())
        plan, err = self.plan(self.NAMES, base="bad.toml")
        self.assertIn("profile.ci.retries must be 0", err)

    def test_retry_policy_rejects_expired_stale_ignored_and_duplicate_entries(self):
        cases = [
            ("expired", POLICY, {"today": "2027-01-01"}, "expired on 2026-12-31"),
            ("stale", POLICY.replace(FLAKY, "m::gone"), {}, "absent in the linux-lib list"),
            ("ignored", POLICY.replace(FLAKY, "m::manual"), {"ignored": ["m::manual"]}, "is ignored"),
            ("dup", POLICY + POLICY.split("\n", 1)[1], {}, "twice"),
            ("prefix", POLICY.replace(FLAKY, "voice::*"), {}, "unsupported characters"),
            ("extra-key", POLICY + "retries = 3\n", {}, "keys must be exactly"),
        ]
        for label, text, kw, needle in cases:
            with self.subTest(label):
                self.write("p.toml", text)
                plan, err = self.plan(self.NAMES, policy="p.toml", **kw)
                self.assertIsNone(plan)
                self.assertIn(needle, err)

    def test_policy_entry_for_another_scope_is_not_applied(self):
        self.write("p.toml", POLICY.replace('"linux-lib"', '"windows-lib"').replace(FLAKY, "m::gone"))
        plan = self.plan_ok(self.NAMES, policy="p.toml")
        self.assertEqual(plan["retry_policy"], [])


class Audit(Fixture):
    NAMES = ["m::a", "m::b", "m::c", "m::d", FLAKY]

    def setUp(self):
        super().setUp()
        self.p = self.plan_ok(self.NAMES, workers=2)
        self.flaky_worker = next(w["index"] for w in self.p["workers"] if [BIN, FLAKY] in w["tests"])

    def cases_for(self, index, **replace):
        return [(t[1], replace.get(t[1], ""), 0.5) for t in self.p["workers"][index]["tests"]]

    def test_complete_green_run_passes(self):
        self.worker_artifacts(self.p)
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, verdict["verdict"], verdict["totals"]["executed"]), (0, "pass", 5))

    def test_missing_worker_artifact_is_red(self):
        self.worker_artifacts(self.p)
        for f in (self.dir / "results/worker-1").iterdir():
            f.unlink()
        (self.dir / "results/worker-1").rmdir()
        rc, verdict, _ = self.audit()
        self.assertEqual(rc, 1)
        self.assertIn("missing-artifact", self.kinds(verdict))

    def test_empty_results_directory_is_red(self):
        rc, verdict, _ = self.audit(results="nothing")
        self.assertEqual(rc, 1)
        self.assertEqual(self.kinds(verdict), {"missing-artifact", "zero-tests"})

    def test_planned_but_not_executed_is_missing(self):
        dropped = self.p["workers"][0]["tests"][0][1]
        self.worker_artifacts(self.p, overrides={0: self.cases_for(0)[1:]})
        rc, verdict, _ = self.audit()
        self.assertEqual(rc, 1)
        self.assertIn({"worker": 0, "kind": "missing", "detail": "planned but not executed",
                       "id": f"{BIN} {dropped}"}, verdict["problems"])

    def test_executed_but_not_planned_is_extra(self):
        self.worker_artifacts(self.p, overrides={0: self.cases_for(0) + [("m::stray", "", 0.1)]})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"extra"}))

    def test_unexpected_skip_is_red(self):
        name = self.p["workers"][0]["tests"][0][1]
        self.worker_artifacts(self.p, overrides={0: self.cases_for(0, **{name: "<skipped/>"})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"unexpected-skip", "missing"}))

    def test_zero_testcases_with_exit_zero_is_red(self):
        self.worker_artifacts(self.p, overrides={1: []})
        rc, verdict, _ = self.audit()
        self.assertEqual(rc, 1)
        self.assertIn("zero-tests", self.kinds(verdict))

    def test_nonzero_exit_with_clean_junit_is_red(self):
        self.worker_artifacts(self.p)
        self.write("results/worker-0/exit-code", "100\n")
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"nonzero-exit"}))

    def test_missing_exit_code_or_junit_is_red(self):
        self.worker_artifacts(self.p)
        (self.dir / "results/worker-0/exit-code").unlink()
        (self.dir / "results/worker-1/junit.xml").unlink()
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"missing-exit-code", "missing-junit"}))

    def test_libtest_style_junit_root_is_rejected(self):
        self.worker_artifacts(self.p)
        self.write("results/worker-0/junit.xml", f'<testsuite name="{BIN}"><testcase name="m::a"/></testsuite>')
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"missing-junit"}))

    def test_failed_test_is_red(self):
        name = self.p["workers"][0]["tests"][0][1]
        self.worker_artifacts(self.p, overrides={0: self.cases_for(0, **{name: "<failure/>"})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"failed"}))

    def test_duplicate_testcase_is_red(self):
        self.worker_artifacts(self.p, overrides={0: self.cases_for(0) + self.cases_for(0)[:1]})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"duplicate"}))

    def test_artifact_from_another_run_or_archive_is_red(self):
        self.worker_artifacts(self.p)
        meta = json.loads((self.dir / "results/worker-0/worker.json").read_text())
        self.write("results/worker-0/worker.json", {**meta, "run_id": "6", "archive_sha256": "0" * 64})
        self.write("results/worker-9/junit.xml", junit([]))
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"bad-identity", "unplanned-artifact"}))
        details = " ".join(p["detail"] for p in verdict["problems"])
        self.assertIn("run_id='6'", details)
        self.assertIn("archive_sha256='000", details)

    def test_tampered_plan_is_rejected(self):
        plan_path = self.dir / "plan/plan.json"
        plan = json.loads(plan_path.read_text())
        plan["workers"][0]["tests"].pop()
        plan_path.write_text(json.dumps(plan))
        rc, verdict, err = self.audit()
        self.assertEqual((rc, verdict), (1, None))
        self.assertIn("plan digest", err)

    def test_allowlisted_retry_pass_is_green_and_named_in_summary(self):
        i = self.flaky_worker
        self.worker_artifacts(self.p, overrides={i: self.cases_for(i, **{FLAKY: "<flakyFailure/>"})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, verdict["verdict"]), (0, "pass"))
        self.assertEqual(verdict["retry_pass"], [{"id": f"{BIN} {FLAKY}", "worker": i, "attempts": 2,
                                                  "seconds": 0.5, "issue": "#1", "expires": "2026-12-31"}])
        self.assertIn(f"| `{BIN} {FLAKY}` | 2 | 0.5 | #1 |", (self.dir / "summary.md").read_text())

    def test_exhausted_retry_is_red(self):
        i = self.flaky_worker
        body = "<failure/><rerunFailure/>"
        self.worker_artifacts(self.p, overrides={i: self.cases_for(i, **{FLAKY: body})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"retry-exhausted"}))
        self.assertEqual(verdict["retry_pass"], [])

    def test_retry_outside_allowlist_is_red_even_when_it_passed(self):
        name = next(t[1] for t in self.p["workers"][0]["tests"] if t[1] != FLAKY)
        self.worker_artifacts(self.p, overrides={0: self.cases_for(0, **{name: "<flakyFailure/>"})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"retry-not-allowed"}))

    def test_more_than_one_retry_is_red_even_when_allowlisted(self):
        i = self.flaky_worker
        body = "<flakyFailure/><flakyFailure/>"
        self.worker_artifacts(self.p, overrides={i: self.cases_for(i, **{FLAKY: body})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"retry-over-limit"}))


class VerifyArchive(Fixture):
    def setUp(self):
        super().setUp()
        self.p = self.plan_ok(["m::a", "m::b"], workers=1)

    def verify(self, **kw):
        argv = {"--plan": self.dir / "plan/plan.json", "--worker": 0, "--archive": self.archive,
                "--config": self.dir / "plan/worker-0.toml", "--sha": SHA, "--run-id": "7",
                "--record": self.dir / "rec"}
        argv.update(kw)
        return self.run_cli("verify-archive", *[x for pair in argv.items() for x in pair])

    def test_matching_worker_records_its_identity(self):
        rc, err = self.verify()
        self.assertEqual(rc, 0, err)
        meta = json.loads((self.dir / "rec/worker.json").read_text())
        self.assertEqual((meta["sha"], meta["archive_sha256"]), (SHA, self.p["identity"]["archive_sha256"]))

    def test_mismatched_inputs_are_refused_without_a_record(self):
        other = self.write("other.toml", "x")
        cases = [({"--sha": "b" * 40}, "is not the planned"), ({"--run-id": "8"}, "run 8"),
                 ({"--config": other}, "not the planned worker-0.toml")]
        for kw, needle in cases:
            with self.subTest(needle):
                rc, err = self.verify(**kw)
                self.assertEqual(rc, 1)
                self.assertIn(needle, err)
        self.archive.write_bytes(b"rebuilt")
        rc, err = self.verify()
        self.assertIn("archive digest differs", err)
        self.assertFalse((self.dir / "rec/worker.json").exists())

    def test_cli_retry_override_in_environment_is_refused(self):
        with mock.patch.dict(os.environ, {"NEXTEST_RETRIES": "3"}):
            rc, err = self.verify()
        self.assertEqual(rc, 1)
        self.assertIn("NEXTEST_RETRIES is set", err)


class BaseDiff(Fixture):
    def diff(self, base_sha, *extra, baseline="base/plan.json"):
        return self.run_cli("base-diff", "--plan", self.dir / "plan/plan.json", "--baseline", self.dir / baseline,
                            "--comparison-base-sha", base_sha, "--json-out", self.dir / "diff.json", *extra)

    def test_baseline_from_another_sha_or_missing_is_refused(self):
        self.plan_ok(["m::a", "m::b"], workers=1, out="base")
        self.plan_ok(["m::a", "m::b"], workers=1)
        rc, err = self.diff("c" * 40)
        self.assertEqual(rc, 2)
        self.assertIn(f"need {'c' * 40} linux-lib", err)
        rc, err = self.diff(SHA, baseline="absent.json")
        self.assertEqual(rc, 2)
        self.assertIn("no baseline plan", err)

    def test_removed_and_newly_ignored_tests_fail_unless_report_only(self):
        self.plan_ok(["m::a", "m::b", "m::c"], workers=1, out="base")
        self.plan_ok(["m::a", "m::new"], ignored=["m::b"], workers=1)
        rc, _ = self.diff(SHA)
        diff = json.loads((self.dir / "diff.json").read_text())
        self.assertEqual(rc, 1)
        self.assertEqual((diff["removed"], diff["runnable_to_ignored"], diff["added"]),
                         ([f"{BIN} m::c"], [f"{BIN} m::b"], [f"{BIN} m::new"]))
        self.assertEqual(self.diff(SHA, "--report-only")[0], 0)

    def test_pure_addition_passes(self):
        self.plan_ok(["m::a"], workers=1, out="base")
        self.plan_ok(["m::a", "m::b"], workers=1)
        self.assertEqual(self.diff(SHA)[0], 0)


if __name__ == "__main__":
    unittest.main()
