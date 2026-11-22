"""Unit tests for scripts/crate_split/equiv_check.py on small fixture repositories."""

import os
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = os.path.join(os.path.dirname(__file__), "..", "scripts", "crate_split", "equiv_check.py")
BODY = "    let total = compute_the_total(first_value, second_value);\n    total * 2\n"
MANIFEST = '[package]\nname = "agentdesk"\n\n[features]\nlegacy-sqlite-tests = []\n'


class RepoCase(unittest.TestCase):
    """A throwaway git repository per test; check() commits base and head and runs the script on them."""

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


class EquivCheckTest(RepoCase):
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


TWO_MODS = ("mod old {\n    pub fn value() -> u32 {\n        1\n    }\n}\nmod new {\n    pub fn value() -> u32 {\n"
            "        2\n    }\n}\n")
LONG_A = '    println!("first notice line that is long enough to be a moved block {}", 1);\n'
LONG_B = '    println!("second notice line that is long enough to be a moved block {}", 2);\n'


class ClassNarrowingTest(RepoCase):
    """KR-C1 review P2-1: what move/vis may not absorb. Each must be C and fail --require move|vis."""

    def assert_c(self, base, head, why):
        for require in ("move", "vis"):
            verdict, out = self.check(base, head, "--require", require)
            self.assertIn("| b class | FAIL (C, required", out, f"{why}\n{out}")
            self.assertEqual(verdict, "EQUIV_FAIL a,b", why)

    def test_call_target_change_is_c(self):
        base = TWO_MODS + "pub fn f() -> u32 {\n    old::value()\n}\n"
        self.assert_c({"src/lib.rs": base}, {"src/lib.rs": base.replace("    old::value()", "    new::value()")},
                      "call target")

    def test_use_target_change_is_c(self):
        base = TWO_MODS + "use old::value;\npub fn f() -> u32 {\n    value()\n}\n"
        self.assert_c({"src/lib.rs": base}, {"src/lib.rs": base.replace("use old::value;", "use new::value;")},
                      "use target")

    def test_string_path_change_is_c(self):
        base = 'pub fn f() -> &\'static str {\n    "old::value"\n}\n'
        self.assert_c({"src/lib.rs": base}, {"src/lib.rs": base.replace("old::value", "new::value")}, "string")

    def test_string_visibility_word_is_c(self):
        base = 'pub fn f() -> &\'static str {\n    "pub message"\n}\n'
        self.assert_c({"src/lib.rs": base}, {"src/lib.rs": base.replace("pub message", "message")}, "string pub")

    def test_statement_reorder_inside_a_function_is_c(self):
        base = "pub fn f() {\n" + LONG_A + LONG_B + "}\n"
        self.assert_c({"src/lib.rs": base}, {"src/lib.rs": "pub fn f() {\n" + LONG_B + LONG_A + "}\n"}, "reorder")

    def test_default_feature_change_is_c(self):
        lib = {"src/lib.rs": 'pub fn f() -> bool {\n    cfg!(feature = "live")\n}\n'}
        base = '[package]\nname = "agentdesk"\n\n[features]\nlive = []\n'
        verdict, out = self.check({"Cargo.toml": base, **lib}, {"Cargo.toml": base.replace("live = []", 'default = ["live"]\nlive = []')},
                                  "--require", "move")
        self.assertIn("| b class | FAIL (C, required", out)

    def test_visibility_inside_a_macro_call_is_not_vis(self):
        base = {"src/lib.rs": "make!(pub(crate) struct A);\n"}
        _, out = self.check(base, {"src/lib.rs": "make!(struct A);\n"})
        self.assertIn("| b class | C |", out)

    def test_declaration_modifiers_stay_vis(self):
        base = ("pub(crate) use self::a::B;\npub(crate) struct S(pub(crate) u32, u8);\npub(crate) struct T {\n"
                "    pub(super) x: u32,\n}\npub(in crate::a) fn g() {}\n")
        head = ("pub use self::a::B;\npub(crate) struct S(pub u32, pub u8);\npub struct T {\n    pub x: u32,\n}\n"
                "pub fn g() {}\n")
        _, out = self.check({"src/lib.rs": base}, {"src/lib.rs": head})
        self.assertIn("| b class | vis |", out)

    def test_caller_path_of_a_moved_item_stays_move(self):
        base = {"src/lib.rs": "mod a;\npub fn run() -> u32 {\n    a::foo()\n}\n",
                "src/a.rs": f"pub(crate) fn foo() -> u32 {{\n{BODY}}}\n"}
        head = {"src/lib.rs": "mod a;\npub fn run() -> u32 {\n    a::child::foo()\n}\n",
                "src/a.rs": "pub(crate) mod child;\n", "src/a/child.rs": f"pub(crate) fn foo() -> u32 {{\n{BODY}}}\n"}
        _, out = self.check(base, head)
        self.assertIn("| b class | move |", out)


PICK = ("macro_rules! pick {\n    (pub(crate) struct $n:ident;) => {\n        pub fn value() -> u32 { 1 }\n    };\n"
        "    (pub struct $n:ident;) => {\n        pub fn value() -> u32 { 2 }\n    };\n}\n")


class FalseOkTest(RepoCase):
    """VT2 review P1-1..P1-5: diffs that change behaviour must be C and fail --require vis|move."""

    def assert_c(self, base, head, why):
        for require in ("move", "vis"):
            verdict, out = self.check(base, head, "--require", require)
            self.assertIn("| b class | FAIL (C, required", out, f"{why}\n{out}")
            self.assertEqual(verdict, "EQUIV_FAIL a,b", why)
            self.tearDown()
            self.setUp()

    def test_whitespace_inside_string_literals_is_c(self):
        for lit, new in (('"a  b"', '"a b"'), ('r"a  b"', 'r"a b"'), ('b"a  b"', 'b"a b"'), ('"a\tb"', '"a b"')):
            base = f"pub fn value() -> &'static [u8] {{\n    {lit}.as_ref()\n}}\n"
            self.assert_c({"src/lib.rs": base}, {"src/lib.rs": base.replace(lit, new)}, lit)
            # On a declaration line the visibility is stripped, so only the token comparison can tell.
            decl = f"pub(crate) const VALUE: &[u8] = {lit};\n"
            self.assert_c({"src/lib.rs": decl}, {"src/lib.rs": decl.replace(lit, new)}, "const " + lit)
            self.assert_c({"src/lib.rs": decl}, {"src/lib.rs": decl.replace(lit, new).replace("pub(crate) ", "pub ")},
                          "pub const " + lit)

    def test_pub_inside_a_multiline_macro_call_is_c(self):
        for open_, close in (("pick! {", "}"), ("pick!(", ");")):
            base = PICK + f"{open_}\n    pub(crate) struct A;\n{close}\n"
            self.assert_c({"src/lib.rs": base}, {"src/lib.rs": base.replace("    pub(crate) struct A;", "    pub struct A;")},
                          open_)

    def test_glob_import_target_change_is_c(self):
        for old, new in (("use old::*;", "use new::*;"), ("use self::{old::*};", "use self::{new::*};")):
            base = TWO_MODS + old + "\npub fn f() -> u32 {\n    value()\n}\n"
            self.assert_c({"src/lib.rs": base}, {"src/lib.rs": base.replace(old, new)}, old)

    def test_unrelated_namesake_of_a_moved_item_is_c(self):
        moved = f"pub fn value() -> u32 {{\n{BODY}}}\n"
        lib = "mod a;\nmod b;\n" + TWO_MODS + "pub fn run() -> u32 {\n    old::value()\n}\n"
        self.assert_c({"src/lib.rs": lib, "src/a.rs": moved, "src/b.rs": "\n"},
                      {"src/lib.rs": lib.replace("    old::value()", "    new::value()"), "src/a.rs": "\n",
                       "src/b.rs": moved}, "namesake")

    def test_unrelated_namesake_in_a_use_is_c(self):
        moved = f"pub fn value() -> u32 {{\n{BODY}}}\n"
        lib = "mod a;\nmod b;\n" + TWO_MODS + "use old::value;\npub fn run() -> u32 {\n    value()\n}\n"
        self.assert_c({"src/lib.rs": lib, "src/a.rs": moved, "src/b.rs": "\n"},
                      {"src/lib.rs": lib.replace("use old::value;", "use new::value;"), "src/a.rs": "\n",
                       "src/b.rs": moved}, "namesake use")

    def test_manifest_lines_moved_by_git_are_c(self):
        lib = {"src/lib.rs": 'pub fn f() -> bool {\n    cfg!(feature = "feature_a_long_enough")\n}\n'}
        manifest = ('[package]\nname = "agentdesk"\n\n[features]\ndefault = ["feature_a_long_enough"]\n'
                    'feature_a_long_enough = []\n\n[package.metadata.spare]\ndefault = ["feature_b_long_enough"]\n')
        swapped = manifest.replace('default = ["feature_a_long_enough"]', "@A").replace(
            'default = ["feature_b_long_enough"]', 'default = ["feature_a_long_enough"]').replace(
            "@A", 'default = ["feature_b_long_enough"]')
        self.assert_c({"Cargo.toml": manifest, **lib}, {"Cargo.toml": swapped}, "manifest move")


class WindowsAutoTest(RepoCase):
    """KR-C1 review P2-4: platform cfgs that span lines or gate the changed body."""

    def needed(self, base, head):
        _, out = self.check({"src/lib.rs": base}, {"src/lib.rs": head})
        return "windows target needed" in out

    def test_multiline_cfg_change_selects_windows(self):
        base = "#[cfg(any(\n    unix,\n))]\npub fn f() -> u32 {\n    1\n}\n"
        self.assertTrue(self.needed(base, base.replace("    unix,", "    windows,")))

    def test_cfg_attr_change_selects_windows(self):
        base = "pub fn f() -> u32 {\n    1\n}\n"
        self.assertTrue(self.needed(base, "#[cfg_attr(windows, allow(dead_code))]\n" + base))

    def test_body_under_a_platform_cfg_selects_windows(self):
        base = "#[cfg(windows)]\npub fn f() -> u32 {\n    let x = 1;\n    x\n}\n"
        self.assertTrue(self.needed(base, base.replace("let x = 1;", "let x = 2;")))

    def test_unrelated_change_does_not(self):
        base = "#[cfg(windows)]\npub fn w() {}\n\npub fn f() -> u32 {\n    1\n}\n"
        self.assertFalse(self.needed(base, base.replace("    1\n", "    2\n")))


class MacroAndLogTest(RepoCase):
    def test_swapping_functions_between_files_moves_module_path(self):
        # KR-C1 review P2-2: both files lose and gain lines, yet module_path!() changes module.
        foo = f"pub fn foo() -> &'static str {{\n    let here = module_path!();\n{BODY}    here\n}}\n"
        bar = f"pub fn bar() -> u32 {{\n    let other = compute_the_other(first_value);\n{BODY}}}\n"
        _, out = self.check({"src/a.rs": foo, "src/b.rs": bar}, {"src/a.rs": bar, "src/b.rs": foo})
        self.assertIn("| f macros | FAIL |", out)

    def test_reordering_in_one_file_keeps_module_path(self):
        foo = f"pub fn foo() -> &'static str {{\n    let here = module_path!();\n{BODY}    here\n}}\n"
        bar = f"pub fn bar() -> u32 {{\n    let other = compute_the_other(first_value);\n{BODY}}}\n"
        _, out = self.check({"src/a.rs": foo + bar}, {"src/a.rs": bar + foo})
        self.assertIn("| f macros | PASS |", out)

    def logtarget(self, filt):
        moved = f'pub fn foo() -> u32 {{\n    tracing::info!("computing the total now");\n{BODY}}}\n'
        base = {"src/a.rs": moved, "ops/filter.env": "RUST_LOG=agentdesk::a=debug\n"}
        _, out = self.check(base, {"src/a.rs": "\n", "src/b.rs": moved, "ops/filter.env": filt})
        return re.search(r"\| e logtarget \| (\w+) \|", out).group(1)

    def test_touched_filter_still_naming_the_old_module_fails(self):
        # KR-C1 review P2-3: editing the filter file is no proof the old target was fixed.
        self.assertEqual(self.logtarget("# reviewed\nRUST_LOG=agentdesk::a=debug\n"), "FAIL")

    def test_filter_level_change_on_the_old_module_fails(self):
        self.assertEqual(self.logtarget("RUST_LOG=agentdesk::a=trace\n"), "FAIL")

    def test_filter_moved_to_the_new_module_passes(self):
        self.assertEqual(self.logtarget("RUST_LOG=agentdesk::b=debug\n"), "PASS")


if __name__ == "__main__":
    unittest.main()
