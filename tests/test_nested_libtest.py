"""Inherited child output must stay in its parent's exact libtest harness."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("nested_libtest", ROOT / "scripts/ci/nested_libtest.py")
N = importlib.util.module_from_spec(spec)
spec.loader.exec_module(N)
from check_test_target_integrity import render_lib_inventory_manifest

PARENT = "services::claude_tui::hook_relay::tests::claude_non_wait_hanging_transport_returns_after_stdout_within_750ms"
CHILD = "services::claude_tui::hook_relay::tests::non_wait_relay_parent_subprocess_entry"
OTHER = "services::claude_tui::hook_relay::tests::claude_relay_keeps_command_session_id_even_when_payload_has_session"
EXCERPT = ROOT / "tests/fixtures/ci/nested_libtest/ci-7244-child-stdout.txt"


def ledger(child="tests::child", lane="non-pg-sweep"):
    return f"ignored | {child} | lane={lane} | #7241 | helper subprocess invoked only by its explicitly selected parent\n"


class DeclarationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        source = self.root / "src/lib.rs"
        source.parent.mkdir()
        source.write_text('''#[cfg(test)]
mod tests {
#[test]
fn parent() {
    Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "tests::child"])
        .status().unwrap();
}
#[test]
#[ignore = "helper subprocess"]
fn child() {}
}
''')
        call = N.source_calls(self.root)[("src/lib.rs", "parent")]
        self.table = {"schema": 1, "children": [{
            "id": "tests::child", "source": "src/lib.rs", "platform": "linux",
            "parents": ["tests::parent"], "calls": [{
                "source": "src/lib.rs", "function": "parent", **call}]}]}
        (self.root / N.REL).parent.mkdir(parents=True)
        self.save()
        (self.root / "scripts/lib_test_inventory_manifest.txt").write_text(
            render_lib_inventory_manifest({"tests::parent", "tests::child"}))
        (self.root / "scripts/known_test_failures.txt").write_text(ledger())
        (self.root / "scripts/ci/non-pg-test-filter.sh").write_text(
            "NON_PG_SKIP_ARGS=(\n  --skip _pg\n)\nNON_PG_FILTER_REPLAY=(\n  tests::not_present\n)\n")

    def save(self, table=None):
        (self.root / N.REL).write_text(json.dumps(self.table if table is None else table))

    def test_empty_schema_and_missing_or_malformed_declarations_fail_closed(self):
        self.save({"schema": 1, "children": []})
        self.assertEqual(N.inherited_pairs(self.root), ())
        for table in ({}, {"schema": True, "children": []},
                      {"schema": 1, "children": [], "extra": True}):
            self.save(table)
            with self.assertRaises(ValueError):
                N.inherited_pairs(self.root)
        (self.root / N.REL).write_text('{"schema":1,"schema":1,"children":[]}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            N.inherited_pairs(self.root)
        (self.root / N.REL).unlink()
        with self.assertRaises(OSError):
            N.inherited_pairs(self.root)

    def test_runtime_child_requires_one_parent_both_expected_and_executed(self):
        self.assertEqual(N.inherited_pairs(self.root), (("tests::parent", "tests::child"),))
        for expected, executed, good in (
                ({"tests::parent", "tests::child"}, {"tests::parent", "tests::child"}, True),
                ({"tests::parent"}, {"tests::parent"}, True),
                ({"tests::child"}, {"tests::parent", "tests::child"}, False),
                ({"tests::parent", "tests::child"}, {"tests::child"}, False),
                ({"tests::child"}, {"tests::child"}, False)):
            with self.subTest(expected=expected, executed=executed):
                errors = N.validate_execution(self.root, expected, executed)
                self.assertEqual(not errors, good)
                if errors:
                    self.assertIn("tests::child", errors[0])

    def test_static_audit_covers_new_sites_source_changes_inventory_and_ledger(self):
        self.assertEqual(N.check(self.root), [])
        source = self.root / "src/lib.rs"
        original = source.read_text()
        source.write_text(original + '\nfn new_child() { c.args(["--ignored"]); }\n')
        self.assertTrue(any("source audit differs" in error for error in N.check(self.root)))
        source.write_text(original.replace('.status()', '.stdout(Stdio::null()).status()'))
        self.assertTrue(any("source audit differs" in error for error in N.check(self.root)))
        source.write_text(original)
        (self.root / "scripts/known_test_failures.txt").write_text("")
        self.assertTrue(any("ignored ledger" in error for error in N.check(self.root)))
        (self.root / "scripts/known_test_failures.txt").write_text(ledger())
        (self.root / "scripts/lib_test_inventory_manifest.txt").write_text(
            render_lib_inventory_manifest({"tests::parent"}))
        self.assertTrue(any("child absent from inventory" in error for error in N.check(self.root)))

    def test_comments_and_raw_prose_are_not_new_spawn_sites(self):
        source = self.root / "src/lib.rs"
        source.write_text(source.read_text() + '\n// "--ignored"\nfn prose() { let s = r#"\"--ignored\""#; }\n')
        self.assertEqual(N.check(self.root), [])

    def test_pg_hash_split_wrong_lane_and_replay_harness_split_are_rejected(self):
        filtering = self.root / "scripts/ci/non-pg-test-filter.sh"
        filtering.write_text("NON_PG_SKIP_ARGS=(\n  --skip tests::\n)\nNON_PG_FILTER_REPLAY=(\n  tests::not_present\n)\n")
        (self.root / "scripts/known_test_failures.txt").write_text(ledger(lane="pg"))
        with mock.patch("check_pg_test_lane_membership.pg_test_shard", side_effect=lambda name: int(name.endswith("child"))):
            self.assertTrue(any("numeric shard" in error for error in N.check(self.root)))
        with mock.patch("check_pg_test_lane_membership.pg_test_shard", return_value=0):
            self.assertEqual(N.check(self.root), [])
            filtering.write_text("NON_PG_SKIP_ARGS=(\n  --skip tests::\n)\nNON_PG_FILTER_REPLAY=(\n  tests::parent\n)\n")
            errors = N.check(self.root)
            self.assertTrue(any("judged/replay harness" in error for error in errors))
            self.assertTrue(any("parent's lane non-pg-sweep" in error for error in errors))

    def test_inherited_child_needs_the_same_lane_ignored_declaration(self):
        (self.root / "scripts/known_test_failures.txt").write_text(ledger(lane="pg"))
        self.assertTrue(any("parent's lane non-pg-sweep" in error for error in N.check(self.root)))

    def test_replay_parity_is_required_even_when_pg_hashes_match(self):
        (self.root / "scripts/ci/non-pg-test-filter.sh").write_text(
            "NON_PG_SKIP_ARGS=(\n  --skip tests::\n)\nNON_PG_FILTER_REPLAY=(\n  tests::parent\n)\n")
        (self.root / "scripts/known_test_failures.txt").write_text(ledger(lane="pg") + ledger())
        with mock.patch("check_pg_test_lane_membership.pg_test_shard", return_value=0):
            self.assertTrue(any("judged/replay harness" in error for error in N.check(self.root)))

    def test_declared_parent_must_be_the_real_test_owning_the_call(self):
        source = self.root / "src/lib.rs"
        source.write_text(source.read_text().replace("fn child() {}", "fn child() {}\n#[test] fn other() {}"))
        (self.root / "scripts/lib_test_inventory_manifest.txt").write_text(
            render_lib_inventory_manifest({"tests::parent", "tests::child", "tests::other"}))
        self.table["children"][0]["parents"] = ["tests::other"]
        self.save()
        self.assertTrue(any("do not match inherited test call owners" in error for error in N.check(self.root)))

    def test_child_identity_is_bound_to_its_own_source_and_spawn_selector(self):
        (self.root / "src/other.rs").write_text('#[test] #[ignore] fn child() {}\n')
        self.table["children"][0]["source"] = "src/other.rs"
        self.save()
        self.assertTrue(any("child inventory source differs" in error for error in N.check(self.root)))
        source = self.root / "src/lib.rs"
        source.write_text(source.read_text().replace("fn child() {}", "fn child() {}\n#[test] #[ignore] fn other() {}"))
        (self.root / "scripts/lib_test_inventory_manifest.txt").write_text(
            render_lib_inventory_manifest({"tests::parent", "tests::child", "tests::other"}))
        (self.root / "scripts/known_test_failures.txt").write_text(ledger() + ledger("tests::other"))
        self.table["children"][0].update(id="tests::other", source="src/lib.rs")
        self.save()
        self.assertTrue(any("not named by its spawn function" in error for error in N.check(self.root)))

    def test_ignore_and_windows_cfg_cannot_be_borrowed_from_a_previous_function(self):
        source = self.root / "src/lib.rs"
        original = source.read_text()
        source.write_text(original.replace('#[ignore = "helper subprocess"]',
            '#[ignore] fn previous() {}\n#[test]'))
        self.assertTrue(any("child definition is not ignored" in error for error in N.check(self.root)))
        source.write_text(original.replace('#[ignore = "helper subprocess"]',
            '#[cfg(windows)] #[ignore] fn previous() {}\n#[test] #[ignore]'))
        self.table["children"][0]["platform"] = "windows"
        self.save()
        self.assertTrue(any("excluded child lacks cfg(windows)" in error for error in N.check(self.root)))

    def test_repository_table_covers_every_current_ignored_spawn(self):
        self.assertEqual(N.check(ROOT), [])
        self.assertEqual(N.inherited_pairs(ROOT), ((PARENT, CHILD),))
        rows = N.declarations(ROOT)
        self.assertEqual(len(rows), 12)
        self.assertEqual(sum(call["ignored_args"] for row in rows for call in row["calls"]), 17)


class RealTranscriptTests(unittest.TestCase):
    def run_lane(self, expected, text, *, nested=True, rc=0):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "ids").write_text("\n".join(expected) + "\n")
            (root / "ledger").write_text(ledger(CHILD))
            command = [sys.executable, str(ROOT / "scripts/run_test_lane.py"),
                       "--lane", "non-pg-sweep", "--expected-ids", str(root / "ids"),
                       "--ledger", str(root / "ledger"), "--max-summaries", "4"]
            if nested:
                command += ["--nested-declarations", str(ROOT / N.REL)]
            return subprocess.run([*command, "--", sys.executable, "-c",
                                   f"import sys; sys.stdout.write({text!r}); sys.exit({rc})"],
                                  capture_output=True, text=True)

    def transcript(self, *, parent=True, child_ignored=True, other=False, summary=(1, 1)):
        excerpt = EXCERPT.read_text()
        if not parent:
            excerpt = "\n".join(line for line in excerpt.splitlines() if not line.startswith(f"test {PARENT} ")) + "\n"
        if child_ignored:
            excerpt += f"test {CHILD} ... ignored, helper subprocess that exits after ordered non-wait handoff\n"
        if other:
            excerpt += f"test {OTHER} ... ok\n"
        return excerpt + f"test result: ok. {summary[0]} passed; 0 failed; {summary[1]} ignored; 0 measured; 0 filtered out; finished in 0.00s\n"

    def test_original_split_selection_rejects_real_child_output(self):
        result = self.run_lane([PARENT], self.transcript(child_ignored=False, summary=(1, 0)), nested=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"lane-extra (reported but not expected): {CHILD}", result.stderr)
        self.assertIn("parsed 2 executed ids but libtest reported 1", result.stderr)

    def test_colocated_ignored_child_preserves_primary_arithmetic_and_passes(self):
        result = self.run_lane([PARENT, CHILD], self.transcript())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("executed=1 passed=1", result.stdout)
        self.assertIn("expected=2 missing=0 extra=0 ignored=1 summaries=2", result.stdout)

    def test_child_own_shard_is_green_without_parent_check_and_red_with_it(self):
        text = self.transcript(parent=False, other=True)
        baseline = self.run_lane([CHILD, OTHER], text, nested=False)
        self.assertEqual(baseline.returncode, 0, baseline.stderr)
        guarded = self.run_lane([CHILD, OTHER], text)
        self.assertEqual(guarded.returncode, 1)
        self.assertIn("without a declared parent both expected and executed", guarded.stderr)

    def test_parent_must_be_both_selected_and_reported(self):
        for expected, text in (
                ([CHILD, OTHER], self.transcript(other=True, summary=(2, 1))),
                ([PARENT, CHILD, OTHER], self.transcript(parent=False, other=True))):
            with self.subTest(expected=expected):
                result = self.run_lane(expected, text)
                self.assertEqual(result.returncode, 1)
                self.assertIn("without a declared parent both expected and executed", result.stderr)

    def test_failed_child_never_becomes_allowed_output(self):
        text = self.transcript().replace(f"test {CHILD} ... ok", f"test {CHILD} ... FAILED")
        text += f"failures:\n    {CHILD}\n"
        result = self.run_lane([PARENT, CHILD], text, rc=1)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"unlisted test failure: {CHILD}", result.stderr)


if __name__ == "__main__":
    unittest.main()
