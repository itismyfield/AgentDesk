"""Unit tests for scripts/crate_split/equiv_check.py on small fixture repositories."""

import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = os.path.join(os.path.dirname(__file__), "..", "scripts", "crate_split", "equiv_check.py")
BODY = "    let total = compute_the_total(first_value, second_value);\n    total * 2\n"
MANIFEST = '[package]\nname = "agentdesk"\n\n[features]\nlegacy-sqlite-tests = []\n'


class EquivCheckTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = self.tmp.name
        self.git("init", "-q")

    def tearDown(self):
        self.tmp.cleanup()

    def git(self, *args):
        cmd = ["git", "-C", self.repo, "-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.hooksPath=/dev/null"]
        return subprocess.run([*cmd, *args], check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, files):
        for path, text in files.items():
            full = os.path.join(self.repo, path)
            if text is None:
                os.remove(full)
                continue
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w") as handle:
                handle.write(textwrap.dedent(text))
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", "c")
        return self.git("rev-parse", "HEAD")

    def check(self, base_files, head_files, *extra):
        base = self.commit({"Cargo.toml": MANIFEST, **base_files})
        head = self.commit(head_files)
        cmd = [sys.executable, SCRIPT, "--repo", self.repo, "--base", base, "--head", head, "--skip-cargo", *extra]
        out = subprocess.run(cmd, capture_output=True, text=True).stdout
        return out.splitlines()[0], out

    def test_moving_a_function_to_a_child_file_is_a_move(self):
        base = {"src/lib.rs": "mod a;\n", "src/a.rs": f"pub(crate) fn foo() -> u32 {{\n{BODY}}}\n"}
        head = {"src/a.rs": "mod child;\npub(crate) use self::child::foo;\n",
                "src/a/child.rs": f"pub(crate) fn foo() -> u32 {{\n{BODY}}}\n"}
        verdict, out = self.check(base, head)
        self.assertEqual(verdict, "EQUIV_FAIL a")  # --skip-cargo leaves the compile check unproven
        self.assertIn("| b class | move |", out)

    def test_visibility_only_change_is_vis(self):
        _, out = self.check({"src/lib.rs": "fn foo() {}\n"}, {"src/lib.rs": "pub(crate) fn foo() {}\n"})
        self.assertIn("| b class | vis |", out)

    def test_behaviour_change_is_c_with_line_evidence_and_fails_require_move(self):
        base = {"src/lib.rs": "fn foo() -> u32 {\n    1 + 1\n}\n"}
        verdict, out = self.check(base, {"src/lib.rs": "fn foo() -> u32 {\n    1 + 2\n}\n"}, "--require", "move")
        self.assertEqual(verdict, "EQUIV_FAIL a,b")
        self.assertIn("C       + src/lib.rs:2 [code] 1 + 2", out)

    def test_dropped_test_fails_and_moved_test_is_mapped(self):
        test = "#[test]\nfn keeps_the_total() {\n    assert_eq!(compute_the_total(1, 2), 3);\n}\n"
        _, out = self.check({"src/a.rs": test, "src/b.rs": "\n"}, {"src/a.rs": "\n", "src/b.rs": test})
        self.assertIn("| c tests | PASS |", out)
        self.assertIn("keeps_the_total | src/a.rs | src/b.rs", out)
        _, out = self.check({}, {"src/b.rs": "\n"})
        self.assertIn("| c tests | FAIL |", out)
        self.assertIn("removed=['keeps_the_total']", out)

    def test_feature_missing_from_destination_manifest_fails(self):
        moved = f'#[cfg(feature = "legacy-sqlite-tests")]\npub fn foo() -> u32 {{\n{BODY}}}\n'
        base = {"src/a.rs": moved, "crates/sub/Cargo.toml": '[package]\nname = "sub"\n'}
        _, out = self.check(base, {"src/a.rs": "\n", "crates/sub/src/lib.rs": moved})
        self.assertIn("| d features | FAIL |", out)

    def test_module_path_in_code_moved_across_files_fails(self):
        moved = f'pub fn foo() -> &\'static str {{\n    let here = module_path!();\n{BODY}    here\n}}\n'
        _, out = self.check({"src/a.rs": moved}, {"src/a.rs": "\n", "src/b.rs": moved})
        self.assertIn("| f macros | FAIL |", out)

    def test_log_filter_on_the_old_module_fails_when_moved_code_logs(self):
        moved = f'pub fn foo() -> u32 {{\n    tracing::info!("computing the total now");\n{BODY}}}\n'
        base = {"src/a.rs": moved, "ops/filter.env": "RUST_LOG=agentdesk::a=debug\n"}
        _, out = self.check(base, {"src/a.rs": "\n", "src/b.rs": moved})
        self.assertIn("| e logtarget | FAIL |", out)
        self.assertIn("ops/filter.env | 1 | agentdesk::a | risky | False", out)


if __name__ == "__main__":
    unittest.main()
