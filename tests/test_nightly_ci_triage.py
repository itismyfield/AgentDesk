"""Execute the privileged triage entrypoint against a persistent, offline GitHub fake."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[1]
REPO = "itismyfield/AgentDesk"
TITLE = "[ci-red] CI Nightly 실패(main)"
NS = "<!-- agentdesk:ci-nightly:main -->"
MOCK = r'''
import json, os, pathlib, re, sys
p = pathlib.Path(os.environ["FAKE_STATE"])
s = json.loads(p.read_text())
a = sys.argv[1:]
s["calls"].append([pathlib.Path(sys.argv[0]).name, *a])
def finish(value=None, code=0):
    p.write_text(json.dumps(s))
    if value is not None: print(value)
    sys.exit(code)
def write(kind):
    s["writes"].append(kind)
    finish(code=1 if s.get("fail_after") == kind else 0)
def number_of(issue):
    # "@@raw:<literal>" seeds a JSON number spelling Python cannot emit, such as 7e0.
    try: return float(str(issue.get("number")).removeprefix("@@raw:"))
    except ValueError: return None
repo = "itismyfield/AgentDesk"
if pathlib.Path(sys.argv[0]).name == "curl":
    assert a == ["--silent", "--show-error", "--output", "/dev/null", "--write-out", "%{http_code}",
        "--request", "POST", "--connect-timeout", "5", "--max-time", "15",
        "https://agentdesk.example/api/github/repos/itismyfield/AgentDesk/sync"], a
    s["writes"].append("sync")
    finish(s.get("sync_status", "200"), s.get("sync_rc", 0))
if a[0] == "api":
    assert a[-1] == "--paginate", a
    endpoint = a[1]
    if endpoint == f"/repos/{repo}/issues?state=all&per_page=100":
        kind, values = "issues", s["issues"]
    elif re_match := re.fullmatch(f"/repos/{repo}/issues/([0-9][-+.0-9eE]*)/comments\\?per_page=100", endpoint):
        kind = "comments"  # Unlike the CLI, REST resolves a decimal spelling of the number.
        values = next(i for i in s["issues"]
            if isinstance(i, dict) and number_of(i) == float(re_match[1]))["comments"]
    else: raise AssertionError(a)
    if s.get("fail_read") == kind: finish("[]", 1)
    if s.get("bad_json") == kind: finish("not-json")
    if s.get("bad_shape") == kind: finish('{}')
    # Deliberately emit multiple JSON pages; a marker may only exist on the last.
    finish("\n".join(re.sub('"@@raw:(.*?)"', r"\1", json.dumps(values[i:i+2]))
        for i in range(0, len(values), 2)) or "[]")
if a[:2] == ["label", "create"]:
    expected = {"ci-red": ["B60205", "Main branch CI red triage issue"],
        "agent:project-agentdesk": ["1D76DB", "Assigned to project-agentdesk"]}
    color, desc = expected[a[2]]
    assert a == ["label", "create", a[2], "--repo", repo, "--color", color, "--description", desc, "--force"], a
    if s.get("fail_before") == "label": finish(code=1)
    write("label")
assert a[0] == "issue", a
kind = a[1]
if s.get("fail_before") == kind: finish(code=1)
if kind == "create":
    assert a[:6] == ["issue", "create", "--repo", repo, "--title", "[ci-red] CI Nightly 실패(main)"], a
    assert a[6] == "--body-file" and a[8:] == ["--label", "ci-red", "--label", "agent:project-agentdesk"], a
    s["issues"].append(dict(number=7, state="open", title=a[5], body=pathlib.Path(a[7]).read_text(), comments=[]))
else:
    assert a[3:5] == ["--repo", repo], a
    if not re.fullmatch(r"#?\d+", a[2]):  # gh 2.97.0 ParseIssueFromArg rejects "7.0".
        print(f'invalid issue format: "{a[2]}"', file=sys.stderr); finish(code=1)
    issue = next(i for i in s["issues"]
        if isinstance(i, dict) and number_of(i) == float(a[2].lstrip("#")))
    if kind == "reopen":
        assert len(a) == 5, a
        issue["state"] = "open"
    elif kind == "comment":
        assert a[5] == "--body-file" and len(a) == 7, a
        issue["comments"].append({"body": pathlib.Path(a[6]).read_text()})
    else: raise AssertionError(a)
write(kind)
'''


def event():
    return {"action": "completed", "repository": {"full_name": REPO}, "workflow_run": {
        "name": "CI Nightly", "workflow_id": 1, "id": 200, "run_attempt": 1,
        "head_repository": {"full_name": REPO}, "head_branch": "main", "status": "completed",
        "event": "schedule", "conclusion": "failure", "head_sha": "a" * 40}}


def marker(run=200, attempt=1):
    return f"<!-- agentdesk:ci-nightly:main:{REPO}:{run}:{attempt} -->"


class NightlyTriage(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name)
        self.state = self.path / "state.json"
        self.save({"issues": [], "calls": [], "writes": []})
        for tool in ("gh", "curl"):
            executable = self.path / tool
            executable.write_text(f"#!{sys.executable}\n" + MOCK)
            executable.chmod(0o755)
        self.payload = event()

    def save(self, state):
        self.state.write_text(json.dumps(state))

    def load(self):
        return json.loads(self.state.read_text())

    def run_entry(self, *, success=True, sync=False):
        before = len(self.load()["writes"])
        payload_path = self.path / "event.json"
        payload_path.write_text(json.dumps(self.payload))
        env = {"PATH": f"{self.path}:{os.environ['PATH']}", "HOME": str(self.path),
            "GITHUB_REPOSITORY": REPO, "GITHUB_EVENT_PATH": str(payload_path),
            "GITHUB_RUN_ATTEMPT": "99", "GITHUB_TOKEN": "offline-fake", "FAKE_STATE": str(self.state)}
        if sync:
            env["AGENTDESK_API_URL"] = "https://agentdesk.example"
        result = subprocess.run(["bash", str(ROOT / "scripts/main-ci-triage.sh")],
            env=env, text=True, capture_output=True, timeout=20)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return self.load()["writes"][before:]

    def seed(self, *, state="open", body=None, comments=None):
        data = self.load()
        data["issues"] = [{"number": 7, "title": TITLE, "state": state,
            "body": body if body is not None else NS, "comments": comments or []}]
        self.save(data)

    def test_first_failure_schedule_and_dispatch_without_history_or_logs(self):
        for upstream in ("schedule", "workflow_dispatch"):
            with self.subTest(upstream=upstream):
                self.save({"issues": [], "calls": [], "writes": []})
                self.payload["workflow_run"]["event"] = upstream
                self.assertEqual(self.run_entry(sync=True), ["label", "label", "create", "sync"])
                self.assertIn(marker(), self.load()["issues"][0]["body"].splitlines())
                self.assertEqual(self.run_entry(sync=True), [])

    def test_new_run_and_upstream_attempt_are_distinct_from_triage_attempt(self):
        self.run_entry()
        self.payload["workflow_run"]["id"] = 201
        self.assertEqual(self.run_entry(), ["comment"])
        self.payload["workflow_run"]["run_attempt"] = 2
        self.assertEqual(self.run_entry(), ["comment"])
        self.assertIn(marker(201, 2), self.load()["issues"][0]["comments"][-1]["body"])
        self.assertEqual(self.run_entry(sync=True), [])

    def test_exact_marker_on_last_page_and_closed_replay(self):
        self.seed(state="closed", comments=[{"body": "noise"}] * 4 + [{"body": marker()}])
        self.assertEqual(self.run_entry(sync=True), [])
        self.seed(body=NS + "\n" + marker() + "extra")
        self.assertEqual(self.run_entry(), ["comment"])

    def test_closed_new_failure_reopens_before_comment(self):
        self.seed(state="closed")
        self.assertEqual(self.run_entry(), ["reopen", "comment"])
        self.assertEqual(self.load()["issues"][0]["state"], "open")

    def test_applied_write_then_error_is_not_repeated(self):
        for kind in ("create", "comment", "reopen"):
            with self.subTest(kind=kind):
                self.save({"issues": [], "calls": [], "writes": [], "fail_after": kind})
                if kind != "create":
                    self.seed(state="closed" if kind == "reopen" else "open")
                writes = self.run_entry(success=False)
                self.assertEqual(writes[-1], kind)
                state = self.load(); state.pop("fail_after"); self.save(state)
                self.assertEqual(self.run_entry(), ["comment"] if kind == "reopen" else [])

    def test_read_errors_and_ambiguity_never_write(self):
        for key in ("fail_read", "bad_json", "bad_shape"):
            for endpoint in ("issues", "comments"):
                with self.subTest(key=key, endpoint=endpoint):
                    self.save({"issues": [], "calls": [], "writes": [], key: endpoint})
                    self.seed()
                    self.assertEqual(self.run_entry(success=False), [])
        self.save({"issues": [], "calls": [], "writes": []}); self.seed()
        state = self.load(); state["issues"] *= 2; self.save(state)
        self.assertEqual(self.run_entry(success=False), [])

    def test_partial_canonical_identity_never_adopts_or_creates(self):
        for state in ("open", "closed"):
            for partial in ("title_only", "namespace_only"):
                with self.subTest(state=state, partial=partial):
                    self.seed(state=state, body="unrelated" if partial == "title_only" else NS)
                    data = self.load()
                    if partial == "namespace_only":
                        data["issues"][0]["title"] = "Unrelated issue"
                    self.save(data)
                    self.assertEqual(self.run_entry(success=False, sync=True), [])

    def test_canonical_plus_partial_conflict_and_true_ambiguity_never_write(self):
        for collision in ("title_only", "namespace_only", "canonical"):
            with self.subTest(collision=collision):
                self.seed()
                data = self.load()
                data["issues"].insert(0, {"number": 8, "state": "closed",
                    "title": "Unrelated issue" if collision == "namespace_only" else TITLE,
                    "body": "unrelated" if collision == "title_only" else NS, "comments": []})
                data["issues"].insert(1, {"number": 9, "state": "open",
                    "title": "Ordinary issue", "body": None, "comments": []})
                self.save(data)  # Canonical record is on the next fake API page.
                self.assertEqual(self.run_entry(success=False, sync=True), [])

    def test_malformed_issue_records_on_any_page_fail_before_filtering(self):
        ordinary = {"number": 8, "state": "open", "title": "Ordinary issue", "body": None}
        malformed = [None, {}]
        for field in ("title", "body", "number", "state"):
            malformed.append({key: value for key, value in ordinary.items() if key != field})
        for field, value in [("title", None), ("body", 9), ("number", 0), ("number", -1),
                ("number", 1.5), ("number", "8"), ("number", True), ("state", "unknown"),
                ("pull_request", True)]:
            malformed.append({**ordinary, field: value})
        for bad in malformed:
            for position in ("only", "early", "late"):
                with self.subTest(record=bad, position=position):
                    self.seed()
                    data = self.load()
                    if position == "only":
                        data["issues"] = [bad]
                    elif position == "early":
                        data["issues"] = [bad, ordinary, *data["issues"]]
                    else:
                        data["issues"] = [*data["issues"], ordinary, bad]
                    self.save(data)
                    self.assertEqual(self.run_entry(success=False, sync=True), [])

    def test_valid_nullable_unrelated_and_later_canonical_pages(self):
        ordinary = {"number": 8, "state": "open", "title": "Ordinary issue", "body": None}
        data = self.load(); data["issues"] = [ordinary]; self.save(data)
        self.assertEqual(self.run_entry(), ["label", "label", "create"])
        self.seed()
        data = self.load()
        data["issues"] = [ordinary, {**ordinary, "number": 9}, *data["issues"]]
        data["issues"].insert(0, {**ordinary, "number": 10, "title": TITLE,
            "body": NS, "pull_request": {"url": "https://github.example/pull/10"}})
        self.save(data)
        self.assertEqual(self.run_entry(), ["comment"])

    def numeric_arguments(self):
        """Issue numbers exactly as spelled in the recorded argv, independent of fake state."""
        selected = []
        for call in self.load()["calls"]:
            if call[:2] == ["gh", "api"] and "/comments?" in call[2]:
                selected.append(call[2].split("/issues/")[1].split("/")[0])
            elif call[:2] == ["gh", "issue"] and call[2] != "create":
                selected.append(call[3])
        return selected

    def test_validated_integral_decimal_number_reaches_the_cli_as_an_integer(self):
        unrelated = {"number": 8, "state": "open", "title": "Ordinary issue", "body": None,
            "comments": []}
        for spelling in (7.0, "@@raw:7e0"):
            for state, writes in (("open", ["comment"]), ("closed", ["reopen", "comment"])):
                with self.subTest(number=spelling, state=state):
                    self.save({"issues": [], "calls": [], "writes": []})
                    self.seed(state=state)
                    data = self.load()
                    data["issues"][0]["number"] = spelling
                    data["issues"].insert(0, dict(unrelated))
                    self.save(data)
                    self.assertEqual(self.run_entry(sync=True), writes + ["sync"])
                    self.assertEqual(self.numeric_arguments(), ["7"] * (len(writes) + 1))
                    self.assertEqual(self.load()["issues"][1]["state"], "open")
        # An old exact marker still replays entirely write-free at the same spelling.
        self.save({"issues": [], "calls": [], "writes": []})
        self.seed(state="closed", comments=[{"body": marker()}])
        data = self.load(); data["issues"][0]["number"] = 7.0; self.save(data)
        self.assertEqual(self.run_entry(sync=True), [])
        self.assertEqual(self.numeric_arguments(), ["7"])

    def test_mandatory_write_errors_propagate(self):
        for kind in ("label", "create", "comment", "reopen"):
            with self.subTest(kind=kind):
                self.save({"issues": [], "calls": [], "writes": [], "fail_before": kind})
                if kind in ("comment", "reopen"):
                    self.seed(state="closed" if kind == "reopen" else "open")
                self.run_entry(success=False)

    def test_sync_failure_is_best_effort_and_replay_does_not_retry_sync(self):
        for extras in ({"sync_status": "503"}, {"sync_rc": 1}):
            self.save({"issues": [], "calls": [], "writes": [], **extras})
            self.assertEqual(self.run_entry(sync=True)[-1], "sync")
            self.assertEqual(self.run_entry(sync=True), [])

    def test_helper_rejects_wrong_provenance_and_event_combinations(self):
        changes = [("head_branch", "other"), ("name", "CI Other"), ("event", "push"),
            ("event", "pull_request"), ("status", "in_progress"),
            ("head_repository", {"full_name": "fork/AgentDesk"}),
            ("conclusion", "success"), ("conclusion", "cancelled")]
        for key, value in changes:
            with self.subTest(key=key, value=value):
                self.payload = event(); self.payload["workflow_run"][key] = value
                self.assertEqual(self.run_entry(), [])
        self.payload = event(); self.payload["repository"]["full_name"] = "other/repo"
        self.assertEqual(self.run_entry(), [])
        self.payload = event(); self.payload["action"] = "requested"
        self.assertEqual(self.run_entry(), [])
        self.payload = event(); self.payload["workflow_run"]["name"] = "CI Main"
        self.assertEqual(self.run_entry(), [])  # CI Main + schedule

    def test_missing_or_malformed_identity_is_error_without_writes(self):
        for field in ("run_attempt", "id", "head_repository", "head_sha"):
            self.payload = event(); del self.payload["workflow_run"][field]
            self.assertEqual(self.run_entry(success=False), [])
        for value in (0, -1, 1.5, "1"):
            self.payload = event(); self.payload["workflow_run"]["run_attempt"] = value
            self.assertEqual(self.run_entry(success=False), [])

    def test_main_recovery_ignores_canonical_nightly_issue(self):
        # Execute main's real recovery self-test with a nightly issue injected in the GitHub list.
        source = (ROOT / "scripts/main-ci-triage.sh").read_text()
        source = source.replace('cat "$scenario_dir/open-issues.json"',
            'jq \' . + [{"number":999,"title":"[ci-red] CI Nightly 실패(main)"}] \' "$scenario_dir/open-issues.json"')
        source = source.replace('cmd="${1-}"',
            '[[ "${1-}" != issue || "${3-}" != 999 ]] || exit 92\ncmd="${1-}"')
        script = self.path / "main-ci-triage.sh"
        (self.path / "ci").mkdir()
        (self.path / "ci/real-failure-predicate.sh").write_text(
            (ROOT / "scripts/ci/real-failure-predicate.sh").read_text())
        script.write_text(source)
        result = subprocess.run(["bash", str(script), "--self-test"],
            capture_output=True, text=True, timeout=120,
            env={"PATH": os.environ["PATH"], "HOME": str(self.path)})
        self.assertEqual(result.returncode, 0, result.stderr)


class NightlyWorkflow(unittest.TestCase):
    def test_workflow_structure_and_guard_truth_table(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/main-ci-triage.yml").read_text())
        self.assertEqual(workflow[True]["workflow_run"], {"workflows": ["CI Main", "CI Nightly"], "types": ["completed"]})
        self.assertEqual(workflow["permissions"], {"actions": "read", "contents": "read", "issues": "write"})
        self.assertNotIn("concurrency", workflow)
        nightly = workflow["jobs"]["nightly"]
        self.assertEqual(nightly["concurrency"], {"group": "agentdesk-nightly-failure-triage", "cancel-in-progress": False, "queue": "max"})
        self.assertEqual(workflow["jobs"]["triage"]["concurrency"],
            {"group": "main-ci-triage-${{ github.event.workflow_run.id }}", "cancel-in-progress": False})
        for job in workflow["jobs"].values():
            self.assertEqual(job["steps"][0]["with"], {"repository": REPO, "ref": "${{ github.sha }}", "persist-credentials": False})
            self.assertEqual(job["steps"][-1]["run"], "./scripts/main-ci-triage.sh")
        cases = [(event(), False, True)]
        for name, upstream, main, night in [("CI Nightly", "workflow_dispatch", False, True),
                ("CI Main", "push", True, False), ("CI Main", "schedule", False, False),
                ("CI Nightly", "push", False, False), ("CI Nightly", "pull_request", False, False)]:
            payload = event(); payload["workflow_run"].update(name=name, event=upstream)
            cases.append((payload, main, night))
        for key, value in [("name", "other"), ("head_branch", "other"), ("status", "in_progress"),
                ("conclusion", "success"), ("conclusion", "cancelled"), ("id", 0), ("run_attempt", 0),
                ("head_repository", {"full_name": "fork/AgentDesk"})]:
            payload = event(); payload["workflow_run"][key] = value; cases.append((payload, False, False))
        for key, value in [("action", "requested"), ("repository", {"full_name": "other/repo"})]:
            payload = event(); payload[key] = value; cases.append((payload, False, False))
        for payload, main, night in cases:
            for job, expected in [("triage", main), ("nightly", night)]:
                def value(match):
                    path = match[0].split('.')[1:]
                    obj = {"repository": REPO, "event": payload}
                    for part in path: obj = obj.get(part) if isinstance(obj, dict) else None
                    return repr(obj)
                expression = re.sub(r'github(?:\.[a-z_]+)+', value, workflow["jobs"][job]["if"])
                expression = expression.replace('&&', ' and ').replace('||', ' or ')
                self.assertEqual(eval(expression, {"__builtins__": {}}, {}), expected, (job, payload))


if __name__ == "__main__":
    unittest.main()
