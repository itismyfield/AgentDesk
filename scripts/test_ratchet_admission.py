#!/usr/bin/env python3
"""Executable cap, history-warning, and raw measurement boundary fixtures."""
from __future__ import annotations

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))
import check_hotfile_ratchet as hotfile
import ratchet_admission as admission


class RatchetAdmissionGuardTest(unittest.TestCase):
    def test_cap_raise_with_valid_history_still_fails(self):
        event = admission.AdmissionEvent("hotfile_ratchet", "src/a.rs", 100, 101, 1, 1)
        self.assertTrue(admission.validate_admission_delta(ratchet="hotfile_ratchet",
            current_caps={"src/a.rs": 101}, prior_caps={"src/a.rs": 100},
            current_events=[event], prior_events=[]))

    def test_history_form_does_not_block_without_cap_growth(self):
        event = admission.AdmissionEvent("hotfile_ratchet", "src/a.rs", 100, 101, None, 4)
        self.assertEqual(admission.validate_admission_delta(ratchet="hotfile_ratchet",
            current_caps={"src/a.rs": 100}, prior_caps={"src/a.rs": 100},
            current_events=[event], prior_events=[]), [])
        self.assertTrue(admission.validate_history([event]))

    def test_equal_and_lower_caps_pass(self):
        for cap in (99, 100):
            self.assertEqual(admission.validate_admission_delta(ratchet="hotfile_ratchet",
                current_caps={"src/a.rs": cap}, prior_caps={"src/a.rs": 100},
                current_events=[], prior_events=[]), [])

    def test_repeated_admission_history_keeps_decomposition_warning(self):
        events = [admission.AdmissionEvent("hotfile_ratchet", "src/a.rs", 100 + count,
                    101 + count, 4200 + count, count)
                  for count in range(1, admission.ADMISSION_WARN_THRESHOLD + 2)]
        warnings = admission.admission_warning_messages(events, "hotfile_ratchet")
        self.assertEqual(len(warnings), 1)
        self.assertIn("prioritize decomposition", warnings[0])


class HotfileRatchetTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.manifest = self.root / "scripts/hotfile_ratchet.toml"
        self.history = self.root / admission.HISTORY_REL_PATH
        self.manifest.parent.mkdir(parents=True)
        self.caps = {path: 100 for path in hotfile.REQUIRED_HOTFILES}
        self.write_caps()
        self.history.write_text("schema_version = 1\nadmission = []\n")
        for path in self.caps:
            self.write_size(path, 100)
        self.grown = hotfile.REQUIRED_HOTFILES[0]
        for target, value in (("REPO_ROOT", self.root), ("MANIFEST", self.manifest)):
            patch = mock.patch.object(hotfile, target, value)
            patch.start()
            self.addCleanup(patch.stop)
        patch = mock.patch.dict(os.environ, {}, clear=True)
        patch.start()
        self.addCleanup(patch.stop)

    def write_caps(self):
        self.manifest.write_text("[hotfile_ratchet]\n" + "".join(
            f'"{path}" = {cap}\n' for path, cap in self.caps.items()))

    def write_size(self, path, lines):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("x\n" * lines)

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True,
                              capture_output=True, text=True).stdout.strip()

    def init_git(self, inherited=100):
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "hotfile@example.invalid")
        self.git("config", "user.name", "Hotfile Fixture")
        self.write_size(self.grown, inherited)
        self.git("add", "-A")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", self.base)
        return self.base

    def run_check(self):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = hotfile.main()
        return rc, out.getvalue(), err.getvalue()

    def test_export_no_growth_equal_and_reduction(self):
        for size, expected in ((99, 0), (100, 0), (101, 1), (130, 1)):
            self.write_size(self.grown, size)
            rc, _, err = self.run_check()
            self.assertEqual(rc, expected, err)
            if expected:
                self.assertIn(f"grew to {size} lines > ceiling 100", err)

    def test_inherited_overage_frozen_by_sha_no_one_line_growth(self):
        self.init_git(inherited=124)
        for size, expected in ((123, 0), (124, 0), (125, 1)):
            self.write_size(self.grown, size)
            rc, out, err = self.run_check()
            self.assertEqual(rc, expected, err)
            self.assertIn(self.base, err if expected else out)

    def test_cap_space_does_not_allow_regrowth_from_smaller_base(self):
        self.init_git(inherited=90)
        self.write_size(self.grown, 91)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("grew to 91 lines", err)

    def test_git_source_equal_shrink_and_one_line_growth_boundary(self):
        self.init_git()
        for size, expected in ((100, 0), (99, 0), (101, 1)):
            self.write_size(self.grown, size)
            self.git("add", "-A")
            self.git("commit", "--allow-empty", "-qm", f"candidate size {size}")
            rc, _, err = self.run_check()
            self.assertEqual(rc, expected, err)

    def test_cap_raise_fails_even_if_file_shrinks(self):
        self.init_git()
        self.write_size(self.grown, 99)
        self.caps[self.grown] = 101
        self.write_caps()
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("cap raised", err)

    def test_ledger_format_fixtures_warn_and_pass(self):
        for text in ('[broken\n', 'schema_version = 9\n',
                     'schema_version = 1\n[[admission]]\nfile = "src/a.rs"\n'):
            with self.subTest(text=text):
                self.history.write_text(text)
                rc, _, err = self.run_check()
                self.assertEqual(rc, 0, err)
                self.assertIn("WARN:", err)
                self.assertNotIn("FAIL:", err)

    def test_history_removal_warns_and_passes_in_git_fixture(self):
        self.history.write_text('schema_version = 1\n[[admission]]\nratchet="hotfile_ratchet"\n'
            'file="src/a.rs"\nold_cap=100\nnew_cap=101\ndecompose_issue=1\ncount=1\n')
        self.init_git()
        self.history.write_text('schema_version = 1\nadmission = []\n')
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("WARN:", err)
        self.assertIn("not append-only", err)

    def test_missing_file_bad_cap_and_removed_required_cap_fail(self):
        target = self.root / self.grown
        target.unlink()
        self.assertEqual(self.run_check()[0], 1)
        self.write_size(self.grown, 100)
        for broken in ('[broken\n', '[hotfile_ratchet]\n',
                       '[hotfile_ratchet]\n"src/a.rs"=true\n'):
            self.manifest.write_text(broken)
            self.assertEqual(self.run_check()[0], 1)

    def test_nonrequired_ceiling_removal_cannot_hide_file(self):
        self.caps["src/extra.rs"] = 100
        self.write_caps()
        self.write_size("src/extra.rs", 100)
        self.init_git()
        del self.caps["src/extra.rs"]
        self.write_caps()
        self.write_size("src/extra.rs", 101)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("ceiling removed", err)

    def extra_base(self, paths=("src/extra.rs",)):
        for path in paths:
            self.caps[path] = 100
            self.write_size(path, 100)
        self.write_caps()
        self.init_git()

    def remove_extra_pin(self, *, delete_source=True, commit=True):
        del self.caps["src/extra.rs"]
        self.write_caps()
        if delete_source:
            (self.root / "src/extra.rs").unlink()
        if commit:
            self.git("add", "-A")
            self.git("commit", "-qm", "retire extra pin")
        return self.git("rev-parse", "HEAD")

    def test_nonrequired_source_and_pin_retirement_passes(self):
        self.extra_base()
        self.remove_extra_pin()
        rc, out, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("retired hotfile: src/extra.rs", out)

    def test_committed_pin_only_removal_keeps_file_blocked(self):
        self.extra_base()
        self.remove_extra_pin(delete_source=False)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("ceiling removed", err)

    def test_worktree_only_source_and_pin_deletion_is_not_retirement(self):
        self.extra_base()
        self.remove_extra_pin(commit=False)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("ceiling removed", err)

    def test_each_missing_key_requires_its_own_source_retirement(self):
        self.extra_base(("src/extra.rs", "src/still_here.rs"))
        del self.caps["src/still_here.rs"]
        self.remove_extra_pin()
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("src/still_here.rs", err)

    def test_retirement_candidate_tree_lookup_failure_is_not_absence(self):
        self.extra_base()
        self.remove_extra_pin()
        tree = self.git("rev-parse", "HEAD^{tree}")
        loose_tree = self.root / ".git/objects" / tree[:2] / tree[2:]
        self.assertTrue(loose_tree.is_file())
        loose_tree.unlink()
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("git could not read", err)

    def test_pr_nonrequired_source_and_pin_retirement_passes(self):
        self.extra_base()
        self.git("checkout", "-qb", "pr")
        head = self.remove_extra_pin()
        self.git("checkout", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "candidate", "pr")
        env = {"GFP_EVENT_NAME": "pull_request", "GFP_CANDIDATE_SHA": self.git("rev-parse", "HEAD"),
               "GFP_BASE_SHA": self.base, "GFP_HEAD_SHA": head}
        with mock.patch.dict(os.environ, env, clear=True):
            rc, out, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("retired hotfile: src/extra.rs", out)

    def test_required_pin_retirement_is_still_blocked(self):
        self.init_git()
        del self.caps[self.grown]
        self.write_caps()
        (self.root / self.grown).unlink()
        self.git("add", "-A")
        self.git("commit", "-qm", "remove required pin")
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("every required hotfile ceiling", err)

    def test_invalid_explicit_base_does_not_fall_back(self):
        self.init_git()
        os.environ["RATCHET_ADMISSION_BASE_REF"] = "refs/missing"
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("cannot resolve", err)

    def test_measurement_exception_cannot_warn_pass(self):
        for exception in (OSError("cannot read"), RuntimeError("checker crashed"),
                          UnicodeDecodeError("utf-8", b"\xff", 0, 1, "bad byte")):
            with mock.patch.object(hotfile, "line_count", side_effect=exception):
                rc, _, err = self.run_check()
            self.assertEqual(rc, 1)
            self.assertIn("FAIL:", err)

    def test_git_blob_read_exception_is_failure(self):
        self.init_git()
        with mock.patch.object(admission, "_git_text", side_effect=ValueError("Git read failed")):
            rc, _, err = self.run_check()
        self.assertEqual(rc, 1)
        self.assertIn("Git read failed", err)

    def test_missing_base_object_is_not_a_missing_path(self):
        self.init_git()
        with self.assertRaisesRegex(ValueError, "git could not read"):
            admission._git_text(self.root, "0" * 40, "src/a.rs")

    def test_pr_candidate_sha_and_parent_order_are_checked(self):
        self.init_git()
        self.git("checkout", "-qb", "pr")
        self.write_size(self.grown, 99)
        self.git("add", "-A")
        self.git("commit", "-qm", "pr")
        head = self.git("rev-parse", "HEAD")
        self.git("checkout", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "candidate", "pr")
        candidate = self.git("rev-parse", "HEAD")
        env = {"GFP_EVENT_NAME": "pull_request", "GFP_CANDIDATE_SHA": candidate,
               "GFP_BASE_SHA": self.base, "GFP_HEAD_SHA": head}
        with mock.patch.dict(os.environ, env, clear=True):
            self.assertEqual(admission.comparison_base(self.root), (self.base, None))
            self.assertEqual(self.run_check()[0], 0)
            for key, value in (("GFP_CANDIDATE_SHA", head), ("GFP_HEAD_SHA", self.base),
                               ("GFP_BASE_SHA", head), ("GFP_BASE_SHA", "0" * 40)):
                with self.subTest(key=key), mock.patch.dict(os.environ, {key: value}):
                    self.assertEqual(self.run_check()[0], 1)


if __name__ == "__main__":
    unittest.main()
