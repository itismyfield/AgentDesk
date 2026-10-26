#!/usr/bin/env python3
"""Executable reference, history-warning, and raw measurement boundary fixtures."""
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
    def test_repeated_admission_history_keeps_responsibility_review_warning(self):
        events = [admission.AdmissionEvent("hotfile_ratchet", "src/a.rs", 100 + count,
                    101 + count, 4200 + count, count)
                  for count in range(1, admission.ADMISSION_WARN_THRESHOLD + 2)]
        warnings = admission.admission_warning_messages(events, "hotfile_ratchet")
        self.assertEqual(len(warnings), 1)
        self.assertIn("review whether an independent responsibility should move", warnings[0])

    def test_reference_changes_are_advisory_in_repository_audit(self):
        for ratchet, unit in (("giant_file_ratchet", "production"), ("hotfile_ratchet", "raw")):
            with self.subTest(ratchet=ratchet), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                config = root / "scripts/reference.toml"
                config.parent.mkdir()
                config.write_text(f'[{ratchet}]\n"src/a.rs" = 1000\n')
                (root / admission.HISTORY_REL_PATH).write_text("schema_version = 1\n")
                for args in (("init", "-qb", "main"), ("add", "."),
                             ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                              "commit", "-qm", "base")):
                    subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True)
                for cap in (999, 1000, 100000):
                    config.write_text(f'[{ratchet}]\n"src/a.rs" = {cap}\n')
                    with mock.patch.dict(os.environ, {"RATCHET_ADMISSION_BASE_REF": "HEAD"}):
                        audit = admission.audit_repository_admissions(repo_root=root,
                            ratchet=ratchet, config_rel_path="scripts/reference.toml", table_name=ratchet)
                    self.assertEqual(audit.errors, ())
                    self.assertEqual(bool(audit.warnings), cap > 1000)
                    if cap > 1000:
                        self.assertTrue(any(f"src/a.rs: +99000 {unit} lines" in message
                                            for message in audit.warnings))


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

    def test_export_growth_warns_equal_and_reduction_stay_quiet(self):
        for size in (99, 100, 101, 130):
            self.write_size(self.grown, size)
            rc, _, err = self.run_check()
            self.assertEqual(rc, 0, err)
            self.assertNotIn("FAIL:", err)
            if size > 100:
                self.assertIn(f"{self.grown}: +{size - 100} raw lines (100 -> {size})", err)
            else:
                self.assertNotIn("WARNING:", err)

    def test_production_giant_raw_growth_and_reference_raise_warn(self):
        self.init_git(inherited=1200)
        self.write_size(self.grown, 1201)
        self.caps[self.grown] = 1201
        self.write_caps()
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn(f"WARNING: giant growth: {self.grown}: +1 raw lines", err)
        self.assertIn(f"giant reference increased: {self.grown}", err)

    def test_tracked_hotfile_crossing_production_giant_threshold_warns(self):
        self.init_git(inherited=999)
        self.write_size(self.grown, 1000)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn(f"WARNING: giant growth: {self.grown}: +1 raw lines", err)

    def test_subthreshold_hotfile_growth_warns_with_file_and_delta(self):
        self.init_git(inherited=992)
        self.write_size(self.grown, 999)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn(f"WARNING: giant growth: {self.grown}: +7 raw lines (992 -> 999)", err)
        self.assertNotIn("FAIL:", err)

    def test_inherited_overage_uses_sha_measurement_for_growth_warning(self):
        self.init_git(inherited=124)
        for size in (123, 124, 125):
            self.write_size(self.grown, size)
            rc, out, err = self.run_check()
            self.assertEqual(rc, 0, err)
            self.assertIn(self.base, err if size > 124 else out)
            if size > 124:
                self.assertIn(f"{self.grown}: +1 raw lines (124 -> 125)", err)

    def test_regrowth_below_reference_warns_against_smaller_base(self):
        self.init_git(inherited=90)
        self.write_size(self.grown, 91)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn(f"WARNING: giant growth: {self.grown}: +1 raw lines (90 -> 91)", err)

    def test_git_source_equal_shrink_and_growth_boundary(self):
        self.init_git()
        for size in (100, 99, 101):
            self.write_size(self.grown, size)
            self.git("add", "-A")
            self.git("commit", "--allow-empty", "-qm", f"candidate size {size}")
            rc, _, err = self.run_check()
            self.assertEqual(rc, 0, err)
            self.assertEqual("WARNING: giant growth:" in err, size > 100)

    def test_reference_raise_warns_even_if_file_shrinks(self):
        self.init_git()
        self.write_size(self.grown, 99)
        self.caps[self.grown] = 101
        self.write_caps()
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn(f"giant reference increased: {self.grown}: +1 raw lines", err)
        self.assertNotIn("FAIL:", err)

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

    def test_source_retirement_with_policy_history_edit_passes(self):
        self.extra_base()
        self.remove_extra_pin(commit=False)
        self.history.write_text(self.history.read_text() + "# retired source pin\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "retire source with policy history update")
        rc, out, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("retired hotfile: src/extra.rs", out)

    def test_source_retirement_with_unlisted_metadata_edit_is_uncertain(self):
        other = self.root / "scripts/retirement_notes.toml"
        other.write_text('note = "before"\n')
        self.extra_base()
        self.remove_extra_pin(commit=False)
        other.write_text('note = "after"\n')
        self.git("add", "-A")
        self.git("commit", "-qm", "retire source with unrelated metadata update")
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1, err)
        self.assertIn("uncertain hotfile retirement", err)

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

    def moved_extra(self, target="src/renamed.rs", *, text=None, pin=None, copies=()):
        self.extra_base()
        self.git("mv", "src/extra.rs", target)
        if text is not None:
            (self.root / target).write_text(text)
        del self.caps["src/extra.rs"]
        if pin is not None:
            self.caps[target] = pin
        for path in copies:
            self.write_size(path, 100)
        self.write_caps()
        self.git("add", "-A")
        self.git("commit", "-qm", "move extra source")
        return self.git("diff-tree", "--no-commit-id", "-r", "-M", "-C",
                        "--find-copies-harder", "--name-status", self.base, "HEAD")

    def test_nonrequired_renamed_source_cannot_retire_pin(self):
        statuses = self.moved_extra()
        self.assertIn("R100\tsrc/extra.rs\tsrc/renamed.rs", statuses)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1, err)

    def test_case_only_rename_cannot_retire_pin(self):
        statuses = self.moved_extra(target="src/Extra.rs")
        self.assertIn("R100\tsrc/extra.rs\tsrc/Extra.rs", statuses)
        self.assertEqual(self.run_check()[0], 1)

    def test_below_similarity_move_is_uncertain_with_or_without_new_pin(self):
        statuses = self.moved_extra(text="changed production line\n" * 100)
        self.assertIn("D\tsrc/extra.rs", statuses)
        self.assertIn("A\tsrc/renamed.rs", statuses)
        print("below-similarity diff statuses:\n" + statuses)
        self.assertEqual(self.run_check()[0], 1)
        self.caps["src/renamed.rs"] = 100
        self.write_caps()
        self.git("add", "-A")
        self.git("commit", "-qm", "pin uncertain added source")
        self.assertEqual(self.run_check()[0], 1)

    def test_detected_move_with_target_pin_is_transfer(self):
        self.moved_extra(pin=100)
        rc, out, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("transferred hotfile: src/extra.rs -> src/renamed.rs", out)
        self.assertNotIn("retired hotfile: src/extra.rs", out)

    def test_giant_pin_transfer_allows_growth_and_reference_change(self):
        source, target = "src/extra.rs", "src/renamed.rs"
        self.caps[source] = 1200
        self.write_caps()
        self.write_size(source, 1200)
        self.init_git()
        self.git("mv", source, target)
        self.write_size(target, 1201)
        del self.caps[source]
        self.caps[target] = 1201
        self.write_caps()
        self.git("add", "-A")
        self.git("commit", "-qm", "move cohesive giant")
        rc, out, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("transferred hotfile", out)
        self.assertIn(f"WARNING: giant growth: {target}: +1 raw lines", err)

    def test_transfer_reference_raise_warns(self):
        self.moved_extra(pin=101)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("WARNING: giant reference increased: src/renamed.rs: +1 raw lines", err)

    def test_transfer_candidate_growth_warns_even_with_dirty_shrink(self):
        self.moved_extra(text="x\n" * 101, pin=100)
        self.write_size("src/renamed.rs", 100)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("WARNING: giant growth: src/renamed.rs: +1 raw lines (100 -> 101)", err)

    def test_movement_query_object_read_failure_is_not_retirement(self):
        self.moved_extra(text="changed production line\n" * 100)
        blob = self.git("rev-parse", f"{self.base}:src/extra.rs")
        loose_blob = self.root / ".git/objects" / blob[:2] / blob[2:]
        self.assertTrue(loose_blob.is_file())
        loose_blob.unlink()
        rc, _, err = self.run_check()
        self.assertEqual(rc, 1, err)
        self.assertIn("movement query failed", err)

    def test_transfer_retains_old_base_measurement_and_inherited_overage(self):
        self.extra_base()
        self.write_size("src/extra.rs", 90)
        self.git("add", "-A")
        self.git("commit", "-qm", "smaller comparison base")
        self.base = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", self.base)
        self.git("mv", "src/extra.rs", "src/renamed.rs")
        self.caps["src/renamed.rs"] = self.caps.pop("src/extra.rs")
        self.write_caps()
        self.git("add", "-A")
        self.git("commit", "-qm", "transfer smaller source")
        self.write_size("src/renamed.rs", 91)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("src/renamed.rs: +1 raw lines (90 -> 91)", err)
        self.write_size("src/renamed.rs", 90)
        self.assertEqual(self.run_check()[0], 0)

    def test_detected_move_inherited_overage_uses_base_measurement(self):
        self.extra_base()
        self.write_size("src/extra.rs", 124)
        self.git("add", "-A")
        self.git("commit", "-qm", "inherited overage comparison base")
        self.base = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", self.base)
        self.git("mv", "src/extra.rs", "src/renamed.rs")
        self.caps["src/renamed.rs"] = self.caps.pop("src/extra.rs")
        self.write_caps()
        self.git("add", "-A")
        self.git("commit", "-qm", "transfer inherited overage")
        self.assertEqual(self.run_check()[0], 0)
        self.write_size("src/renamed.rs", 125)
        rc, _, err = self.run_check()
        self.assertEqual(rc, 0, err)
        self.assertIn("src/renamed.rs: +1 raw lines (124 -> 125)", err)

    def test_multiple_indicated_move_targets_are_ambiguous(self):
        self.moved_extra(pin=100, copies=("src/copied.rs",))
        self.caps["src/copied.rs"] = 100
        self.write_caps()
        self.git("add", "-A")
        self.git("commit", "-qm", "pin second indicated copy")
        self.assertEqual(self.run_check()[0], 1)

    def test_uncertain_new_sources_with_new_pin_do_not_infer_mapping(self):
        self.moved_extra(text="changed production line\n" * 100, pin=100,
                         copies=("src/also_added.rs",))
        self.assertEqual(self.run_check()[0], 1)

    def test_below_similarity_overwrite_is_uncertain_with_or_without_new_pin(self):
        target = self.root / "src/existing.rs"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("existing destination line\n" * 100)
        self.extra_base()
        self.remove_extra_pin(commit=False)
        target.write_text("replaced production line\n" * 100)
        self.git("add", "-A")
        self.git("commit", "-qm", "overwrite existing destination")
        statuses = self.git("diff-tree", "--no-commit-id", "-r", "-M", "-C",
                            "--find-copies-harder", "--name-status", self.base, "HEAD")
        self.assertIn("D\tsrc/extra.rs", statuses)
        self.assertIn("M\tsrc/existing.rs", statuses)
        print("below-similarity overwrite statuses:\n" + statuses)
        self.assertEqual(self.run_check()[0], 1)
        self.caps["src/existing.rs"] = 100
        self.write_caps()
        self.git("add", "-A")
        self.git("commit", "-qm", "pin uncertain overwritten destination")
        self.assertEqual(self.run_check()[0], 1)

    def test_below_similarity_arbitrary_extension_cannot_retire_pin(self):
        for target, status in (("src/renamed.inc", "A"), ("src/existing.inc", "M"),
                               ("generated/renamed.data", "A"), ("generated/existing.data", "M")):
            with self.subTest(target=target, status=status):
                case = HotfileRatchetTest()
                case.setUp()
                try:
                    destination = case.root / target
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    if status == "M":
                        destination.write_text("existing destination line\n" * 100)
                    case.extra_base()
                    case.git("mv", "--force", "src/extra.rs", target)
                    destination.write_text("changed production line\n" * 100)
                    case.remove_extra_pin(delete_source=False)
                    statuses = case.git("diff-tree", "--no-commit-id", "-r", "-M", "-C",
                                        "--find-copies-harder", "--name-status", case.base, "HEAD")
                    self.assertIn("D\tsrc/extra.rs", statuses)
                    self.assertIn(status + "\t" + target, statuses)
                    print("arbitrary-extension movement statuses:\n" + statuses)
                    without_pin = case.run_check()
                    case.caps[target] = 100
                    case.write_caps()
                    case.git("add", "-A")
                    case.git("commit", "-qm", "pin uncertain arbitrary-extension target")
                    for pinned, result in ((False, without_pin), (True, case.run_check())):
                        with self.subTest(pinned=pinned):
                            self.assertEqual(result[0], 1, result[2])
                finally:
                    case.doCleanups()

    def test_original_path_recreation_blocks_retirement(self):
        self.extra_base()
        self.remove_extra_pin(commit=False)
        self.write_size("src/extra.rs", 1)
        self.git("add", "-A")
        self.git("commit", "-qm", "recreate original path")
        self.assertEqual(self.run_check()[0], 1)

    def test_original_path_symlink_blocks_retirement(self):
        self.extra_base()
        self.remove_extra_pin(commit=False)
        (self.root / "src/extra.rs").symlink_to(self.root / self.grown)
        self.git("add", "-A")
        self.git("commit", "-qm", "replace source with symlink")
        self.assertEqual(self.run_check()[0], 1)

    def test_original_path_submodule_blocks_retirement(self):
        self.extra_base()
        self.remove_extra_pin(commit=False)
        self.git("add", "-A")
        self.git("update-index", "--add", "--cacheinfo", f"160000,{self.base},src/extra.rs")
        self.git("commit", "-qm", "replace source with gitlink")
        self.assertEqual(self.run_check()[0], 1)

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
