from __future__ import annotations

import copy
import importlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
P = PROGRESS = importlib.import_module("giant_file_progress")
G = P.inventory
META_ROOT = ("shrink", "team", "2026-08-31", "#4712", "")


class GiantFileProgressTest(unittest.TestCase):
    def fixture(self, size=1200):
        base = {"modules": {"src/fixture.rs": 1200}, "pins": {"src/fixture.rs": 1100},
                "overdue": ["src/fixture.rs"], "registrations": {"src/fixture.rs": META_ROOT}}
        candidate = copy.deepcopy(base)
        candidate["modules"]["src/fixture.rs"] = size
        facts = {"changed": {"src/fixture.rs"}, "additions": 1}
        return base, candidate, facts

    def test_new_and_growing_giants_fail_without_wiring_slack(self):
        for old, new, expected in ((1200, 1201, True), (1200, 1200, False),
                (1200, 1199, False), (1200, 999, False), (999, 1000, True),
                (0, 1000, True), (0, 999, False)):
            with self.subTest(old=old, new=new):
                base, candidate, facts = self.fixture(new)
                base["modules"]["src/fixture.rs"] = old
                self.assertEqual(bool(P.pr_evaluation(base, candidate, facts)[1]), expected)

    def test_overdue_one_line_shrink_passes_with_warning(self):
        base, candidate, facts = self.fixture(1199)
        self.assertEqual(P.pr_evaluation(base, candidate, facts)[1], [])
        self.assertIn("under 200-line giant progress", "; ".join(P.planning_warnings(base, candidate, facts)))

    def test_ledger_deadline_and_progress_form_only_warn(self):
        base, candidate, facts = self.fixture()
        candidate.update(overdue=[], registrations={})
        facts.update(changed=set(P.LEDGER) | {f"src/p{i}.rs" for i in range(21)}, additions=900)
        self.assertEqual(P.pr_evaluation(base, candidate, facts)[1], [])
        warnings = "; ".join(P.planning_warnings(base, candidate, facts))
        for fragment in ("ledger changed", "overdue planning set changed", "20 files or 800"):
            self.assertIn(fragment, warnings)

    def test_cap_increase_cannot_be_authorized_by_planning_changes(self):
        base, candidate, facts = self.fixture(1199)
        candidate["pins"]["src/fixture.rs"] = 1101
        self.assertIn("giant cap increased", "; ".join(P.pr_evaluation(base, candidate, facts)[1]))
        candidate["pins"].clear()
        self.assertEqual(P.pr_evaluation(base, candidate, facts)[1], [])

    def test_invalid_pin_authority_and_loader_exception_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pin = root / P.GIANT_PIN
            pin.parent.mkdir(parents=True)
            for text in ('[giant_file_ratchet]\n"src/a.rs" = 1200\n', '[giant_file_ratchet\n',
                         '[giant_file_ratchet]\n"src/a.rs" = true\n', ''):
                pin.write_text(text)
                if text.endswith('1200\n'):
                    self.assertEqual(P.base_pins(root), {"src/a.rs": 1200})
                else:
                    with self.assertRaises(G.ParseError):
                        P.base_pins(root)
            pin.unlink()
            with self.assertRaises(G.ParseError):
                P.base_pins(root)
            pin.write_text('[giant_file_ratchet]\n"src/a.rs" = 1200\n')
            with mock.patch("audit_maintainability.checks.giant_file_ratchet.load_giant_baseline",
                            side_effect=RuntimeError("loader crashed")), self.assertRaisesRegex(G.ParseError, "loader crashed"):
                P.base_pins(root)

    def test_main_records_debt_without_zero_requirement(self):
        self.assertEqual(P.main_record({"overdue": ["src/a.rs"]}),
                         {"overdue": ["src/a.rs"], "overdue_count": 1})

    def test_evidence_has_stable_json_and_atomic_destination(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(P, "EVIDENCE", Path(directory) / "evidence.json"):
            P.write_evidence({"verdict": "progress-pass", "schema": 1})
            self.assertEqual(json.loads(P.EVIDENCE.read_text()), {"verdict": "progress-pass", "schema": 1})
            self.assertFalse(P.EVIDENCE.with_suffix(".tmp").exists())


class GiantFileCandidateBaseTest(unittest.TestCase):
    """Real Git DAG regressions for #5904/#5905's stale event-base failure."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.evidence = Path(self.temp.name) / "evidence.json"
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "candidate@example.invalid")
        self.git("config", "user.name", "Candidate Base Test")
        self.put(P.EVALUATOR, Path(P.__file__).read_text(encoding="utf-8"))
        self.put(P.REGISTRY, "# fixture registry\n")
        self.put(P.GIANT_PIN, '[giant_file_ratchet]\n"src/fixture.rs" = 1200\n')
        self.put(P.TRANSITION, "# unchanged transition\n")
        self.put("src/fixture.rs", "pub fn fixture() {}\n" * 1200)
        self.event_base = self.commit("event base")
        self.git("checkout", "-qb", "pr")
        self.put("docs/pr.txt", "PR-only change\n")
        self.head = self.commit("PR head")
        self.git("checkout", "-q", "main")
        # Another PR shrinks a giant before GitHub synthesizes our candidate.
        self.put("src/fixture.rs", "pub fn fixture() {}\n" * 1100)
        self.put("docs/unrelated.txt", "another PR\n")
        self.comparison_base = self.commit("main advanced")
        self.git("merge", "--no-ff", "-qm", "candidate", "pr")
        self.candidate = self.git("rev-parse", "HEAD")
        self.parents = [self.candidate, self.comparison_base, self.head]
        patch = mock.patch.object(P, "ROOT", self.repo)
        patch.start()
        self.addCleanup(patch.stop)

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, check=True,
                              capture_output=True, text=True).stdout.strip()

    def put(self, path, text):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def resolve(self, *, event_base=None, head=None, parents=None):
        return P.pr_comparison_base(
            self.candidate, event_base or self.event_base, head or self.head,
            self.parents if parents is None else parents)

    def run_candidate(self, *, event_base=None, head=None, candidate=None):
        env = {"GFP_EVENT_NAME": "pull_request", "GFP_REPOSITORY": "itismyfield/AgentDesk",
               "GFP_HEAD_REPOSITORY": "itismyfield/AgentDesk",
               "GFP_CANDIDATE_SHA": candidate or self.candidate,
               "GFP_BASE_SHA": event_base or self.event_base,
               "GFP_HEAD_SHA": head or self.head}
        # This fixture isolates provenance, archive/diff selection and evidence;
        # real inventory parsing remains covered by the existing integration tests.
        def snapshot(root, evaluation_date=None):
            return {"overdue": ["src/fixture.rs"],
                    "modules": {"src/fixture.rs": len((root / "src/fixture.rs").read_text().splitlines())},
                    "registrations": {"src/fixture.rs": META_ROOT}}
        with mock.patch.dict(P.os.environ, env, clear=True), mock.patch.object(
                P, "EVIDENCE", self.evidence), mock.patch.object(
                G, "giant_file_snapshot", side_effect=snapshot) as scan, mock.patch.object(
                P, "archive", wraps=P.archive) as archive, mock.patch.object(
                P, "diff_facts", wraps=P.diff_facts) as diff, redirect_stderr(io.StringIO()):
            rc = P.main()
        return rc, json.loads(self.evidence.read_text()), scan, archive, diff

    def test_equal_and_advanced_event_bases_resolve_to_candidate_parent(self):
        self.assertEqual(self.resolve(event_base=self.comparison_base), self.comparison_base)
        self.assertEqual(self.resolve(), self.comparison_base)

    def test_reverse_ancestry_and_unrelated_base_are_rejected(self):
        tree = self.git("rev-parse", self.comparison_base + "^{tree}")
        newer = self.git("commit-tree", tree, "-p", self.comparison_base, "-m", "newer")
        unrelated = self.git("commit-tree", tree, "-m", "unrelated root")
        for event_base in (newer, unrelated):
            with self.subTest(event_base=event_base), self.assertRaisesRegex(RuntimeError, "provenance mismatch"):
                self.resolve(event_base=event_base)

    def test_shape_order_candidate_and_head_bindings_are_not_relaxed(self):
        bad = [[], [self.candidate], [self.candidate, self.comparison_base],
               [self.candidate, self.comparison_base, self.head, self.event_base],
               [self.candidate, self.head, self.comparison_base],
               [self.head, self.comparison_base, self.head],
               [self.candidate, self.comparison_base, self.event_base]]
        for parents in bad:
            with self.subTest(parents=parents), self.assertRaisesRegex(RuntimeError, "provenance mismatch"):
                self.resolve(parents=parents)
        with self.assertRaisesRegex(RuntimeError, "provenance mismatch"):
            self.resolve(head=self.event_base)

    def test_missing_object_and_git_operational_error_fail_closed(self):
        with self.assertRaises(subprocess.CalledProcessError):
            self.resolve(event_base="0" * 40)
        with mock.patch.object(P, "git", side_effect=subprocess.CalledProcessError(128, "git")):
            with self.assertRaises(subprocess.CalledProcessError):
                self.resolve()

    def test_main_uses_actual_parent_for_archive_diff_and_evidence(self):
        rc, evidence, scan, archive, diff = self.run_candidate()
        self.assertEqual(rc, 0, evidence)
        self.assertEqual(evidence["selector"], "pr_ordinary_no_regression")
        self.assertEqual(evidence["event_base_sha"], self.event_base)
        self.assertEqual(evidence["comparison_base_sha"], self.comparison_base)
        self.assertEqual(evidence["merge_first_parent"], self.comparison_base)
        self.assertEqual(evidence["base_tree"], self.git("rev-parse", self.comparison_base + "^{tree}"))
        self.assertNotEqual(evidence["base_tree"], self.git("rev-parse", self.event_base + "^{tree}"))
        self.assertEqual(evidence["changed_files"], 1)
        self.assertEqual(evidence["retired"], [])
        self.assertEqual(scan.call_count, 2)
        self.assertEqual([call.args[0] for call in archive.call_args_list],
                         [self.candidate, self.comparison_base])
        diff.assert_called_once_with(self.comparison_base, self.candidate)

    def test_later_origin_advance_does_not_change_candidate_evidence(self):
        self.git("update-ref", "refs/remotes/origin/main", self.comparison_base)
        first = self.run_candidate()
        # Even a completely unrelated mutable ref must not participate in a
        # verdict already pinned to the event candidate's immutable objects.
        tree = self.git("rev-parse", self.candidate + "^{tree}")
        unrelated = self.git("commit-tree", tree, "-m", "unrelated tip")
        self.git("update-ref", "refs/remotes/origin/main", unrelated)
        second = self.run_candidate()
        self.assertEqual((first[0], second[0]), (0, 0))
        self.assertEqual(first[1], second[1])

    def test_invalid_provenance_fails_before_snapshot_and_diff(self):
        for options in ({"head": self.event_base}, {"event_base": self.head},
                        {"candidate": self.head}):
            with self.subTest(options=options):
                rc, evidence, scan, archive, diff = self.run_candidate(**options)
                self.assertEqual(rc, 2, evidence)
                self.assertEqual(evidence["verdict"], "fail")
                scan.assert_not_called()
                archive.assert_not_called()
                diff.assert_not_called()

    def test_advanced_base_does_not_hide_giant_growth_or_cap_raise(self):
        tree = self.git("rev-parse", self.candidate + "^{tree}")
        for path, text, reason in (
                ("src/fixture.rs", "pub fn fixture() {}\n" * 1101, "new or growing giant"),
                (P.GIANT_PIN, '[giant_file_ratchet]\n"src/fixture.rs" = 1201\n', "giant cap increased")):
            with self.subTest(path=path):
                self.git("checkout", "-qf", "--detach", self.candidate)
                self.put(path, text)
                self.git("add", "-A")
                changed_tree = self.git("write-tree")
                changed = self.git("commit-tree", changed_tree, "-p", self.comparison_base,
                                   "-p", self.head, "-m", "invalid integration candidate")
                self.git("checkout", "-qf", "--detach", changed)
                rc, evidence, _, _, _ = self.run_candidate(candidate=changed)
                self.assertEqual(rc, 2, evidence)
                self.assertIn(reason, evidence["reason"])
                self.assertEqual(evidence["event_base_sha"], self.event_base)
                self.assertEqual(evidence["comparison_base_sha"], self.comparison_base)

    def test_giant_pinned_at_base_rejects_any_growth(self):
        self.git("checkout", "-q", "main")
        self.put(P.GIANT_PIN, '[giant_file_ratchet]\n"src/fixture.rs" = 1100\n')
        pinned_base = self.commit("pin the fixture giant")
        for lines, expected_rc in ((1099, 0), (1100, 0), (1101, 2)):
            with self.subTest(lines=lines):
                self.git("checkout", "-qf", "--detach", pinned_base)
                self.put("docs/pr.txt", "PR-only change\n")
                self.put("src/fixture.rs", "pub fn fixture() {}\n" * lines)
                self.git("add", "-A")
                changed = self.git("commit-tree", self.git("write-tree"), "-p", pinned_base,
                                   "-p", self.head, "-m", "pinned integration candidate")
                self.git("checkout", "-qf", "--detach", changed)
                rc, evidence, _, _, _ = self.run_candidate(candidate=changed)
                self.assertEqual(rc, expected_rc, evidence)
                self.assertEqual(evidence["comparison_base_sha"], pinned_base)
                if expected_rc:
                    self.assertIn(f"grew to {lines} > base 1100", evidence["reason"])
                else:
                    self.assertEqual(evidence["selector"], "pr_ordinary_no_regression")


class GiantMeasurementFailureTest(unittest.TestCase):
    setUp, git, put, commit, run_candidate = (GiantFileCandidateBaseTest.setUp, GiantFileCandidateBaseTest.git,
        GiantFileCandidateBaseTest.put, GiantFileCandidateBaseTest.commit, GiantFileCandidateBaseTest.run_candidate)
    def test_snapshot_exception_fails_and_writes_evidence(self):
        env = {"GFP_EVENT_NAME": "pull_request", "GFP_REPOSITORY": "itismyfield/AgentDesk",
               "GFP_HEAD_REPOSITORY": "itismyfield/AgentDesk", "GFP_CANDIDATE_SHA": self.candidate,
               "GFP_BASE_SHA": self.event_base, "GFP_HEAD_SHA": self.head}
        for exception in (RuntimeError("measurement failed"), ValueError("bad measure"), SyntaxError("broken checker")):
            with self.subTest(exception=type(exception).__name__), mock.patch.dict(P.os.environ, env, clear=True), \
                    mock.patch.object(P, "EVIDENCE", self.evidence), mock.patch.object(G, "giant_file_snapshot", side_effect=exception), \
                    redirect_stderr(io.StringIO()):
                self.assertEqual(P.main(), 2)
            payload = json.loads(self.evidence.read_text())
            self.assertEqual(payload["verdict"], "fail")
            self.assertIn(str(exception), payload["reason"])

    def test_overdue_one_line_shrink_through_main_passes_and_emits_warning(self):
        self.git("checkout", "-qf", "--detach", self.comparison_base)
        self.put("src/fixture.rs", "pub fn fixture() {}\n" * 1099)
        self.put("docs/pr.txt", "PR-only change\n")
        self.git("add", "-A")
        changed = self.git("commit-tree", self.git("write-tree"), "-p", self.comparison_base,
                           "-p", self.head, "-m", "one line progress")
        self.git("checkout", "-qf", "--detach", changed)
        rc, evidence, _, _, _ = self.run_candidate(candidate=changed)
        self.assertEqual(rc, 0, evidence)
        self.assertTrue(any("under 200-line" in warning for warning in evidence["warnings"]))


class GiantRealSnapshotTest(unittest.TestCase):
    git, put, commit = (GiantFileCandidateBaseTest.git, GiantFileCandidateBaseTest.put,
                        GiantFileCandidateBaseTest.commit)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.evidence = Path(self.temp.name) / "evidence.json"
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "snapshot@example.invalid")
        self.git("config", "user.name", "Snapshot Fixture")
        self.put("src/fixture.rs", "pub fn fixture() {}\n" * 1200)
        self.put(P.EVALUATOR, Path(P.__file__).read_text())
        self.put(P.GIANT_PIN, '[giant_file_ratchet]\n"src/fixture.rs" = 1200\n')
        self.put(P.REGISTRY, 'grandfathered=[]\ngrandfathered_baseline_paths=[]\n'
            '[[entry]]\nfile="src/fixture.rs"\ndecision="shrink"\nowner="team"\n'
            'deadline="2026-08-31"\ndecompose_issue="#4712"\n')
        self.put(P.TRANSITION, "# no transitions\n")
        self.put(P.METADATA, json.dumps({"schema_version": 2,
            "refreshed_at": G.now_utc().isoformat(timespec="seconds").replace("+00:00", "Z"),
            "ratchets": {"closed_deadline_entries": 0, "transition_list_entries": 0},
            "issues": [{"number": 4712, "state": "open", "title": "split fixture",
                        "owners": ["team"], "files": ["src/fixture.rs"]}]}))
        self.base = self.commit("base")
        patch = mock.patch.object(P, "ROOT", self.repo)
        patch.start()
        self.addCleanup(patch.stop)

    def run_pr(self, files):
        self.git("checkout", "-qf", "--detach", self.base)
        self.put("docs/change.txt", "fixture change\n")
        for path, text in files.items():
            if isinstance(text, bytes):
                (self.repo / path).write_bytes(text)
            else:
                self.put(path, text)
        head = self.commit("head")
        self.git("checkout", "-qf", "--detach", self.base)
        self.git("merge", "--no-ff", "-qm", "candidate", head)
        env = {"GFP_EVENT_NAME": "pull_request", "GFP_REPOSITORY": "itismyfield/AgentDesk",
               "GFP_HEAD_REPOSITORY": "trusted/AgentDesk", "GFP_CANDIDATE_SHA": self.git("rev-parse", "HEAD"),
               "GFP_BASE_SHA": self.base, "GFP_HEAD_SHA": head}
        err = io.StringIO()
        with mock.patch.dict(P.os.environ, env, clear=True), mock.patch.object(P, "EVIDENCE", self.evidence), redirect_stderr(err):
            rc = P.main()
        return rc, json.loads(self.evidence.read_text()), err.getvalue()

    def test_real_snapshot_one_line_shrink_and_equal_pass_growth_fails(self):
        for size, expected in ((1199, 0), (1200, 0), (1201, 2)):
            rc, evidence, err = self.run_pr({"src/fixture.rs": "pub fn fixture() {}\n" * size})
            self.assertEqual(rc, expected, (evidence, err))
            if expected:
                self.assertIn("new or growing giant", evidence["reason"])
            else:
                self.assertIn("WARNING:", err)

    def test_real_snapshot_new_giant_is_blocked(self):
        rc, evidence, _ = self.run_pr({"src/new.rs": "pub fn new() {}\n" * 1000})
        self.assertEqual(rc, 2, evidence)
        self.assertIn("new or growing giant", evidence["reason"])

    def test_real_snapshot_unreadable_source_measurement_fails(self):
        rc, evidence, _ = self.run_pr({"src/fixture.rs": b"\xff"})
        self.assertEqual(rc, 2, evidence)
        self.assertEqual(evidence["verdict"], "fail")

    def test_real_snapshot_metadata_format_changes_warn(self):
        registry = (self.repo / P.REGISTRY).read_text()
        rc, evidence, err = self.run_pr({P.REGISTRY: registry.replace('owner="team"', 'owner=""')})
        self.assertEqual(rc, 0, (evidence, err))
        self.assertIn("WARN", err)

    def test_real_snapshot_invalid_metadata_document_fails(self):
        rc, evidence, _ = self.run_pr({P.METADATA: "{broken"})
        self.assertEqual(rc, 2, evidence)
        self.assertEqual(evidence["verdict"], "fail")


class GiantProtectedMainDispatchTest(unittest.TestCase):
    setUp, git, put, commit = (GiantRealSnapshotTest.setUp, GiantRealSnapshotTest.git,
        GiantRealSnapshotTest.put, GiantRealSnapshotTest.commit)

    def run_main(self, **overrides):
        env = {"GFP_EVENT_NAME": "workflow_dispatch", "GFP_REPOSITORY": "itismyfield/AgentDesk",
               "GFP_CANDIDATE_SHA": self.base, "GFP_REF": "refs/heads/main",
               "GFP_REF_PROTECTED": "true"}
        env.update(overrides)
        with mock.patch.dict(P.os.environ, env, clear=True), mock.patch.object(
                P, "EVIDENCE", self.evidence), mock.patch.object(
                G, "giant_file_snapshot", wraps=G.giant_file_snapshot) as scan, mock.patch.object(
                P, "archive", wraps=P.archive) as archive, mock.patch.object(
                P, "diff_facts", wraps=P.diff_facts) as diff, redirect_stderr(io.StringIO()):
            rc = P.main()
        return rc, json.loads(self.evidence.read_text()), scan, archive, diff

    def test_protected_main_dispatch_records_same_debt_as_push(self):
        self.git("update-ref", "refs/remotes/origin/main", self.base)
        dispatch_rc, dispatch, scan, archive, diff = self.run_main()
        self.assertEqual(dispatch_rc, 0, dispatch)
        self.assertEqual(dispatch["verdict"], "progress-pass")
        self.assertEqual(dispatch["selector"], "main_no_regression_record")
        self.assertEqual(dispatch["overdue"], ["src/fixture.rs"])
        self.assertEqual(dispatch["overdue_count"], 1)
        self.assertEqual(scan.call_count, 1)
        archive.assert_called_once_with(self.base, mock.ANY)
        diff.assert_not_called()
        push_rc, push, _, _, _ = self.run_main(
            GFP_EVENT_NAME="push", GFP_REF="", GFP_REF_PROTECTED="")
        self.assertEqual(push_rc, 0, push)
        for field in ("selector", "verdict", "reason", "overdue", "overdue_count",
                      "candidate_overdue", "candidate_tree"):
            self.assertEqual(dispatch[field], push[field], field)

    def test_dispatch_rejects_wrong_ref_unprotected_branch_and_repository_before_scan(self):
        self.git("update-ref", "refs/remotes/origin/main", self.base)
        for overrides in ({"GFP_REF": "refs/heads/topic"}, {"GFP_REF": "refs/tags/main"},
                          {"GFP_REF": ""}, {"GFP_REF_PROTECTED": "false"},
                          {"GFP_REF_PROTECTED": ""},
                          {"GFP_REPOSITORY": "other/AgentDesk"}):
            with self.subTest(overrides=overrides):
                rc, evidence, scan, archive, diff = self.run_main(**overrides)
                self.assertEqual(rc, 2, evidence)
                self.assertEqual(evidence["verdict"], "fail")
                self.assertIn("dispatch requires protected main head", evidence["reason"])
                scan.assert_not_called()
                archive.assert_not_called()
                diff.assert_not_called()

    def test_dispatch_rejects_stale_candidate_and_missing_main_before_scan(self):
        newer = self.git("commit-tree", self.git("rev-parse", self.base + "^{tree}"),
                         "-p", self.base, "-m", "main advanced")
        for main_sha in (newer, None):
            with self.subTest(main_sha=main_sha):
                if main_sha:
                    self.git("update-ref", "refs/remotes/origin/main", main_sha)
                else:
                    self.git("update-ref", "-d", "refs/remotes/origin/main")
                rc, evidence, scan, archive, diff = self.run_main()
                self.assertEqual(rc, 2, evidence)
                self.assertEqual(evidence["verdict"], "fail")
                scan.assert_not_called()
                archive.assert_not_called()
                diff.assert_not_called()

    def test_dispatch_still_requires_candidate_to_be_checked_out_head(self):
        self.git("update-ref", "refs/remotes/origin/main", self.base)
        self.put("docs/later.txt", "later checkout\n")
        self.commit("different checked-out head")
        rc, evidence, scan, archive, diff = self.run_main()
        self.assertEqual(rc, 2, evidence)
        self.assertIn("candidate SHA is not checked-out HEAD", evidence["reason"])
        scan.assert_not_called()
        archive.assert_not_called()
        diff.assert_not_called()


if __name__ == "__main__":
    unittest.main()
