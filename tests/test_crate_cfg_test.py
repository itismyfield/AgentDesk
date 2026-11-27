from contextlib import redirect_stdout
from hashlib import sha256
from io import StringIO
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts import check_crate_cfg_test as gate


SCRIPT = Path(gate.__file__).resolve()


class CrateCfgTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/runtime", "crates/local"]\n'
                   '[package]\nname = "agentdesk"\n')
        self.write("crates/runtime/Cargo.toml", '[package]\nname = "runtime"\n'
                   '[lib]\npath = "../../src/dep.rs"\n')
        self.write("crates/local/Cargo.toml", '[package]\nname = "local"\n')
        self.write("crates/local/src/lib.rs", "")
        self.write("src/lib.rs", "mod root_only;")
        self.write("src/root_only.rs", "#[cfg(test)] pub fn root_probe() {}")
        self.write("src/dep.rs", "")

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def commit_base(self):
        gate.git(self.root, "init", "-q")
        gate.git(self.root, "add", ".")
        gate.git(self.root, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.com",
                 "-c", "commit.gpgSign=false", "-c", "core.hooksPath=/dev/null",
                 "commit", "-qm", "fixture base")

    def snapshot(self):
        rows = sorted(gate.scan(self.root))
        self.write(gate.BASELINE, "".join(f"{key}\n" for key in rows))

    def run_gate(self, base="HEAD"):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--base-ref", base],
            cwd=self.root, text=True, capture_output=True, check=False,
        )

    def test_switches_statements_fields_and_items_in_dependency_owned_files(self):
        self.write("src/dep.rs", """
mod a {
    #[path = "routed.rs"]
    pub(in crate) mod selected;
}
""")
        self.write("src/a/routed.rs", """#[cfg(not(test))]
fn production() { if cfg! { test, } {} }
#[cfg(test)]
#[inline]
pub(crate) fn probe() {}
#[cfg(test)] pub use crate::Thing;
fn f() {
    #[cfg ( /* cfg note */ test )] probe();
    let value = (#[cfg(test)] 1, 2);
}
struct Data {
    #[cfg(test)] field: usize,
}
""")
        self.write("crates/local/src/lib.rs", "#[cfg(all(test, unix))] fn test_hook() {}")
        found = {(site.path, site.line, site.kind) for site in gate.scan(self.root).values()}
        self.assertEqual(found, {
            ("crates/local/src/lib.rs", 1, "cfg-test-item"),
            ("src/a/routed.rs", 1, "cfg-not-test"),
            ("src/a/routed.rs", 2, "cfg-bang-test"),
            ("src/a/routed.rs", 3, "cfg-test-item"),
            ("src/a/routed.rs", 6, "cfg-test-item"),
            ("src/a/routed.rs", 8, "cfg-test-statement"),
            ("src/a/routed.rs", 9, "cfg-test-statement"),
            ("src/a/routed.rs", 12, "cfg-test-statement"),
        })

    def test_excluded_test_modules_feature_gates_comments_and_literals_pass(self):
        self.write("src/dep.rs", '''mod local_tests;
#[cfg(test)]
#[path = "private_suite.rs"]
mod external;
#[cfg(any(test, feature = "test-support"))] fn feature_hook() {}
#[cfg(all(unix, any(feature = "test-support", test)))] fn unix_hook() {}
#[cfg(test)]
mod inline {
    mod helper;
    #[cfg(not(test))] mod no_runtime {}
    const BODY: &str = r#" { } "#;
}
const TEXT: &str = r#" # [cfg(test)] fn raw() {} cfg!(test) "#;
// #[cfg(test)] fn comment() {}
/* outer /* cfg!(test) */ #[cfg(test)] fn comment() {} */
const TOKENS: &str = stringify! { #[cfg(test)] fn inert() {} };
''')
        for path in ("src/local_tests.rs", "src/private_suite.rs", "src/inline/helper.rs"):
            self.write(path, "#[cfg(test)] fn ignore() { let _ = cfg!(test); }")
        self.write(gate.BASELINE, "")
        self.commit_base()
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("sites=0 new=0 baseline_growth=0", result.stdout)

    def test_existing_keys_survive_line_drift_and_new_occurrence_fails(self):
        old = "#[cfg(test)]\nfn old() {}\n#[cfg(not(test))] fn regular() {}\n"
        self.write("src/dep.rs", old)
        self.snapshot()
        self.commit_base()
        self.write("src/dep.rs", "\n// moved down\n#[cfg( test )] fn old( ) { useful(); }\n"
                   "#[cfg(not(test))] fn regular() {}\n")
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("baseline=2 sites=2 new=0 baseline_growth=0", result.stdout)
        self.write("src/dep.rs", old + "#[cfg(test)] fn new() {}\n")
        result = self.run_gate()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("CFG_TEST_IN_CRATE src/dep.rs:4 cfg-test-item\n", result.stdout)
        self.assertIn("baseline=2 sites=3 new=1 baseline_growth=0", result.stdout)

    def test_repeated_signature_counts_each_occurrence(self):
        call = "fn f() { #[cfg(test)] probe(); }\n"
        self.write("src/dep.rs", call)
        self.snapshot()
        self.commit_base()
        self.write("src/dep.rs", call + call.replace("fn f", "fn g"))
        result = self.run_gate()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("CFG_TEST_IN_CRATE src/dep.rs:2 cfg-test-statement\n", result.stdout)

    def test_allow_comment_must_be_in_base_as_an_allowance(self):
        ordinary = "#[cfg(test)]\nfn ordinary() {}\n"
        allowed = "#[cfg(test)] // cfg-test-in-crate: allow used by this crate's unit tests\nfn allowed() {}\n"
        self.write("src/dep.rs", ordinary + allowed)
        self.snapshot()
        self.commit_base()
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("baseline=2 sites=2 new=0 baseline_growth=0", result.stdout)
        promoted = ordinary.replace("#[cfg(test)]", "#[cfg(test)] // cfg-test-in-crate: allow unit only")
        self.write("src/dep.rs", promoted + allowed)
        result = self.run_gate()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("CFG_TEST_IN_CRATE src/dep.rs:1 cfg-test-item\n", result.stdout)
        self.snapshot()
        result = self.run_gate()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("CFG_TEST_BASELINE_FAIL added row", result.stdout)
        self.assertIn("baseline=2 sites=2 new=1 baseline_growth=1", result.stdout)

    def test_allow_marker_only_matches_real_same_line_comment_with_reason(self):
        source = """#[cfg(test)]
// cfg-test-in-crate: allow wrong line
fn next_line() {}
#[cfg(test)] /* cfg-test-in-crate: allow block comment */
fn not_comment() {}
#[cfg(test)] const FAKE: &str = "// cfg-test-in-crate: allow in string";
#[cfg(test)] // cfg-test-in-crate: allow
fn no_reason() {}
/*
// cfg-test-in-crate: allow inside block */ #[cfg(test)] fn after_block() {}
#[cfg(test)] // cfg-test-in-crate: allow private unit helper
fn helper() {}
"""
        findings = gate.file_findings("src/dep.rs", source)
        self.assertEqual([site.line for site in findings if site.allowed], [11])

    def test_initial_baseline_is_pinned_and_duplicate_rows_fail(self):
        self.write("src/dep.rs", "#[cfg(test)] fn existing() {}\n")
        self.commit_base()
        self.snapshot()
        baseline = (self.root / gate.BASELINE).read_text(encoding="utf-8")
        digest = sha256(baseline.encode()).hexdigest()
        with patch.object(gate, "INITIAL_BASELINE_SHA256", digest), redirect_stdout(StringIO()) as output:
            self.assertEqual(gate.check(self.root, gate.scan(self.root), "HEAD"), 0)
            self.write("src/dep.rs", "#[cfg(test)] fn changed() {}\n")
            self.snapshot()
            self.assertEqual(gate.check(self.root, gate.scan(self.root), "HEAD"), 1)
            self.assertIn("CFG_TEST_BASELINE_FAIL initial baseline differs", output.getvalue())
        self.write(gate.BASELINE, baseline * 2)
        result = self.run_gate()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("baseline rows must be sorted, nonempty, and unique", result.stderr)

    def test_bad_base_or_no_crate_fails_instead_of_reporting_zero(self):
        self.write(gate.BASELINE, "")
        self.commit_base()
        result = self.run_gate("not-a-base")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("CFG_TEST_SCAN_FAIL", result.stderr)
        self.write("Cargo.toml", '[workspace]\nmembers = []\n')
        result = self.run_gate()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("no library crates found", result.stderr)
