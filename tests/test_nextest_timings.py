"""scripts/ci/nextest-timings.sh against a fake gh: which CI Main run feeds the plan, and every gap stays uniform."""

import contextlib
import datetime
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("nextest_ci", ROOT / "scripts/ci/nextest_ci.py")
nci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nci)
BIN = "agentdesk"
WORKERS = 6

# GitHub's run list, artifact list and download, filtered the way the real API filters; --jq runs through jq.
FAKE_GH = f"""#!{sys.executable}
import fnmatch, json, os, subprocess, sys
from pathlib import Path
state = json.loads(Path(os.environ["FAKE_GH_STATE"]).read_text())
argv = sys.argv[1:]
def opt(name):
    return argv[argv.index(name) + 1] if name in argv else None
def jq(doc):
    sys.stdout.write(subprocess.run(["jq", "-r", opt("--jq")], input=json.dumps(doc), text=True,
                                    capture_output=True, check=True).stdout)
command = " ".join(argv[:2])
if command in state.get("fail", []):
    sys.exit("HTTP 403: API rate limit exceeded")
if command == "run list":
    runs = [r for r in state["runs"] if r["workflow"] == opt("--workflow")
            and (opt("--branch") is None or r["headBranch"] == opt("--branch"))][:int(opt("--limit"))]
    jq([{{k: r[k] for k in opt("--json").split(",")}} for r in runs])
elif argv[0] == "api":
    run = argv[1].split("/")[-2]
    jq({{"artifacts": [{{"name": name, "expired": f"{{run}}/{{name}}" in state.get("expired", [])}}
                       for name in state["artifacts"].get(run, {{}})]}})
elif command == "run download":
    matched = {{name: files for name, files in state["artifacts"].get(argv[2], {{}}).items()
               if fnmatch.fnmatch(name, opt("--pattern")) and f"{{argv[2]}}/{{name}}" not in state.get("expired", [])}}
    if not matched:
        sys.exit("no artifact matches any of the names or patterns provided")
    for name, files in matched.items():
        for rel, text in files.items():
            path = Path(opt("--dir")) / name / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
else:
    sys.exit(f"unexpected gh {{argv}}")
"""


def junit(seconds):
    """seconds: {test name: time} under one agentdesk suite."""
    rows = "".join(f'<testcase name="{n}" classname="{BIN}" time="{s}"/>' for n, s in seconds.items())
    return f'<testsuites name="nextest-run"><testsuite name="{BIN}">{rows}</testsuite></testsuites>'


def worker_artifacts(seconds, attempt=1, skip=()):
    """One artifact per worker; worker i's report times m::t<i> at `seconds` and m::slow at 10*seconds."""
    return {f"nextest-worker-{i}-attempt-{attempt}": {
        "junit.xml": junit({f"m::t{i}": seconds, "m::slow": 10 * seconds}), "exit-code": "0\n"}
        for i in range(WORKERS) if i not in skip}


def created(days_ago):
    when = datetime.datetime.now(datetime.UTC) - datetime.timedelta(days=days_ago)
    return when.strftime("%Y-%m-%dT%H:%M:%SZ")


def main_run(run_id, days_ago=0.1, event="push", branch="main", workflow="ci-main.yml"):
    return {"databaseId": run_id, "event": event, "createdAt": created(days_ago), "headBranch": branch,
            "workflow": workflow}


class TimingsScript(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="nextest-timings-")
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        (self.dir / "bin").mkdir()
        (self.dir / "bin/gh").write_text(FAKE_GH)
        (self.dir / "bin/gh").chmod(0o755)

    def run_script(self, state):
        (self.dir / "state.json").write_text(json.dumps(state))
        summary = self.dir / "summary.md"
        summary.write_text("")
        env = {**os.environ, "PATH": f"{self.dir / 'bin'}{os.pathsep}{os.environ['PATH']}",
               "FAKE_GH_STATE": str(self.dir / "state.json"), "GITHUB_REPOSITORY": "o/r",
               "GITHUB_STEP_SUMMARY": str(summary)}
        done = subprocess.run(["bash", str(ROOT / "scripts/ci/nextest-timings.sh"), str(self.dir / "timings"),
                               str(WORKERS)], cwd=ROOT, env=env, capture_output=True, text=True)
        return done.returncode, done.stdout.splitlines(), done.stderr, summary.read_text()

    def plan(self, args):
        """nextest_ci plan over m::t0..5 and m::slow with the script's arguments; its log line and plan."""
        names = [f"m::t{i}" for i in range(WORKERS)] + ["m::slow"]
        universe = {"rust-suites": {BIN: {"binary-id": BIN, "kind": "lib", "status": "listed", "testcases": {
            n: {"ignored": False, "filter-match": {"status": "matches"}} for n in names}}}}
        (self.dir / "list.json").write_text(json.dumps(universe))
        (self.dir / "archive").write_bytes(b"x")
        (self.dir / "empty.toml").write_text("version = 1\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            rc = nci.main(["plan", "--universe", str(self.dir / "list.json"), "--archive", str(self.dir / "archive"),
                           "--workers", "2", "--sha", "a" * 40, "--scope", "linux-lib", "--rust-version", "1.94.1",
                           "--nextest-version", "0.9.149", "--target", "x86_64-unknown-linux-gnu",
                           "--base-config", str(ROOT / ".config/nextest.toml"),
                           "--retry-policy", str(self.dir / "empty.toml"), "--output", str(self.dir / "plan"), *args])
        self.assertEqual(rc, 0, out.getvalue())
        return out.getvalue(), json.loads((self.dir / "plan/plan.json").read_text())

    def test_newest_complete_main_push_run_feeds_the_plan_with_every_attempt(self):
        complete = worker_artifacts(7)
        complete.update(worker_artifacts(9, attempt=2, skip={0, 2, 3, 4, 5}))
        state = {"runs": [main_run(13, event="pull_request"), main_run(12, branch="feature", event="workflow_dispatch"),
                          main_run(11, workflow="ci-pr.yml"), main_run(10), main_run(9), main_run(8),
                          main_run(7, days_ago=1), main_run(6, days_ago=8)],
                 "artifacts": {"13": worker_artifacts(1), "12": worker_artifacts(1), "11": worker_artifacts(1),
                               "10": {}, "9": worker_artifacts(1, skip={2}),
                               "8": worker_artifacts(1), "7": complete, "6": worker_artifacts(1)},
                 "expired": ["8/nextest-worker-3-attempt-1"]}
        rc, args, err, summary = self.run_script(state)
        self.assertEqual(rc, 0, err)
        self.assertNotIn("::warning", err)
        self.assertIn("nextest timings: 7 JUnit reports from CI Main run 7", summary)
        self.assertEqual(args[0::2], ["--timings"] * 7)
        timings, _ = nci.load_timings(args[1::2])
        # Worker 1's second attempt (9s) beats its first (7s); nothing from runs 13..8 or the 8-day-old run 6.
        self.assertEqual(timings[(BIN, "m::t1")], 9.0)
        self.assertEqual({timings[(BIN, f"m::t{i}")] for i in (0, 2, 3, 4, 5)}, {7.0})
        self.assertEqual(timings[(BIN, "m::slow")], 90.0)
        line, plan = self.plan(args)
        self.assertIn("timing timed, known 7, fallback 0", line)
        self.assertEqual([[BIN, "m::slow"]], plan["workers"][0]["tests"])

    def test_any_gap_prints_no_argument_and_warns_without_failing(self):
        def run7(**changes):
            artifacts = worker_artifacts(7)
            for worker, files in changes.items():
                artifacts[f"nextest-worker-{worker[1:]}-attempt-1"] = files
            return {"runs": [main_run(7)], "artifacts": {"7": artifacts}}

        cases = {
            "cannot list CI Main runs": {**run7(), "fail": ["run list"]},
            "cannot list the artifacts of CI Main run 7": {**run7(), "fail": ["api repos/o/r/actions/runs/7/artifacts?per_page=100"]},
            "cannot download the worker artifacts of CI Main run 7": {**run7(), "fail": ["run download"]},
            "no CI Main push or dispatch run of the last 7 days": {"runs": [main_run(7, days_ago=8), main_run(5)],
                                                                    "artifacts": {"7": worker_artifacts(7),
                                                                                  "5": worker_artifacts(7, skip={5})}},
            "CI Main run 7 has no JUnit report for worker 5": run7(w5={"exit-code": "101\n"}),
            "cannot parse JUnit": run7(w4={"junit.xml": junit({"m::t4": 1})[:-20]}),
            "root is <testsuite>": run7(w2={"junit.xml": f'<testsuite name="{BIN}"/>'}),
            "has no timed testcase": run7(w3={"junit.xml": junit({})}),
            "has 2 testcases without a valid time": run7(w1={"junit.xml": junit({"m::t1": 5.0, "m::a": "nan", "m::b": -3})}),
        }
        for reason, state in cases.items():
            with self.subTest(reason):
                rc, args, err, summary = self.run_script(state)
                self.assertEqual((rc, args), (0, []), err)
                warning = [line for line in err.splitlines() if line.startswith("::warning title=nextest timing fallback::")]
                self.assertEqual(len(warning), 1, err)
                self.assertIn(reason, warning[0])
                self.assertIn("nextest timing fallback: ", summary)
                self.assertIn("the plan splits by test count", summary)
        line, _ = self.plan(args)
        self.assertIn("timing uniform, known 0", line)


if __name__ == "__main__":
    unittest.main()
