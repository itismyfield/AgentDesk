"""Exercise scripts/ci/rust_test_failure_history.py through a fake gh executable."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/ci/rust_test_failure_history.py"
REPO = "repos/itismyfield/AgentDesk/actions/"
END = "2026-11-22T02:11:00Z"
WINDOW = f"2026-10-23T02:11:00Z..{END}"
PAGE = "per_page=100&page=1"
SHA = "a" * 40
FAKE_GH = f"""#!{sys.executable}
import json, pathlib, sys
root = pathlib.Path(__file__).parent
with (root / 'calls').open('a') as stream:
    print(sys.argv[-1], file=stream)
route = json.loads((root / 'routes').read_text()).get(sys.argv[-1])
if route is None:
    sys.exit('HTTP 404')
sys.stdout.buffer.write((root / route).read_bytes())
"""


def run(id_, branch, attempts=1, conclusion="failure", created="2026-11-21T00:00:00Z"):
    return dict(id=id_, head_sha="b" * 40, head_branch=branch, created_at=created, conclusion=conclusion,
                run_attempt=attempts, status="completed", event="push" if branch == "main" else "pull_request")


def worker(id_, conclusion, index=0):
    return dict(id=id_, name=f"Rust tests (worker {index})", head_sha="b" * 40, conclusion=conclusion,
                started_at=END, completed_at=END, steps=[{"name": "Run planned tests"}])


def page(key, *rows):
    return {"total_count": len(rows), key: rows}


def install(tmp_path, payloads):
    fake = tmp_path / "gh"
    fake.write_text(FAKE_GH)
    fake.chmod(0o700)
    routes = {}
    for index, (endpoint, payload) in enumerate(payloads.items()):
        response = tmp_path / str(index)
        if isinstance(payload, bytes):
            response.write_bytes(payload)
        else:
            response.write_text(json.dumps(payload))
        routes[REPO + endpoint] = response.name
    (tmp_path / "routes").write_text(json.dumps(routes))


def invoke(tmp_path, *args):
    out = tmp_path / "out.json"
    proc = subprocess.run([sys.executable, str(SCRIPT), "--until", END, "--cache", str(tmp_path / "cache"),
                           "--output", str(out), "--timing-runs", "0", *args], text=True, capture_output=True)
    return proc, json.loads(out.read_text()) if out.exists() else None


class RustTestFailureHistoryTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        # The fake gh must shadow the real one for the script's subprocess.
        env = mock.patch.dict(os.environ, {"PATH": tmp.name})
        env.start()
        self.addCleanup(env.stop)

    def test_history_uses_matching_attempt_and_explicit_pass(self):
        tmp_path = self.tmp
        rerun = tmp_path / "rerun.zip"
        with zipfile.ZipFile(rerun, "w") as artifact:
            artifact.writestr("worker.json", json.dumps(dict(run_id="1", run_attempt="2", worker=1, sha=SHA)))
            artifact.writestr("junit.xml", '<testsuites><testsuite name="agentdesk">'
                              '<testcase name="m::recovers" time="2"/><testcase name="m::retried" time="3">'
                              '<flakyFailure time="4"/></testcase></testsuite></testsuites>')
        checkout = f"{END} [command]/usr/bin/git log -1 --format=%H\n{END} {SHA}\n"
        terminal = "TRY 2 FAIL [ 1.25s] (2/2) agentdesk m::recovers\n"
        jobs = checkout + f"{END} TRY 1 FAIL [ 1.50s] (─────────) agentdesk m::recovers\n" + terminal * 2
        jobs += "FAIL [ 5.0s] (1/2) agentdesk m::not_run_again\nSummary [8s] 2 tests run: 0 passed, 2 failed\n"
        legacy = f"{END} \x1b[0mtest m::recovers ... FAILED\n{END} test result: FAILED. 0 passed; 1 failed;\n"
        audit = (f"{END} SHA `{SHA}` · plan `abc` · executed 3/3 · failed 0 · retry-pass 2 · retry mode all-once\n"
                 "| `agentdesk m::retried` | 2 | 3 | 7 | - | - |\n"
                 "| `agentdesk m::audit_retry` | 2 | 1 | 4 | - | - |\n")
        install(tmp_path, {
            f"workflows/ci-pr.yml/runs?created={WINDOW}&{PAGE}": page("workflow_runs",
                run(1, "batch/land-test", 2, "success"), run(3, "exclude-end", created=END)),
            f"workflows/ci-main.yml/runs?created={WINDOW}&{PAGE}": page("workflow_runs", run(2, "main")),
            f"runs/1/attempts/1/jobs?{PAGE}": page("jobs", worker(11, "failure")),
            f"runs/1/attempts/2/jobs?{PAGE}": page("jobs", worker(12, "success", 1),
                dict(worker(13, "success"), name="Rust tests evidence")),
            f"runs/2/attempts/1/jobs?{PAGE}": page("jobs", dict(worker(21, "failure"), name="Library test sweep (main shard 0)")),
            f"runs/1/artifacts?{PAGE}": page("artifacts", {"name": "nextest-worker-1-attempt-2", "expired": False, "id": 100}),
            "artifacts/100/zip": rerun.read_bytes(), "jobs/11/logs": jobs.encode(), "jobs/21/logs": legacy.encode(),
            "jobs/13/logs": audit.encode(),
        })
        proc, result = invoke(tmp_path)
        assert proc.returncode == 0, proc.stderr
        assert len(result["runs"]) == 2 and next(r for r in result["runs"] if r["id"] == 1)["landing"]
        by_name = {row["test"]: row for row in result["tests"]}
        assert by_name["m::recovers"]["failures"] == 2
        assert by_name["m::recovers"]["main_failures"] == 1
        assert by_name["m::recovers"]["rerun_passes_same_tested_sha"] == 1
        assert by_name["m::recovers"]["max_seconds"] == 2.75
        assert by_name["m::not_run_again"]["rerun_passes"] == 0
        assert by_name["m::retried"]["in_job_retry_passes"] == 1 and by_name["m::retried"]["max_seconds"] == 7
        assert by_name["m::audit_retry"]["in_job_retry_passes"] == 1
        assert not result["collection"]["partial"]
        before = (tmp_path / "calls").read_text()
        proc, cached = invoke(tmp_path, "--max-requests", "0")
        assert proc.returncode == 0 and cached["tests"] == result["tests"]
        assert (tmp_path / "calls").read_text() == before


    def test_missing_log_and_exhausted_budget_cannot_report_complete(self):
        tmp_path = self.tmp
        install(tmp_path, {
            f"workflows/ci-pr.yml/runs?created={WINDOW}&{PAGE}": page("workflow_runs", run(1, "pr")),
            f"workflows/ci-main.yml/runs?created={WINDOW}&{PAGE}": page("workflow_runs"),
            f"runs/1/attempts/1/jobs?{PAGE}": page("jobs", worker(11, "failure")),
            f"runs/1/artifacts?{PAGE}": page("artifacts"),
        })
        proc, result = invoke(tmp_path, "--max-requests", "4")
        assert proc.returncode == 2
        assert result["collection"]["partial"] is True
        assert result["coverage"][0]["source"] == "missing"
        assert "budget exhausted" in result["issues"][0]


if __name__ == "__main__":
    unittest.main()
