"""Exercise the nextest CI helpers on fixture lists, JUnit reports and Rust trees, without nextest or cargo."""

import contextlib
import importlib.util
import io
import json
import ntpath
import os
from pathlib import Path
import posixpath
import random
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/ci/nextest_ci.py"
spec = importlib.util.spec_from_file_location("nextest_ci", SCRIPT)
nci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nci)
spec = importlib.util.spec_from_file_location("pg_test_classifier", ROOT / "scripts/ci/pg_test_classifier.py")
pgc = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = pgc  # dataclasses resolve postponed annotations through sys.modules
spec.loader.exec_module(pgc)
spec = importlib.util.spec_from_file_location("rust_test_sources", ROOT / "scripts/ci/rust_test_sources.py")
rts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rts)
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


BUILD = {"--rust-version": "1.94.1", "--nextest-version": "0.9.149", "--target": "x86_64-unknown-linux-gnu"}
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
        """rc and stderr; an uncaught exception is reported as rc "crash" so contracts fail by assertion."""
        err, self.out = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(self.out):
            try:
                rc = nci.main([str(a) for a in argv])
            except Exception as exc:  # noqa: BLE001
                return "crash", f"{err.getvalue()}{type(exc).__name__}: {exc}"
        return rc, err.getvalue()

    def plan(self, names, workers=2, ignored=(), timings=None, out="plan", policy=None,
             order=None, today="2026-10-24", base="base.toml", build=None, sha=SHA, mode=None):
        self.write("list.json", nextest_list(names, ignored, order))
        policy = policy or ("policy.toml" if FLAKY in names else "empty.toml")
        argv = ["plan", "--universe", self.dir / "list.json", "--archive", self.archive,
                "--workers", workers, "--sha", sha, "--scope", "linux-lib", "--run-id", "7",
                "--base-config", self.dir / base, "--retry-policy", self.dir / policy,
                "--today", today, "--output", self.dir / out]
        argv += ["--retry-mode", mode] if mode else []
        build = BUILD if build is None else build
        argv += [x for key, value in build.items() for x in ([key, value] if value is not None else [])]
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

    def test_single_worker_takes_every_test(self):
        one = self.plan_ok(self.NAMES, workers=1)
        self.assertEqual(len(one["workers"][0]["tests"]), len(self.NAMES))

    def test_worker_count_below_one_is_refused(self):
        for workers in (0, -1):
            plan, err = self.plan(self.NAMES, workers=workers)
            self.assertIsNone(plan)
            self.assertIn("between 1 and 32", err)

    def test_worker_count_above_cap_is_refused_even_with_enough_tests(self):
        names = [f"m::t{i}" for i in range(40)]
        self.assertIsNotNone(self.plan(names, workers=32)[0])
        plan, err = self.plan(names, workers=33)
        self.assertIsNone(plan)
        self.assertIn("between 1 and 32", err)

    def test_more_workers_than_tests_is_refused(self):
        plan, err = self.plan(self.NAMES, workers=11)
        self.assertIsNone(plan)
        self.assertIn("every worker needs a test", err)

    def test_build_identity_is_required_and_normalized(self):
        for missing in ("--rust-version", "--nextest-version", "--target"):
            with self.subTest(missing):
                plan, err = self.plan(self.NAMES, build={**BUILD, missing: ""})
                self.assertIsNone(plan)
                self.assertIn(f"needs {missing[2:].replace('-version', '')}", err)
        plan = self.plan_ok(self.NAMES, build={**BUILD, "--features": "b, a,b",
                                               "--build-option": "RUSTFLAGS="})
        self.assertEqual(plan["identity"]["build"], {
            "target": "x86_64-unknown-linux-gnu", "features": ["a", "b"], "cargo_profile": "test",
            "nextest_profile": "ci", "options": ["RUSTFLAGS="]})
        self.assertEqual((plan["identity"]["rust"], plan["identity"]["nextest"]), ("1.94.1", "0.9.149"))
        plan, err = self.plan(self.NAMES, build={**BUILD, "--build-option": "RUSTFLAGS=-C x"})
        self.assertIn("KEY=VALUE", err)
        plan, err = self.plan(self.NAMES, build={**BUILD, "--nextest-profile": "default"})
        self.assertIsNone(plan)
        self.assertIn("generated for profile ci", err)

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
        self.assertEqual(plan["retry_mode"], "policy")

    def test_all_once_config_retries_every_test_once_and_passes_flaky(self):
        plan = self.plan_ok(self.NAMES, workers=2, mode="all-once")
        self.assertEqual((plan["retry_mode"], len(plan["retry_policy"])), ("all-once", 1))
        import tomllib
        for w in plan["workers"]:
            cfg = tomllib.loads((self.dir / f"plan/worker-{w['index']}.toml").read_text())["profile"]
            ci = cfg["ci"]
            selected = {s.split(")")[0] for s in ci["default-filter"].split("test(=")[1:]}
            self.assertEqual(selected, {t[1] for t in w["tests"]})
            self.assertEqual((ci["retries"], ci["flaky-result"], ci["test-threads"]), (1, "pass", 1))
            self.assertEqual([o for o in ci["overrides"] if "retries" in o or "flaky-result" in o], [])
            self.assertEqual(cfg["default"]["retries"], 0)

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

    def test_null_or_non_object_worker_identity_is_red(self):
        for text in ("null", "[]", '"worker"'):
            with self.subTest(text):
                self.worker_artifacts(self.p)
                self.write("results/worker-0/worker.json", text)
                rc, verdict, _ = self.audit()
                self.assertEqual((rc, self.kinds(verdict)), (1, {"bad-identity"}))

    def test_retry_time_is_summed_apart_from_wall_time(self):
        i = self.flaky_worker
        cases = [(n, '<flakyFailure time="120"/>' if n == FLAKY else "", 1 if n == FLAKY else 0.5)
                 for n in self.worker_tests(self.p, i)]
        self.worker_artifacts(self.p, overrides={i: cases})
        rc, verdict, _ = self.audit()
        self.assertEqual(rc, 0)
        self.assertEqual((verdict["retry_pass"][0]["seconds"], verdict["retry_pass"][0]["total_attempt_seconds"]),
                         (1.0, 121.0))
        row = verdict["workers"][i]
        self.assertEqual(row["total_attempt_seconds"] - row["seconds"], 120.0)
        timings, invalid = nci.load_timings([self.write("t.xml", junit(cases))])
        self.assertEqual((timings[(BIN, FLAKY)], invalid), (121.0, 0))

    def test_unreadable_retry_time_is_red(self):
        i = self.flaky_worker
        cases = [(n, '<flakyFailure time="x"/>' if n == FLAKY else "", 0.5) for n in self.worker_tests(self.p, i)]
        self.worker_artifacts(self.p, overrides={i: cases})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"bad-time"}))

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
        self.worker_artifacts(self.p, overrides={i: self.cases_for(i, **{FLAKY: '<flakyFailure time="0.1"/>'})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, verdict["verdict"]), (0, "pass"))
        self.assertEqual(verdict["retry_pass"], [{"id": f"{BIN} {FLAKY}", "worker": i, "attempts": 2,
                                                  "seconds": 0.5, "total_attempt_seconds": 0.6,
                                                  "issue": "#1", "expires": "2026-12-31"}])
        self.assertIn(f"| `{BIN} {FLAKY}` | 2 | 0.5 | 0.6 | #1 |", (self.dir / "summary.md").read_text())
        self.assertIn(f"::warning title=FLAKY tests passed only on retry (1)::{BIN} {FLAKY}\n", self.out.getvalue())

    def test_exhausted_retry_is_red(self):
        i = self.flaky_worker
        body = '<failure/><rerunFailure time="0.1"/>'
        self.worker_artifacts(self.p, overrides={i: self.cases_for(i, **{FLAKY: body})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"retry-exhausted"}))
        self.assertEqual(verdict["retry_pass"], [])

    def test_retry_outside_allowlist_is_red_even_when_it_passed(self):
        name = next(t[1] for t in self.p["workers"][0]["tests"] if t[1] != FLAKY)
        self.worker_artifacts(self.p, overrides={0: self.cases_for(0, **{name: '<flakyFailure time="0.1"/>'})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"retry-not-allowed"}))

    def test_more_than_one_retry_is_red_even_when_allowlisted(self):
        i = self.flaky_worker
        body = '<flakyFailure time="0.1"/><flakyFailure time="0.1"/>'
        self.worker_artifacts(self.p, overrides={i: self.cases_for(i, **{FLAKY: body})})
        rc, verdict, _ = self.audit()
        self.assertEqual((rc, self.kinds(verdict)), (1, {"retry-over-limit"}))


class AuditAllOnce(Fixture):
    """PR and main plans: any test may pass on its one retry, and is named as FLAKY."""
    NAMES = ["m::a", "m::b", "m::c", "m::d", FLAKY]

    def setUp(self):
        super().setUp()
        self.p = self.plan_ok(self.NAMES, workers=2, mode="all-once")

    def with_bodies(self, **bodies):
        overrides = {w["index"]: [(t[1], bodies.get(t[1].replace("::", "_"), ""), 0.5) for t in w["tests"]]
                     for w in self.p["workers"]}
        self.worker_artifacts(self.p, overrides=overrides)
        return self.audit()

    def test_retry_pass_of_any_test_is_green_counted_once_and_annotated(self):
        rc, verdict, _ = self.with_bodies(m_a='<flakyFailure time="0.1"/>', m_c='<flakyFailure time="0.2"/>')
        self.assertEqual((rc, verdict["verdict"], verdict["retry_mode"]), (0, "pass", "all-once"))
        self.assertEqual((verdict["totals"]["executed"], verdict["totals"]["passed"]), (5, 5))
        self.assertEqual(verdict["totals"]["retry_pass"], 2)
        self.assertEqual([(r["id"], r["attempts"], r["issue"]) for r in verdict["retry_pass"]],
                         [(f"{BIN} m::a", 2, None), (f"{BIN} m::c", 2, None)])
        self.assertIn(f"| `{BIN} m::a` | 2 | 0.5 | 0.6 | - | - |", (self.dir / "summary.md").read_text())
        self.assertIn(f"::warning title=FLAKY tests passed only on retry (2)::{BIN} m::a%0A{BIN} m::c\n",
                      self.out.getvalue())

    def test_failure_on_both_attempts_is_still_red(self):
        rc, verdict, _ = self.with_bodies(m_a='<failure/><rerunFailure time="0.1"/>')
        self.assertEqual((rc, self.kinds(verdict), verdict["retry_pass"]), (1, {"retry-exhausted"}, []))
        self.assertNotIn("::warning", self.out.getvalue())

    def test_more_than_one_retry_is_still_red(self):
        rc, verdict, _ = self.with_bodies(m_a='<flakyFailure time="0.1"/><flakyFailure time="0.1"/>')
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


def source_losses(removed=None, ignored=None, present=None):
    """rust_test_sources.py removed JSON: per-platform removed and runnable->ignored names."""
    empty = {"linux": [], "macos": [], "windows": []}
    doc = {"removed": {**empty, **(removed or {})}, "runnable_to_ignored": {**empty, **(ignored or {})}}
    if present is not None:
        doc["present_tests"] = {**empty, **present}
    return doc


class BaseDiff(Fixture):
    def diff(self, base_sha, baseline="base/plan.json", rows="", source=None):
        self.write("justify.txt", rows)
        self.write("source-removed.json", source or source_losses())
        return self.run_cli("base-diff", "--plan", self.dir / "plan/plan.json", "--baseline", self.dir / baseline,
                            "--comparison-base-sha", base_sha, "--justifications", self.dir / "justify.txt",
                            "--source-removed", self.dir / "source-removed.json", "--json-out", self.dir / "diff.json")

    def test_baseline_from_another_sha_or_missing_is_refused(self):
        self.plan_ok(["m::a", "m::b"], workers=1, out="base")
        self.plan_ok(["m::a", "m::b"], workers=1)
        rc, err = self.diff("c" * 40)
        self.assertEqual(rc, 2)
        self.assertIn(f"need {'c' * 40} linux-lib", err)
        rc, err = self.diff(SHA, baseline="absent.json")
        self.assertEqual(rc, 2)
        self.assertIn("no baseline plan", err)

    def test_baseline_with_other_build_under_same_scope_is_refused(self):
        self.plan_ok(["m::a", "m::b"], workers=1, out="base", build={**BUILD, "--features": "default,extra"})
        self.plan_ok(["m::a", "m::b"], workers=1)
        rc, err = self.diff(SHA)
        self.assertEqual(rc, 2)
        self.assertIn("under the same scope", err)
        self.plan_ok(["m::a", "m::b"], workers=1, out="base", build={**BUILD, "--rust-version": "1.95.0"})
        self.assertEqual(self.diff(SHA)[0], 0)

    def test_removed_and_newly_ignored_tests_fail_without_rows(self):
        self.plan_ok(["m::a", "m::b", "m::c"], workers=1, out="base")
        self.plan_ok(["m::a", "m::new"], ignored=["m::b"], workers=1)
        rc, err = self.diff(SHA, source=source_losses(present={}))
        diff = json.loads((self.dir / "diff.json").read_text())
        self.assertEqual(rc, 1)
        self.assertEqual((diff["removed"], diff["runnable_to_ignored"], diff["added"]),
                         ([f"{BIN} m::c"], [f"{BIN} m::b"], [f"{BIN} m::new"]))
        self.assertEqual(diff["unjustified"], [f"{BIN} m::b | runnable->ignored", f"{BIN} m::c | removed"])
        self.assertIn(f"lost without a justification row: {BIN} m::c | removed", err)

    def test_landed_removal_rows_warn_with_full_row_and_do_not_fail(self):
        self.plan_ok(["m::a"], workers=1, out="base")
        self.plan_ok(["m::a"], workers=1)
        rows = (f"{BIN} m::landed | removed | merged into m::a\n"
                f"{BIN} m::typo | runnable->ignored | visibly misspelled row\n")
        rc, err = self.diff(SHA, rows=rows, source=source_losses(present={}))
        self.assertEqual(rc, 0, err)
        self.assertNotIn("ERROR", err)
        warnings = err.splitlines()
        self.assertEqual(len(warnings), 2)
        for row in rows.splitlines():
            self.assertTrue(any(line.startswith("WARN ") and line.endswith(row) for line in warnings), err)
        diff = json.loads((self.dir / "diff.json").read_text())
        self.assertEqual(diff["landed_justifications"],
                         [f"{BIN} m::landed | removed", f"{BIN} m::typo | runnable->ignored"])
        self.assertEqual(diff["stale_justifications"], diff["landed_justifications"])
        self.assertEqual(diff["unjustified"], [])

    def test_native_presence_or_incomplete_evidence_keeps_rows_errors(self):
        self.plan_ok(["m::a"], workers=1, out="base")
        self.plan_ok(["m::a"], workers=1)
        sources = [source_losses(), source_losses(present={"macos": ["m::native"]}),
                   source_losses(present={"windows": ["m::native"]})]
        for evidence in ({"macos": []}, {"macos": [], "windows": None},
                         {"macos": [], "windows": [False]}):
            sources.append({**source_losses(), "present_tests": evidence})
        for source in sources:
            with self.subTest(source=source):
                rc, err = self.diff(SHA, rows=f"{BIN} m::native | removed | native row\n", source=source)
                self.assertEqual(rc, 1, err)
                self.assertIn(f"matches no loss against {SHA}: {BIN} m::native | removed", err)
                self.assertEqual(json.loads((self.dir / "diff.json").read_text())["landed_justifications"], [])

    def test_base_ignored_or_head_only_presence_keeps_rows_errors(self):
        self.plan_ok(["m::a"], ignored=["m::ignored"], workers=1, out="base")
        self.plan_ok(["m::a", "m::new"], ignored=["m::ignored"], workers=1)
        for name in ("m::ignored", "m::new"):
            with self.subTest(name=name):
                rc, err = self.diff(SHA, rows=f"{BIN} {name} | removed | still here\n",
                                    source=source_losses(present={}))
                self.assertEqual(rc, 1, err)
                self.assertIn(f"matches no loss against {SHA}: {BIN} {name} | removed", err)

    def test_rows_matching_each_loss_exactly_pass(self):
        self.plan_ok(["m::a", "m::b", "m::c"], workers=1, out="base")
        self.plan_ok(["m::a"], ignored=["m::b"], workers=1)
        rows = f"# intended\n{BIN} m::c | removed | merged into m::a\n{BIN} m::b | runnable->ignored | needs a GPU\n"
        self.assertEqual(self.diff(SHA, rows=rows), (0, ""))

    def test_stale_or_wrong_kind_rows_fail(self):
        self.plan_ok(["m::a", "m::b"], workers=1, out="base")
        self.plan_ok(["m::a"], workers=1)
        cases = ((f"{BIN} m::b | removed | gone\n{BIN} m::a | removed | still here\n", f"{BIN} m::a | removed"),
                 (f"{BIN} m::b | runnable->ignored | wrong kind\n", f"{BIN} m::b | runnable->ignored"))
        for rows, stale in cases:
            with self.subTest(rows=rows):
                rc, err = self.diff(SHA, rows=rows, source=source_losses(present={}))
                self.assertEqual(rc, 1)
                self.assertIn(f"matches no loss against {SHA}: {stale}", err)
        for rows in ("m::b | removed | no binary\n", f"{BIN} m::b | deleted | bad kind\n", f"{BIN} m::b | removed |\n",
                     f"{BIN} m::b | removed | a\n{BIN} m::b | removed | b\n"):
            with self.subTest(rows=rows):
                self.assertEqual(self.diff(SHA, rows=rows)[0], 1)

    def test_tests_only_macos_or_windows_compiled_need_rows_too(self):
        self.plan_ok(["m::a"], workers=1, out="base")
        self.plan_ok(["m::a"], workers=1)
        source = source_losses(removed={"macos": ["m::mac"], "windows": ["m::win", "m::mac"]})
        rc, err = self.diff(SHA, source=source)
        self.assertEqual(rc, 1)
        self.assertEqual(json.loads((self.dir / "diff.json").read_text())["platform_losses"],
                         {f"{BIN} m::mac | removed": ["macos", "windows"], f"{BIN} m::win | removed": ["windows"]})
        rows = f"{BIN} m::mac | removed | macOS API retired\n{BIN} m::win | removed | Win32 path retired\n"
        self.assertEqual(self.diff(SHA, rows=rows, source=source), (0, ""))

    def test_native_only_tests_newly_ignored_need_a_runnable_to_ignored_row(self):
        self.plan_ok(["m::a"], workers=1, out="base")
        self.plan_ok(["m::a"], workers=1)
        source = source_losses(ignored={"windows": ["m::win"], "macos": ["m::mac"]})
        rc, err = self.diff(SHA, source=source)
        self.assertEqual(rc, 1)
        self.assertIn(f"lost without a justification row: {BIN} m::win | runnable->ignored", err)
        exact = f"{BIN} m::win | runnable->ignored | flaky on hosted Windows\n{BIN} m::mac | runnable->ignored | x\n"
        self.assertEqual(self.diff(SHA, rows=exact, source=source), (0, ""))
        wrong_kind = exact.replace("m::win | runnable->ignored", "m::win | removed")
        rc, err = self.diff(SHA, rows=wrong_kind, source=source)
        self.assertEqual(rc, 1)
        self.assertIn(f"{BIN} m::win | runnable->ignored", err)
        self.assertIn(f"matches no loss against {SHA}: {BIN} m::win | removed", err)
        rc, err = self.diff(SHA, rows=exact, source=source_losses())
        self.assertEqual(rc, 1)
        self.assertIn(f"matches no loss against {SHA}: {BIN} m::win | runnable->ignored", err)

    def test_pure_addition_passes(self):
        self.plan_ok(["m::a"], workers=1, out="base")
        self.plan_ok(["m::a", "m::b"], workers=1)
        self.assertEqual(self.diff(SHA)[0], 0)


class NativePlan(Fixture):
    """Native macOS/Windows runs plan only the classifier's non-PG tests; a base rebuild copies a plan's identity."""

    def test_only_plans_the_non_pg_names(self):
        self.write("pg.json", {"non_pg": ["m::a", "m::ign"]})
        plan = self.plan_ok(["m::a", "m::pg"], ignored=["m::ign"], workers=1,
                            build={**BUILD, "--only": self.dir / "pg.json"})
        self.assertEqual((self.worker_tests(plan, 0), plan["ignored"]), (["m::a"], [[BIN, "m::ign"]]))
        self.write("pg.json", {"non_pg": ["m::a", "m::gone"]})
        self.assertIn("outside the listed universe", self.plan(["m::a"], workers=1,
                                                              build={**BUILD, "--only": self.dir / "pg.json"})[1])

    def test_identity_from_copies_scope_and_build(self):
        head = self.plan_ok(["m::a"], workers=1, build={**BUILD, "--features": "default,extra"})
        rc, err = self.run_cli("plan", "--universe", self.dir / "list.json", "--archive", self.archive, "--workers", 1,
                               "--sha", "b" * 40, "--identity-from", self.dir / "plan/plan.json",
                               "--base-config", self.dir / "base.toml", "--retry-policy", self.dir / "empty.toml",
                               "--output", self.dir / "rebuilt")
        self.assertEqual(rc, 0, err)
        rebuilt = json.loads((self.dir / "rebuilt/plan.json").read_text())["identity"]
        drop = ("sha", "archive_sha256", "run_id")
        self.assertEqual({k: v for k, v in rebuilt.items() if k not in drop},
                         {k: v for k, v in head["identity"].items() if k not in drop})


class ScriptRepo(Fixture):
    """A git checkout holding the CI scripts, with gh and cargo stubbed by the subclass texts."""

    LIB = "#[test]\nfn a() {}\n#[cfg(windows)]\n#[test]\nfn w() {}\n"
    GH = """#!/bin/sh
case "$1 $2" in
  "run list") [ -n "$GH_RUN_ID" ] && echo "$GH_RUN_ID"; exit 0 ;;
  "run download") shift 2; while [ "$1" != "--dir" ]; do shift; done
    [ -n "$GH_BASELINE" ] && mkdir -p "$2" && cp "$GH_BASELINE" "$2/plan.json" ;;
  *) exit 9 ;;
esac
"""
    CARGO = """#!/bin/sh
echo "$*" >> "$CARGO_LOG"
[ "$2" = --version ] && { echo "cargo-nextest 0.9.149"; exit 0; }
[ -z "$CARGO_FAIL" ] || exit 101
if [ "$2" = archive ]; then while [ "$1" != --archive-file ]; do shift; done; echo bytes > "$2"; exit 0; fi
[ "$2" = list ] && cat "$BASE_LIST"
"""

    def setUp(self):
        super().setUp()
        import shutil
        import subprocess
        self.sp = subprocess
        self.repo = self.dir / "repo"
        for rel in ("scripts/ci/nextest_ci.py", "scripts/ci/nextest_required.py", "scripts/ci/rust_test_sources.py",
                    "scripts/ci/nextest-base-diff.sh", ".config/nextest.toml",
                    "scripts/ci/nextest-native.sh", "scripts/ci/pg_test_classifier.py", "scripts/ci/nextest-retry-policy.toml"):
            (self.repo / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / rel, self.repo / rel)
        # Only the header: the rows a real change carries match no loss in this fixture.
        rows = (ROOT / "scripts/ci/test-removal-justifications.txt").read_text().splitlines(keepends=True)
        (self.repo / "scripts/ci/test-removal-justifications.txt").write_text("".join(r for r in rows if r.startswith("#")))
        (self.repo / "Cargo.toml").write_text('[package]\nname = "agentdesk"\n')
        (self.repo / "src").mkdir()
        self.base_sha = self.commit(self.LIB, "base")
        self.git("update-ref", "refs/remotes/origin/main", self.base_sha)
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        for name, text in (("gh", self.GH), ("cargo", self.CARGO)):
            (self.bin / name).write_text(text)
            (self.bin / name).chmod(0o755)
        self.write("base-list.json", nextest_list(["m::a"]))

    def git(self, *argv):
        return self.sp.run(["git", "-C", str(self.repo), *argv], check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, lib, message):
        (self.repo / "src/lib.rs").write_text(lib)
        if not (self.repo / ".git").exists():
            self.git("init", "-q")
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def head_plan(self, names, lib=None):
        head = self.commit(lib or self.LIB + "#[test]\nfn b() {}\n", "head")
        self.plan_ok(names, workers=1, out="nextest/plan", sha=head)
        return head

    def baseline(self, names, **kw):
        self.plan_ok(names, workers=1, out="main", sha=kw.pop("sha", self.base_sha), **kw)
        return self.dir / "main/plan.json"

    def run_script(self, head, **env):
        summary = self.dir / "summary.md"
        summary.write_text("")
        full = {**os.environ, "PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}", "PR_HEAD_SHA": head,
                "GITHUB_REPOSITORY": "o/r", "GITHUB_WORKSPACE": str(self.repo), "GITHUB_STEP_SUMMARY": str(summary),
                "CARGO_LOG": str(self.dir / "cargo.log"), "BASE_LIST": str(self.dir / "base-list.json"),
                "GH_RUN_ID": "", "GH_BASELINE": "", "CARGO_FAIL": "", **env}
        full.pop("GITHUB_OUTPUT", None)
        done = self.sp.run(["bash", "scripts/ci/nextest-base-diff.sh", str(self.dir / "nextest")], cwd=self.repo,
                           env=full, capture_output=True, text=True)
        cargo = (self.dir / "cargo.log").read_text() if (self.dir / "cargo.log").exists() else ""
        return done.returncode, done.stdout + done.stderr, summary.read_text(), cargo



class BaseDiffScript(ScriptRepo):
    """scripts/ci/nextest-base-diff.sh: CI Main's baseline for the exact base SHA, or a rebuild."""

    def test_main_baseline_is_used_when_it_matches(self):
        head = self.head_plan(["m::a", "m::b"])
        row = f"{BIN} m::landed | removed | removed by the preceding PR"
        (self.repo / "scripts/ci/test-removal-justifications.txt").write_text(row + "\n")
        rc, out, summary, cargo = self.run_script(head, GH_RUN_ID="5", GH_BASELINE=str(self.baseline(["m::a"])))
        self.assertEqual(rc, 0, out)
        self.assertIn("WARN ", out)
        self.assertIn(row, out)
        self.assertIn("against CI Main's baseline", summary)
        self.assertNotIn("archive", cargo)

    def test_missing_or_mismatched_baseline_rebuilds_the_base_plan(self):
        head = self.head_plan(["m::a", "m::b"])
        cases = ({}, {"GH_RUN_ID": "5", "GH_BASELINE": str(self.baseline(["m::a"], sha="c" * 40))},
                 {"GH_RUN_ID": "5", "GH_BASELINE": str(self.baseline(["m::a"], build={**BUILD, "--features": "x"}))})
        for env in cases:
            with self.subTest(env=env):
                (self.dir / "cargo.log").unlink(missing_ok=True)
                self.sp.run(["rm", "-rf", str(self.dir / "nextest/base")], check=True)
                self.git("worktree", "prune")
                rc, out, summary, cargo = self.run_script(head, **env)
                self.assertEqual(rc, 0, out)
                self.assertIn("::warning title=nextest baseline fallback::", out)
                self.assertIn("nextest baseline fallback:", summary)
                self.assertIn("against the fallback base plan", summary)
                self.assertIn("nextest archive --lib", cargo)

    def test_loss_fails_on_either_path_and_a_failed_rebuild_never_passes(self):
        head = self.head_plan(["m::b"])
        rc, out, _, _ = self.run_script(head, GH_RUN_ID="5", GH_BASELINE=str(self.baseline(["m::a"])))
        self.assertEqual(rc, 1)
        self.assertIn(f"lost without a justification row: {BIN} m::a | removed", out)
        self.sp.run(["rm", "-rf", str(self.dir / "nextest/base")], check=True)
        self.git("worktree", "prune")
        rc, out, summary, _ = self.run_script(head, CARGO_FAIL="1")
        self.assertNotEqual(rc, 0)
        self.assertNotIn("against the fallback base plan", summary)

    def test_windows_only_source_removal_needs_a_row(self):
        head = self.head_plan(["m::a"], lib="#[test]\nfn a() {}\n")
        rc, out, _, _ = self.run_script(head, GH_RUN_ID="5", GH_BASELINE=str(self.baseline(["m::a"])))
        self.assertEqual(rc, 1)
        self.assertIn(f"lost without a justification row: {BIN} w | removed", out)

    def test_windows_only_test_gaining_ignore_needs_its_exact_row(self):
        head = self.head_plan(["m::a"], lib=self.LIB.replace("fn w()", "#[ignore = \"temporarily disabled\"]\nfn w()"))
        baseline = str(self.baseline(["m::a"]))
        rc, out, _, _ = self.run_script(head, GH_RUN_ID="5", GH_BASELINE=baseline)
        self.assertEqual(rc, 1)
        self.assertIn(f"lost without a justification row: {BIN} w | runnable->ignored", out)
        rows = self.repo / "scripts/ci/test-removal-justifications.txt"
        rows.write_text(f"{BIN} w | runnable->ignored | Win32 lock flaky on hosted runners\n")
        self.sp.run(["rm", "-rf", str(self.dir / "nextest/base")], check=True)
        self.git("worktree", "prune")
        rc, out, _, _ = self.run_script(head, GH_RUN_ID="5", GH_BASELINE=baseline)
        self.assertEqual(rc, 0, out)


class NativeScript(ScriptRepo):
    """scripts/ci/nextest-native.sh with cargo stubbed: the plan, run record and audit decide, not the exit code."""

    LIB = "mod m {\n#[test]\nfn a() {}\n#[test]\nfn health_b() {}\n#[test]\nfn health_pg_c() {}\n}\n"
    CARGO = """#!/bin/sh
echo "$*" >> "$NATIVE_CARGO_LOG"
case "$2" in
  --version) echo "cargo-nextest 0.9.149" ;;
  archive) while [ "$1" != --archive-file ]; do shift; done; echo bytes > "$2" ;;
  list) cat "$NATIVE_LIST" ;;
  run) while [ "$1" != --extract-to ]; do shift; done; mkdir -p "$2/target/nextest/ci"
       cp "$NATIVE_JUNIT" "$2/target/nextest/ci/junit.xml"; exit "${NATIVE_EXIT:-0}" ;;
esac
"""

    def setUp(self):
        super().setUp()
        (self.bin / "rustc").write_text("#!/bin/sh\nprintf 'host: %s\\n' \"$NATIVE_TARGET\"\n")
        (self.bin / "rustc").chmod(0o755)
        self.write("native-list.json", nextest_list(["m::a", "m::health_b", "m::health_pg_c"]))
        # The real exemptions name repository tests; this tree has none of them.
        classifier = self.repo / "scripts/ci/pg_test_classifier.py"
        text = classifier.read_text()
        start = text.index("EXEMPT = {")
        classifier.write_text(text[:start] + "EXEMPT = {}\n_UNUSED_EXEMPT = {" + text[start + len("EXEMPT = {"):])

    def native(self, ran, exit_code=0, phase=None, reset=True, platform="macos"):
        import shutil
        self.write("junit.xml", junit([(n, "", 0.1) for n in ran]))
        temp = self.dir / "runner"
        if reset:
            shutil.rmtree(temp, ignore_errors=True)
            temp.mkdir()
        env = {**os.environ, "PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}", "RUNNER_TEMP": str(temp),
               "GITHUB_WORKSPACE": str(self.repo), "GITHUB_STEP_SUMMARY": str(self.dir / "summary.md"),
               "NATIVE_LIST": str(self.dir / "native-list.json"), "NATIVE_JUNIT": str(self.dir / "junit.xml"),
               "NATIVE_EXIT": str(exit_code), "GITHUB_RUN_ID": "9", "NATIVE_CARGO_LOG": str(temp / "cargo.log"),
               "NATIVE_TARGET": "x86_64-pc-windows-msvc" if platform == "windows" else "aarch64-apple-darwin"}
        command = ["bash", "scripts/ci/nextest-native.sh", platform, f"{platform}-lib", "pg-classifier"]
        if phase is not None:
            command.append(phase)
        done = self.sp.run(command,
                           cwd=self.repo, env=env, capture_output=True, text=True)
        plan = temp / "nextest/plan/plan.json"
        tests = [t[1] for t in json.loads(plan.read_text())["workers"][0]["tests"]] if plan.exists() else None
        return done.returncode, done.stdout + done.stderr, tests

    def test_pg_classifier_scope_plans_every_non_pg_test_and_passes_only_when_all_ran(self):
        rc, out, tests = self.native(["m::a", "m::health_b", "m::health_pg_c"])
        self.assertEqual((rc, tests), (0, ["m::a", "m::health_b", "m::health_pg_c"]), out)
        self.assertIn("## nextest macos-lib: PASS", (self.dir / "summary.md").read_text())

    def test_a_planned_test_missing_from_the_run_fails_despite_exit_zero(self):
        rc, out, tests = self.native(["m::a", "m::health_b"])
        self.assertEqual(rc, 1, out)
        self.assertEqual(tests, ["m::a", "m::health_b", "m::health_pg_c"])

    def test_a_source_test_missing_from_the_native_list_fails_before_running(self):
        self.write("native-list.json", nextest_list(["m::a", "m::health_b"]))
        rc, out, tests = self.native(["m::a", "m::health_b"])
        self.assertEqual((rc, tests), (1, None), out)
        self.assertIn("static_only 1", out)

    def test_a_nonzero_run_exit_fails_even_with_every_test_passing(self):
        rc, out, _ = self.native(["m::a", "m::health_b", "m::health_pg_c"], exit_code=100)
        self.assertEqual(rc, 1, out)
        self.assertIn("nextest exited 100", out)

    def test_prepare_then_run_uses_the_same_plan_on_both_native_platforms(self):
        for platform in ("macos", "windows"):
            with self.subTest(platform=platform):
                rc, out, tests = self.native(["m::a", "m::health_b", "m::health_pg_c"],
                                             phase="prepare", platform=platform)
                self.assertEqual(rc, 0, out)
                self.assertEqual(tests, ["m::a", "m::health_b", "m::health_pg_c"])
                directory = self.dir / "runner/nextest"
                files = ("plan/plan.json", "plan/worker-0.toml", "results/worker-0/worker.json")
                prepared = {name: (directory / name).read_bytes() for name in files}
                self.assertFalse((directory / "results/worker-0/exit-code").exists())
                self.assertFalse((directory / "verdict.json").exists())
                rc, out, _ = self.native(tests, phase="run", reset=False, platform=platform)
                self.assertEqual(rc, 0, out)
                self.assertEqual(prepared, {name: (directory / name).read_bytes() for name in files})
                verdict = json.loads((directory / "verdict.json").read_text())
                self.assertEqual((verdict["verdict"], verdict["totals"]["executed"]), ("pass", 3))
                calls = (self.dir / "runner/cargo.log").read_text().splitlines()
                for operation in ("archive", "list", "run"):
                    self.assertEqual(sum(line.startswith(f"nextest {operation} ") for line in calls), 1)

    def test_prepare_without_run_fails_execution_audit(self):
        rc, out, _ = self.native(["m::a", "m::health_b", "m::health_pg_c"], phase="prepare")
        self.assertEqual(rc, 0, out)
        directory = self.dir / "runner/nextest"
        verdict = nci.audit(nci.load_plan(directory / "plan/plan.json"), directory / "results")
        self.assertEqual((verdict["verdict"], verdict["totals"]["executed"]), ("fail", 0))
        self.assertLessEqual({"missing-exit-code", "missing-junit"}, {p["kind"] for p in verdict["problems"]})

    def test_run_without_prepared_plan_fails_before_starting_tests(self):
        rc, out, _ = self.native(["m::a", "m::health_b", "m::health_pg_c"], phase="run")
        self.assertNotEqual(rc, 0, out)
        self.assertFalse((self.dir / "runner/cargo.log").exists())

    def test_run_rechecks_the_archive_across_the_step_boundary(self):
        tests = ["m::a", "m::health_b", "m::health_pg_c"]
        rc, out, _ = self.native(tests, phase="prepare")
        self.assertEqual(rc, 0, out)
        (self.dir / "runner/nextest/lib-tests.tar.zst").write_bytes(b"changed between steps")
        rc, out, _ = self.native(tests, phase="run", reset=False)
        self.assertNotEqual(rc, 0, out)
        self.assertIn("archive digest differs", out)
        self.assertFalse(any(line.startswith("nextest run ")
                             for line in (self.dir / "runner/cargo.log").read_text().splitlines()))

    def test_split_run_still_fails_when_a_planned_test_did_not_run(self):
        rc, out, _ = self.native(["m::a", "m::health_b", "m::health_pg_c"], phase="prepare")
        self.assertEqual(rc, 0, out)
        rc, out, _ = self.native(["m::a", "m::health_b"], phase="run", reset=False)
        self.assertNotEqual(rc, 0, out)
        verdict = json.loads((self.dir / "runner/nextest/verdict.json").read_text())
        self.assertEqual((verdict["verdict"], verdict["totals"]["planned"], verdict["totals"]["executed"]),
                         ("fail", 3, 2))


class RustSources(unittest.TestCase):
    """The source walker: what a crate root reaches and on which platform each test compiles."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="rust-sources-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.tree({"Cargo.toml": '[package]\nname = "x"\n[features]\ndefault = ["voice"]\nvoice = []\nextra = []\n'})

    def tree(self, files):
        for rel, text in files.items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)

    def cli(self, *argv):
        err, out = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
            try:
                rc = rts.main(["--repo-root", str(self.root), *map(str, argv)])
            except Exception as exc:  # noqa: BLE001
                return "crash", f"{type(exc).__name__}: {exc}"
        return rc, err.getvalue()

    def split(self, os_name):
        inv = rts.lib_inventory(self.root)
        return rts.platform_split(inv, os_name, rts.default_features(self.root))

    def test_mod_path_and_include_name_tests_by_their_module(self):
        self.tree({
            "src/lib.rs": "mod a;\n#[path = \"elsewhere/b_impl.rs\"]\nmod b;\n",
            "src/a.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn one() {}\n    include!(\"a/inc.rs\");\n}\n",
            "src/a/inc.rs": "#[tokio::test]\nasync fn included() {}\n",
            "src/elsewhere/b_impl.rs": "mod pinned {\n    include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/fixtures/up.rs\"));\n}\n",
            "fixtures/up.rs": "#[test]\nfn upstream() {}\n",
        })
        inv = rts.lib_inventory(self.root)
        self.assertEqual(sorted(inv.tests), ["a::tests::included", "a::tests::one", "b::pinned::upstream"])
        self.assertEqual((inv.errors, inv.unreadable), ([], []))

    def test_unreached_test_file_is_an_orphan(self):
        self.tree({"src/lib.rs": "mod used;\n", "src/used.rs": "#[test]\nfn ok() {}\n",
                   "src/forgotten.rs": "#[test]\nfn never_compiled() {}\n",
                   "src/forgotten_attr.rs": "#[cfg_attr(test, test)]\nfn lost() {}\n",
                   "src/forgotten_paren.rs": "#[cfg_attr(test, doc = \"(\", test)]\nfn lost() {}\n",
                   "src/plain.rs": "fn helper() {}\n",
                   "src/main.rs": "mod bin_only;\n", "src/bin_only.rs": "#[test]\nfn bin_test() {}\n"})
        rc, err = self.cli("check")
        self.assertEqual(rc, 1)
        self.assertIn("orphan test file: src/forgotten.rs", err)
        self.assertIn("orphan test file: src/forgotten_attr.rs", err)
        self.assertIn("orphan test file: src/forgotten_paren.rs", err)
        self.assertNotIn("plain.rs", err)
        self.assertNotIn("bin_only.rs", err)

    def test_missing_module_or_include_file_fails_the_check(self):
        self.tree({"src/lib.rs": "#[cfg(windows)]\nmod gone;\nmod t {\n    include!(\"nowhere.rs\");\n}\n"})
        rc, err = self.cli("check")
        self.assertEqual(rc, 1)
        self.assertIn("mod gone: no file at src/gone.rs, src/gone/mod.rs", err)
        self.assertIn("include! names missing file", err)

    def test_non_literal_include_and_gating_cfg_attr_are_unreadable(self):
        self.tree({"src/lib.rs": "mod t {\n    include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n}\n"
                                 "#[cfg_attr(unix, path = \"u.rs\")]\nmod u;\n"
                                 "#[cfg_attr(not(test), allow(dead_code))]\nfn quiet() {}\n"
                                 "#[cfg_attr(test, derive(Default))]\nstruct Probe;\n"
                                 "#[cfg_attr(test, cfg_attr(unix, test))]\nfn nested() {}\n"
                                 "#[cfg_attr(test, doc = \"(\", test)]\nfn paren() {}\n",
                   "src/u.rs": ""})
        rc, err = self.cli("check")
        self.assertEqual(rc, 1)
        self.assertIn("include! with a non-literal path", err)
        self.assertIn("src/lib.rs:4 cfg_attr can change compilation", err)
        self.assertNotIn("src/lib.rs:6", err)
        self.assertNotIn("src/lib.rs:8", err)
        self.assertIn("src/lib.rs:10 cfg_attr can change compilation", err)
        self.assertIn("src/lib.rs:12 cfg_attr can change compilation", err)

    def test_unreadable_source_under_main_or_bin_fails_the_check(self):
        self.tree({"src/lib.rs": "#[test]\nfn ok() {}\n",
                   "src/main.rs": "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n",
                   "src/bin/tool.rs": "#[cfg_attr(unix, path = \"t.rs\")]\nmod t;\n", "src/bin/t.rs": ""})
        rc, err = self.cli("check")
        self.assertEqual(rc, 1)
        self.assertIn("src/main.rs:1 include! with a non-literal path", err)
        self.assertIn("src/bin/tool.rs:1 cfg_attr can change compilation", err)

    def test_path_inside_an_inline_module_resolves_under_that_module(self):
        self.tree({"src/lib.rs": "#[path = \"custom\"]\nmod outer {\n    mod inner;\n}\n"
                                 "mod other {\n    #[path = \"leaf.rs\"]\n    mod leaf;\n}\n",
                   "src/custom/inner.rs": "#[test]\nfn right() {}\n", "src/outer/inner.rs": "#[test]\nfn decoy() {}\n",
                   "src/other/leaf.rs": "#[test]\nfn right() {}\n", "src/leaf.rs": "#[test]\nfn decoy() {}\n"})
        inv = rts.lib_inventory(self.root)
        self.assertEqual(sorted(inv.tests), ["other::leaf::right", "outer::inner::right"])
        self.assertEqual((inv.errors, inv.unreadable), ([], []))

    def test_mod_inside_included_text_resolves_beside_the_included_file(self):
        self.tree({"src/lib.rs": "include!(\"generated/items.rs\");\n",
                   "src/generated/items.rs": "#[path = \"leaf.rs\"]\nmod leaf;\nmod plain;\n",
                   "src/generated/leaf.rs": "#[test]\nfn right() {}\n", "src/leaf.rs": "#[test]\nfn decoy() {}\n",
                   "src/generated/plain.rs": "#[test]\nfn right() {}\n", "src/plain.rs": "#[test]\nfn decoy() {}\n"})
        inv = rts.lib_inventory(self.root)
        self.assertEqual(sorted(inv.tests), ["leaf::right", "plain::right"])
        self.assertEqual((inv.errors, inv.unreadable), ([], []))

    def test_cfg_chain_decides_the_platform(self):
        self.tree({
            "src/lib.rs": "#[cfg(windows)]\nmod win;\nmod unixy;\nmod feat;\n",
            "src/win.rs": "#[test]\nfn w() {}\n",
            "src/unixy.rs": "#![cfg(unix)]\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn u() {}\n"
                            "    #[test]\n    #[cfg(target_os = \"macos\")]\n    fn mac() {}\n}\n",
            "src/feat.rs": "#[cfg(all(test, feature = \"voice\"))]\nmod on {\n    #[test]\n    fn v() {}\n}\n"
                           "#[cfg(any(feature = \"extra\", not(test)))]\nmod off {\n    #[test]\n    fn e() {}\n}\n",
        })
        self.assertEqual(self.split("linux")[0], {"unixy::tests::u", "feat::on::v"})
        self.assertEqual(self.split("macos")[0], {"unixy::tests::u", "unixy::tests::mac", "feat::on::v"})
        self.assertEqual(self.split("windows")[0], {"win::w", "feat::on::v"})

    def test_tail_expression_cfg_does_not_leak_into_the_next_item(self):
        self.tree({"src/lib.rs": "fn pick() -> u8 {\n    #[cfg(not(unix))]\n    1\n}\n"
                                 "#[cfg(test)]\nmod tests {\n    #[test]\n    fn runs() {}\n}\n"})
        self.assertEqual(self.split("linux")[0], {"tests::runs"})

    def test_unknown_cfg_is_reported_not_guessed(self):
        self.tree({"src/lib.rs": "#[cfg(target_arch = \"x86_64\")]\n#[test]\nfn arch() {}\n"})
        compiled, excluded, unknown = self.split("linux")
        self.assertEqual((compiled, excluded, list(unknown)), (set(), set(), ["arch"]))
        rc, err = self.cli("check")
        self.assertEqual(rc, 1)
        self.assertIn("cfg unknown on linux: arch", err)

    def test_cfg_disjoint_duplicates_compile_where_either_holds(self):
        self.tree({"src/lib.rs": "#[cfg(unix)]\n#[path = \"a.rs\"]\nmod imp;\n"
                                 "#[cfg(windows)]\n#[path = \"b.rs\"]\nmod imp;\n",
                   "src/a.rs": "#[test]\nfn same() {}\n", "src/b.rs": "#[test]\nfn same() {}\n"})
        for os_name in ("linux", "windows"):
            self.assertEqual(self.split(os_name)[0], {"imp::same"})

    def test_every_workflow_compare_step_runs_as_written(self):
        """Each compare command the workflows and the native helper run must parse and write its JSON."""
        import shlex
        import shutil
        import subprocess
        import yaml
        runs = [s["run"] for name in ("ci-pr.yml", "ci-main.yml", "ci-nightly.yml")
                for job in yaml.safe_load((ROOT / ".github/workflows" / name).read_text())["jobs"].values()
                for s in job.get("steps", []) if "rust_test_sources.py compare" in s.get("run", "")]
        self.assertEqual(len(runs), 4)
        native = (ROOT / "scripts/ci/nextest-native.sh").read_text().replace('"$dir', '"$NEXTEST_DIR')
        runs += [native.replace('"$platform"', platform) for platform in ("macos", "windows")]
        self.tree({"src/lib.rs": "#[test]\nfn a() {}\n#[cfg(windows)]\n#[test]\nfn w() {}\n"
                                 "#[cfg(unix)]\n#[test]\nfn u() {}\n"})
        (self.root / "scripts/ci").mkdir(parents=True)
        shutil.copy(ROOT / "scripts/ci/rust_test_sources.py", self.root / "scripts/ci")
        bin_dir, nextest_dir = self.root / "bin", self.root / "nextest"
        for d in (bin_dir, nextest_dir, self.root / "target"):
            d.mkdir()
        for name in ("python", "python3"):
            (bin_dir / name).write_text(f'#!/bin/sh\nexec "{sys.executable}" "$@"\n')
            (bin_dir / name).chmod(0o755)
        env = {**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}", "NEXTEST_DIR": str(nextest_dir)}
        for run in runs:
            line = next(l for l in run.replace("\\\n", " ").splitlines() if "rust_test_sources.py compare" in l)
            argv = [a.replace("$NEXTEST_DIR", str(nextest_dir)) for a in shlex.split(line)]
            platform = argv[argv.index("--platform") + 1]
            names = {"a": {"ignored": False}, ("w" if platform == "windows" else "u"): {"ignored": False}}
            (self.root / argv[argv.index("--list") + 1]).write_text(json.dumps(
                {"rust-suites": {"x": {"binary-id": "x", "kind": "lib", "testcases": names}}}))
            with self.subTest(platform=platform):
                (self.root / argv[argv.index("--json-out") + 1]).unlink(missing_ok=True)
                done = subprocess.run(["bash", "-c", line], cwd=self.root, env=env, capture_output=True, text=True)
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                self.assertIn("--json-out", argv)
                written = json.loads((self.root / argv[argv.index("--json-out") + 1]).read_text())
                self.assertEqual(written["static_only"], [])

    def test_removed_lists_what_each_platform_stops_compiling(self):
        lib = ("#[test]\nfn a() {}\n#[cfg(windows)]\n#[test]\nfn w() {}\n#[cfg(target_os = \"macos\")]\n#[test]\nfn m() {}\n"
               "#[test]\nfn gated() {}\n")
        base = self.root / "base"
        base.mkdir()
        (base / "Cargo.toml").write_text((self.root / "Cargo.toml").read_text())
        (base / "src").mkdir()
        (base / "src/lib.rs").write_text(lib)
        self.tree({"src/lib.rs": lib.replace("#[cfg(windows)]\n#[test]\nfn w() {}\n", "")
                   .replace("#[test]\nfn gated()", "#[cfg(unix)]\n#[test]\nfn gated()") + "#[test]\nfn new() {}\n"})
        rc, err = self.cli("removed", "--base-root", base, "--json-out", self.root / "removed.json")
        self.assertEqual(rc, 0, err)
        self.assertEqual(json.loads((self.root / "removed.json").read_text())["removed"],
                         {"linux": [], "macos": [], "windows": ["gated", "w"]})
        self.assertEqual(json.loads((self.root / "removed.json").read_text())["present_tests"],
                         {"linux": ["a", "gated", "new"], "macos": ["a", "gated", "m", "new"],
                          "windows": ["a", "gated", "new", "w"]})
        (base / "src/lib.rs").write_text("#[cfg(feature = \"unknown\" || x)]\n#[test]\nfn odd() {}\n")
        self.assertEqual(self.cli("removed", "--base-root", base, "--json-out", self.root / "removed.json")[0], 1)

    def test_removed_reports_tests_each_platform_newly_ignores(self):
        lib = ("#[cfg(windows)]\n#[test]\nfn w() {}\n#[cfg(target_os = \"macos\")]\n#[test]\nfn m() {}\n"
               "#[test]\nfn all() {}\n#[test]\n#[ignore]\nfn was_ignored() {}\n")
        base = self.root / "base"
        (base / "src").mkdir(parents=True)
        (base / "Cargo.toml").write_text((self.root / "Cargo.toml").read_text())
        (base / "src/lib.rs").write_text(lib)
        self.tree({"src/lib.rs": lib.replace("fn w()", "#[ignore = \"temporarily disabled\"]\nfn w()")
                   .replace("fn m()", "#[cfg_attr(target_os = \"macos\", ignore)]\nfn m()")
                   .replace("fn all()", "#[cfg_attr(windows, ignore)]\nfn all()")
                   .replace("#[ignore]\nfn was_ignored", "fn was_ignored")})
        rc, err = self.cli("removed", "--base-root", base, "--json-out", self.root / "removed.json")
        self.assertEqual(rc, 0, err)
        self.assertEqual(json.loads((self.root / "removed.json").read_text())["runnable_to_ignored"],
                         {"linux": [], "macos": ["m"], "windows": ["all", "w"]})
        self.assertIn("was_ignored", json.loads((self.root / "removed.json").read_text())["present_tests"]["windows"])
        self.tree({"src/lib.rs": lib.replace("fn w()", "#[cfg_attr(cfg_attr(windows, x), ignore)]\nfn w()")})
        self.assertEqual(self.cli("removed", "--base-root", base, "--json-out", self.root / "removed.json")[0], 1)

    def test_compare_fails_when_source_and_list_disagree_on_ignore(self):
        self.tree({"src/lib.rs": "#[test]\n#[ignore]\nfn a() {}\n#[test]\n#[cfg_attr(unix, ignore)]\nfn b() {}\n"})
        suite = {"binary-id": "x", "kind": "lib", "testcases": {"a": {"ignored": True}, "b": {"ignored": True}}}
        self.assertEqual(self.compare({"rust-suites": {"x": suite}}), (0, ""))
        self.assertEqual(self.compare({"rust-suites": {"x": suite}}, platform="windows")[0], 1)
        self.assertEqual(json.loads((self.root / "cmp.json").read_text())["ignore_mismatch"], ["b"])

    def compare(self, doc, platform="linux"):
        listed = self.root / "list.json"
        listed.write_text(json.dumps(doc))
        return self.cli("compare", "--platform", platform, "--list", listed, "--json-out", self.root / "cmp.json")

    def test_compare_fails_when_a_compiled_source_test_is_missing_from_the_list(self):
        self.tree({"src/lib.rs": "#[test]\nfn a() {}\n#[test]\nfn b() {}\n#[cfg(windows)]\n#[test]\nfn w() {}\n"})
        suite = {"binary-id": "x", "kind": "lib", "testcases": {"a": {"ignored": False}, "gen": {"ignored": True}}}
        rc, _ = self.compare({"rust-suites": {"x": suite}})
        result = json.loads((self.root / "cmp.json").read_text())
        self.assertEqual((rc, result["static_only"], result["cargo_only"]), (1, ["b"], ["gen"]))
        suite["testcases"]["b"] = {"ignored": False}
        self.assertEqual(self.compare({"rust-suites": {"x": suite}})[0], 0)

    def test_compare_rejects_lists_without_one_lib_suite(self):
        self.tree({"src/lib.rs": "#[test]\nfn a() {}\n"})
        for doc in ({"rust-binaries": {}}, {"rust-suites": {}},
                    {"rust-suites": {"x": {"kind": "lib", "testcases": {}}}}):
            with self.subTest(doc):
                rc, err = self.compare(doc)
                self.assertEqual(rc, 1)
                self.assertIn("ERROR", err)

    def test_cfg_evaluator_operators(self):
        cases = [("( unix )", "linux", True), ("( not ( unix ) )", "linux", False),
                 ("( any ( windows target_os = macos ) )", "macos", True),
                 ("( all ( test target_family = unix ) )", "windows", False),
                 ("( feature = voice )", "linux", True), ("( any ( windows target_pointer_width = 64 ) )", "linux", None),
                 ("( all ( test sanitize ) )", "linux", None), ("( not ( unix windows ) )", "linux", None),
                 ("( all ( ) )", "linux", True), ("( any ( ) )", "linux", False), ("( not ( any ( ) ) )", "linux", True)]
        for text, os_name, want in cases:
            with self.subTest(text):
                self.assertIs(rts.cfg_holds(tuple(text.split()), os_name, frozenset({"voice"})), want)


class PgClassifier(unittest.TestCase):
    """Which lib tests need PostgreSQL, judged from source by scripts/ci/pg_test_classifier.py."""

    DB = """pub fn plain() {}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::fixtures::open as opener;
    fn make_db() -> u8 { TestPostgresDb::new(); 0 }
    fn wrapper() { make_db(); }
    #[test]
    fn direct() { let _db = TestPostgresDb::new(); }
    #[test]
    fn indirect_two_hops() { wrapper(); }
    #[test]
    fn via_use_alias() { opener(); }
    #[test]
    fn pure() { plain(); }
    #[test]
    fn nested_helper_does_not_taint() { fn unused() { TestPostgresDb::new(); } }
}
"""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="pg-classifier-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.tree({
            "Cargo.toml": '[package]\nname = "x"\n',
            "src/lib.rs": "mod db;\nmod support;\n#[path = \"store_impl.rs\"]\nmod store;\n#[cfg(test)]\nmod ext_tests;\n",
            "src/db.rs": self.DB,
            "src/support.rs": "#[cfg(test)]\npub mod fixtures {\n    pub fn open() { connect_test_pool(); }\n}\n",
            "src/store_impl.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn uses_pool() { connect_test_pool_for(); }\n}\n",
            "src/ext_tests.rs": "#[test]\nfn ext() { let _ = PgRecoveryTestDatabase::new(); }\n#[test]\nfn ext_pure() {}\n",
        })

    def tree(self, files):
        for rel, text in files.items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)

    def test_seed_reach_decides_pg_membership(self):
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertEqual(set(pg), {"db::tests::direct", "db::tests::indirect_two_hops", "db::tests::via_use_alias",
                                   "store::tests::uses_pool", "ext_tests::ext"})
        self.assertEqual(pg["store::tests::uses_pool"], "src/store_impl.rs")

    def test_exemption_removes_a_classified_test_and_a_stale_one_is_a_problem(self):
        pg, problems = pgc.classify(self.root, exempt={"db::tests::direct": "fixture"})
        self.assertNotIn("db::tests::direct", pg)
        self.assertEqual(problems, [])
        _, problems = pgc.classify(self.root, exempt={"db::tests::pure": "not PG at all"})
        self.assertEqual(problems, ["stale exemption: db::tests::pure is no longer classified as PG"])

    def test_call_chain_past_the_depth_limit_is_a_problem_not_non_pg(self):
        hops = "".join(f"    fn h{i}() {{ h{i + 1}(); }}\n" for i in range(18))
        self.tree({"src/db.rs": "#[cfg(test)]\nmod tests {\n" + hops
                   + "    fn h18() { TestPostgresDb::new(); }\n    #[test]\n    fn deep() { h0(); }\n}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertNotIn("db::tests::deep", pg)
        self.assertEqual(len(problems), 1)
        self.assertIn("pg-reach-depth-exhausted: db::tests::deep", problems[0])

    def test_unresolved_test_module_is_a_problem(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\n#[cfg(test)]\nmod gone_tests;\n"})
        _, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(len(problems), 1)
        self.assertIn("unresolved-external-test-module: src/lib.rs:3", problems[0])

    def test_every_supported_test_attribute_is_classified(self):
        attrs = ("test", "tokio::test", "async_std::test", "actix_rt::test")
        body = "".join(f"    #[{a}]\n    async fn pg_{i}() {{ TestPostgresDb::new(); }}\n" for i, a in enumerate(attrs))
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod attrs;\n",
                   "src/attrs.rs": "#[cfg(test)]\nmod tests {\n" + body + "}\n"})
        names = [f"attrs::tests::pg_{i}" for i in range(len(attrs))]
        listed = {"rust-suites": {"x": {"binary-id": "x", "kind": "lib", "testcases": {
            name: {"ignored": False} for name in (*names, "db::tests::pure")}}}}
        (self.root / "list.json").write_text(json.dumps(listed))
        rc, err = self.cli("classify", "--json-out", self.root / "out.json", "--list", self.root / "list.json")
        self.assertEqual(rc, 0, err)
        out = json.loads((self.root / "out.json").read_text())
        self.assertLessEqual(set(names), set(out["pg"]))
        self.assertEqual(out["non_pg"], ["db::tests::pure"])

    def test_recognised_test_missing_from_the_inventory_fails_the_cli(self):
        bin_tests = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn bin_only() {}\n}\n"
        self.tree({"src/main.rs": "mod cli;\n", "src/cli.rs": bin_tests})
        self.assertEqual(self.cli("classify", "--json-out", self.root / "out.json"), (0, ""))
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod attrs;\n",
                   "src/attrs.rs": "#[cfg(test)]\nmod tests {\n    #[async_std::test]\n    async fn pg() {}\n}\n"})
        with mock.patch.object(pgc.rust_test_sources, "TEST_ATTRS", {("test",), ("tokio", "test")}):
            rc, err = self.cli("classify", "--json-out", self.root / "out.json")
        self.assertEqual(rc, 1)
        self.assertIn("test-not-in-inventory: attrs::tests::pg", err)
        self.assertNotIn("bin_only", err)

    def test_unlinked_file_does_not_lend_a_helper_to_the_call_graph(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\n#[path = \"other.rs\"]\nmod unused;\n",
                   "src/other.rs": "#[cfg(test)]\nmod tests {\n    fn real_db() { TestPostgresDb::new(); }\n"
                                   "    fn helper() { real_db(); }\n    #[test]\n    fn t() { helper(); }\n}\n",
                   "src/unused.rs": "#[cfg(test)]\n#[path = \"zombie.rs\"]\nmod tests;\n",
                   "src/zombie.rs": "fn helper() {}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertEqual(pg.get("unused::tests::t"), "src/other.rs")

    def test_a_file_reached_as_two_modules_is_classified_under_both(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\n#[path = \"shared.rs\"]\nmod a;\n#[path = \"shared.rs\"]\nmod b;\n",
                   "src/shared.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn pg() { TestPostgresDb::new(); }\n}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertEqual((pg.get("a::tests::pg"), pg.get("b::tests::pg")), ("src/shared.rs", "src/shared.rs"))

    def test_child_of_a_path_loaded_file_keeps_its_real_module_name(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\n#[path = \"w/outer.rs\"]\nmod outer;\n",
                   "src/w/outer.rs": "mod inner;\n",
                   "src/w/inner.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn pg() { TestPostgresDb::new(); }\n}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertEqual(pg.get("outer::inner::tests::pg"), "src/w/inner.rs")

    def test_a_fixture_two_globs_up_makes_the_grandchild_test_pg(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod q;\n",
                   "src/q.rs": "#[cfg(test)]\nmod tests;\n",
                   "src/q/tests.rs": "struct Fixture;\nimpl Fixture {\n    fn new() -> Self { TestPostgresDb::new(); Fixture }\n}\n"
                                     "mod auto;\n",
                   "src/q/tests/auto.rs": "use super::*;\nmod mend;\n",
                   "src/q/tests/auto/mend.rs": "use super::*;\n#[test]\nfn grandchild() { let _f = Fixture::new(); }\n"
                                               "#[test]\nfn pure() {}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertIn("q::tests::auto::mend::grandchild", pg)
        self.assertNotIn("q::tests::auto::mend::pure", pg)

    def test_a_use_of_a_child_module_is_relative_to_its_module(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod h;\n",
                   "src/h.rs": "#[cfg(test)]\nmod tests {\n    mod service {\n        pub(super) struct Service;\n"
                               "        impl Service {\n            pub(super) fn start() -> Self { TestPostgresDb::new(); Service }\n"
                               "        }\n    }\n    use service::Service;\n    #[test]\n    fn via_child() { Service::start(); }\n}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertIn("h::tests::via_child", pg)

    def test_impl_helpers_follow_pg_references_and_preserve_local_item_boundaries(self):
        self.tree({"src/lib.rs": "#[cfg(test)]\nmod fixtures;\n",
                   "src/fixtures.rs": """fn open() { TestPostgresDb::new(); }
struct Rig;
impl Rig { fn new() -> Self { open(); Rig } }
struct Relaunched;
impl Relaunched { fn new() -> Self { Rig::new(); Relaunched } }
impl Relaunched { fn name() -> &'static str { "fixture" } }
fn plain() {}
struct Aliased;
impl Aliased {
    fn new() { use self::open as call; call(); }
    fn other() { use self::plain as call; call(); }
}
struct Pure;
impl Pure {
    fn new() -> Self {
        fn unused() { open(); }
        impl Nested { fn unused() { TestPostgresDb::new(); } }
        Pure
    }
}
#[test]
fn via_impl() { Relaunched::new(); }
#[test]
fn via_method_import() { Aliased::new(); }
#[test]
fn pure() { Pure::new(); }
"""})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertEqual(set(pg), {"fixtures::via_impl", "fixtures::via_method_import"})

    def test_a_call_through_a_reexport_reaches_the_pg_helper(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod r;\n",
                   "src/r.rs": "mod inner;\n#[cfg(test)]\npub(crate) use inner::exact_tests::shared_pg;\n"
                               "#[cfg(test)]\nmod tests {\n    #[test]\n    fn via_reexport() { crate::r::shared_pg(); }\n}\n",
                   "src/r/inner.rs": "#[cfg(test)]\npub(crate) mod exact_tests {\n"
                                     "    pub(crate) fn shared_pg() { TestPostgresDb::new(); }\n}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertIn("r::tests::via_reexport", pg)

    def test_a_glob_import_of_a_reexport_reaches_the_pg_helper(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod g;\n",
                   "src/g.rs": "mod inner;\n#[cfg(test)]\npub(crate) use inner::helpers::shared_pg;\n"
                               "#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn via_glob_reexport() { shared_pg(); }\n"
                               "    #[test]\n    fn pure() {}\n}\n",
                   "src/g/inner.rs": "#[cfg(test)]\npub(crate) mod helpers {\n"
                                     "    pub(crate) fn shared_pg() { TestPostgresDb::new(); }\n}\n"})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertEqual(problems, [])
        self.assertIn("g::tests::via_glob_reexport", pg)
        self.assertNotIn("g::tests::pure", pg)

    SELF_EXEC = """#[cfg(test)]
mod tests {
    use std::process::Command;
    fn run(test: &str) { Command::new(std::env::current_exe().unwrap()).args(["--exact", test]).output().unwrap(); }
    #[test]
    fn parent() {
        for test in ["s::tests::child_a_pg", "s::tests::child_b_pg"] {
            let out = Command::new(std::env::current_exe().unwrap()).args(["--exact", test]).output().unwrap();
            assert!(out.status.success());
        }
    }
    #[test]
    fn pure_parent() { run("s::tests::child_pure"); }
    #[test]
    fn cycle_a() { run("t::tests::cycle_b"); }
    #[test]
    fn cycle_b() { run("t::tests::cycle_a"); run("s::tests::child_a_pg"); }
    #[test]
    fn reruns_itself() {
        let name = std::thread::current().name().unwrap().to_owned();
        Command::new(std::env::current_exe().unwrap()).args(["--exact", &name]).output().unwrap();
    }
}
"""

    def test_a_parent_that_reruns_pg_tests_with_exact_is_pg(self):
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod s;\nmod t;\n",
                   "src/s.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn child_a_pg() { TestPostgresDb::new(); }\n"
                               "    #[test]\n    fn child_b_pg() { TestPostgresDb::new(); }\n    #[test]\n    fn child_pure() {}\n}\n",
                   "src/t.rs": self.SELF_EXEC})
        names = ["t::tests::parent", "t::tests::pure_parent", "t::tests::cycle_a", "t::tests::cycle_b",
                 "t::tests::reruns_itself", "s::tests::child_pure"]
        listed = {"rust-suites": {"x": {"binary-id": "x", "kind": "lib", "testcases": {
            name: {"ignored": False} for name in names}}}}
        (self.root / "list.json").write_text(json.dumps(listed))
        rc, err = self.cli("classify", "--json-out", self.root / "out.json", "--list", self.root / "list.json")
        self.assertEqual(rc, 0, err)
        out = json.loads((self.root / "out.json").read_text())
        self.assertLessEqual({"t::tests::parent", "t::tests::cycle_a", "t::tests::cycle_b"}, set(out["pg"]))
        self.assertEqual(out["non_pg"], ["s::tests::child_pure", "t::tests::pure_parent", "t::tests::reruns_itself"])

    def test_a_self_exec_naming_no_resolvable_test_is_a_problem(self):
        text = ("#[cfg(test)]\nmod tests {\n    #[test]\n    fn opaque() {\n        let name = pick();\n"
                "        std::process::Command::new(std::env::current_exe().unwrap()).args([\"--exact\", &name]).output().unwrap();\n"
                "    }\n}\n")
        self.tree({"src/lib.rs": "mod db;\nmod support;\nmod u;\n", "src/u.rs": text})
        pg, problems = pgc.classify(self.root, exempt={})
        self.assertNotIn("u::tests::opaque", pg)
        self.assertEqual(len(problems), 1)
        self.assertIn("unresolved-self-exec: src/u.rs:4: u::tests::opaque", problems[0])

    def impl_self_exec_cases(self):
        run = ('fn run(test: &str) { std::process::Command::new(std::env::current_exe().unwrap())'
               '.args(["--exact", test]).output().unwrap(); }')
        label = 'fn label() -> &\'static str { "pure_child" }'
        calls = {
            "opaque": ("Runner::run(&pick());", False, 1),
            "known_pg": ('Runner::run("fixtures::pg_child");', True, 0),
            "known_and_opaque": ('Runner::run("fixtures::pure_child"); Runner::run(&pick());', False, 1),
            "value_receiver": ("let runner = Runner; runner.run(&pick());", False, 1),
        }
        for variant in ("absent", "before", "after", "same", "forwarded", "ufcs"):
            methods = f"impl Runner {{ {run} }}"
            if variant == "before":
                methods = f"impl Runner {{ {label} }}\n" + methods
            elif variant == "after":
                methods += f"\nimpl Runner {{ {label} }}"
            elif variant == "same":
                methods = f"impl Runner {{ {label} {run} }}"
            elif variant == "forwarded":
                methods = (f"impl Runner {{ {label} fn run(test: &str) {{ Self::dispatch(test); }} }}\n"
                           f"impl Runner {{ {run.replace('fn run(', 'fn dispatch(')} }}")
            elif variant == "ufcs":
                methods = f"trait Invoke {{ fn run(test: &str); }}\nimpl Invoke for Runner {{ {run} }}\nimpl Runner {{ {label} }}"
            cases = dict(calls)
            if variant != "absent":
                cases.update({"unrelated_label_call": ("Runner::label(); Runner::run(&pick());", False, 1),
                              "connected_label": ("Runner::run(Runner::label());", False, 0)})
            for case, (call, parent_pg, problems) in cases.items():
                if variant in ("forwarded", "ufcs") and case == "value_receiver":
                    continue
                if variant == "ufcs":
                    call = call.replace("Runner::run", "<Runner as Invoke>::run")
                fixture_methods = methods.replace("fn run(test:", "fn run(&self, test:") if case == "value_receiver" else methods
                self.tree({"src/lib.rs": "#[cfg(test)]\nmod fixtures;\n",
                           "src/fixtures.rs": "struct Runner;\n" + fixture_methods + "\n"
                           'fn pick() -> String { ["fixtures::pg_", "child"].concat() }\n'
                           "#[test]\nfn opaque() { " + call + " }\n"
                           "#[test]\nfn pg_child() { TestPostgresDb::new(); }\n"
                           "#[test]\nfn pure_child() {}\n"})
                yield f"{variant}/{case}", parent_pg, problems

    def test_impl_self_exec_requires_connected_child_evidence(self):
        for case, parent_pg, problem_count in self.impl_self_exec_cases():
            with self.subTest(case=case):
                pg, problems = pgc.classify(self.root, exempt={})
                self.assertEqual("fixtures::opaque" in pg, parent_pg)
                self.assertEqual(len(problems), problem_count)
                if problem_count:
                    self.assertIn("unresolved-self-exec:", problems[0])

    def test_impl_self_exec_cli_rejects_unresolved_child_with_a_list(self):
        listed = {"rust-suites": {"x": {"binary-id": "x", "kind": "lib", "testcases": {
            name: {"ignored": False} for name in ("fixtures::opaque", "fixtures::pg_child", "fixtures::pure_child")}}}}
        (self.root / "list.json").write_text(json.dumps(listed))
        for case, parent_pg, problem_count in self.impl_self_exec_cases():
            with self.subTest(case=case):
                rc, err = self.cli("classify", "--list", self.root / "list.json", "--json-out", self.root / "out.json")
                self.assertEqual(rc, 1 if problem_count else 0, err)
                result = json.loads((self.root / "out.json").read_text())
                self.assertEqual("fixtures::opaque" in result["pg"], parent_pg)
                self.assertEqual(len(result["problems"]), problem_count)

    def test_local_self_exec_ids_use_the_same_paths_on_every_host(self):
        self.tree({"src/lib.rs": "#[cfg(test)]\nmod local;\n#[cfg(test)]\nmod foreign;\n",
                   "src/local.rs": """fn run(test: &str) {
    std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", test]).output();
}
#[test]
fn literal_parent() { run("child"); }
#[test]
fn concat_parent() { run(concat!(module_path!(), "::child")); }
#[test]
fn format_parent() { run(&format!("{}::{}", module_path!(), "child")); }
#[test]
fn pure_parent() { run("pure_child"); }
#[test]
fn child() { TestPostgresDb::new(); }
#[test]
fn pure_child() {}
#[test]
fn opaque() { run(&pick()); }
""",
                   "src/foreign.rs": "#[test]\nfn child() {}\n#[test]\nfn pure_child() { TestPostgresDb::new(); }\n"})
        inventory = pgc.rust_test_sources.lib_inventory(self.root)
        expected = {"local::child", "local::literal_parent", "local::concat_parent",
                    "local::format_parent", "foreign::pure_child"}
        for normalizer in (posixpath.normpath, ntpath.normpath):
            for separator in ("/", "\\"):
                with self.subTest(host=normalizer.__module__, separator=separator):
                    sites = {name: [(site.replace("src/", "src/../src/").replace("/", separator), cfg)
                                    for site, cfg in declarations]
                             for name, declarations in inventory.tests.items()}
                    host_os = SimpleNamespace(path=SimpleNamespace(normpath=normalizer, relpath=os.path.relpath))
                    with mock.patch.object(pgc, "os", host_os), \
                            mock.patch.object(pgc.rust_test_sources, "lib_inventory",
                                              return_value=SimpleNamespace(tests=sites, modules=inventory.modules)):
                        pg, problems = pgc.classify(self.root, exempt={})
                    self.assertEqual(set(pg), expected)
                    self.assertEqual(len(problems), 1)
                    self.assertIn("unresolved-self-exec: src/local.rs:", problems[0])
                    self.assertIn("local::opaque re-runs", problems[0])

    def cli(self, *argv):
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()), \
                mock.patch.object(pgc, "EXEMPT", {}):
            try:
                rc = pgc.main(["--repo-root", str(self.root), *map(str, argv)])
            except Exception as exc:  # noqa: BLE001
                return "crash", f"{type(exc).__name__}: {exc}"
        return rc, err.getvalue()

    def test_list_split_gives_native_runs_the_non_pg_tests(self):
        listed = {"rust-suites": {"x": {"binary-id": "x", "kind": "lib", "testcases": {
            name: {"ignored": False} for name in ("db::tests::direct", "db::tests::pure", "ext_tests::ext_pure")}}}}
        (self.root / "list.json").write_text(json.dumps(listed))
        rc, err = self.cli("classify", "--json-out", self.root / "out.json", "--list", self.root / "list.json")
        self.assertEqual(rc, 0, err)
        out = json.loads((self.root / "out.json").read_text())
        self.assertEqual(out["non_pg"], ["db::tests::pure", "ext_tests::ext_pure"])

    def test_unreadable_list_or_problem_fails_the_cli(self):
        (self.root / "list.json").write_text(json.dumps({"rust-binaries": {}}))
        rc, err = self.cli("classify", "--json-out", self.root / "out.json", "--list", self.root / "list.json")
        self.assertEqual(rc, 1)
        self.assertIn("nextest list:", err)
        self.tree({"src/lib.rs": "mod db;\nmod support;\n#[cfg(test)]\nmod gone_tests;\n"})
        rc, err = self.cli("classify", "--json-out", self.root / "out.json")
        self.assertEqual(rc, 1)
        self.assertIn("unresolved-external-test-module", err)


if __name__ == "__main__":
    unittest.main()
