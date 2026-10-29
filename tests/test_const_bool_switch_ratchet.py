import json
import subprocess
import tempfile
import unittest
from collections import Counter
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path

from scripts import check_const_bool_switch_ratchet as ratchet
from scripts.check_const_bool_switch_ratchet import BASELINE, collect, declarations, parse_baseline, problems


class ConstBoolSwitchRatchetTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def put(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def run_main(self, rows, *argv):
        self.put(BASELINE.relative_to(ratchet.ROOT).as_posix(), json.dumps(rows))
        out = StringIO()
        with redirect_stdout(out):
            rc = ratchet.main(list(argv), root=self.root)
        return rc, out.getvalue()

    def test_current_tree_matches_baseline(self):
        self.assertEqual(problems(collect(), parse_baseline(BASELINE.read_text())), [])

    def test_listed_switch_passes_and_new_switch_fails_with_guidance(self):
        self.put("src/a.rs", "pub(crate) const OLD: bool = true;\n")
        self.assertEqual(self.run_main([["src/a.rs", "OLD"]])[0], 0)
        self.put("src/a.rs", "pub(crate) const OLD: bool = true;\npub const NEW: bool = false;\n")
        rc, out = self.run_main([["src/a.rs", "OLD"]])
        self.assertEqual(rc, 1)
        self.assertIn("src/a.rs:2 new `const NEW: bool` switch; use a config key", out)

    def test_moved_or_repeated_switch_fails(self):
        self.assertTrue(problems({("src/b.rs", "OLD"): [1]}, Counter({("src/a.rs", "OLD"): 1})))
        self.assertTrue(problems({("src/a.rs", "OLD"): [1, 2]}, Counter({("src/a.rs", "OLD"): 1})))

    def test_vanished_switch_must_leave_the_baseline(self):
        self.put("src/a.rs", "fn f() {}\n")
        rc, out = self.run_main([["src/a.rs", "GONE"]])
        self.assertEqual(rc, 1)
        self.assertIn("src/a.rs GONE: no longer declared; remove it from const_bool_switch_baseline.json", out)

    def test_test_code_and_non_literal_values_are_ignored(self):
        self.put("src/a_tests.rs", "const T: bool = true;\n")
        self.put("src/tests.rs", "const T: bool = true;\n")
        self.assertEqual(collect(self.root), {})
        text = "\n".join([
            "#[cfg(test)]",
            "mod t {",
            '    const S: &str = "{";',
            "    const IN_MOD: bool = true;",
            "}",
            "#[cfg(test)]",
            "const ITEM: bool = false;",
            "#[cfg(all(test, unix))] const SAME_LINE: bool = true;",
            "const COMPUTED: bool = cfg!(unix);",
            "// const COMMENT: bool = true;",
            'const TEXT: &str = "const QUOTED: bool = true;";',
            "#[cfg(not(test))]",
            "const PROD: bool = false;",
            "impl S { const ASSOC: bool = true; }",
            "pub const WRAPPED: bool =",
            "    false;",
            "const WRAPPED_COMPUTED: bool =",
            "    cfg!(unix);",
        ])
        self.assertEqual(declarations(text), [(13, "PROD"), (14, "ASSOC"), (15, "WRAPPED")])

    def test_bad_baseline_fails_closed(self):
        self.put("src/a.rs", "fn f() {}\n")
        for rows in ({"src/a.rs": "X"}, [["src/a.rs"]], [["src/a.rs", ""]], [["src/a.rs", 1]]):
            self.assertEqual(self.run_main(rows)[0], 1)

    def test_base_ref_rejects_added_baseline_rows(self):
        def git(*args):
            subprocess.run(["git", "-C", str(self.root), *args], check=True, capture_output=True)

        git("init", "--quiet")
        git("config", "user.email", "t@example.com")
        git("config", "user.name", "t")
        self.put("src/a.rs", "const OLD: bool = true;\n")
        self.assertEqual(self.run_main([["src/a.rs", "OLD"]])[0], 0)
        git("add", "-A")
        git("commit", "--quiet", "-m", "base")
        self.put("src/a.rs", "const OLD: bool = true;\nconst NEW: bool = true;\n")
        rc, out = self.run_main([["src/a.rs", "NEW"], ["src/a.rs", "OLD"]], "--base-ref", "HEAD")
        self.assertEqual(rc, 1)
        self.assertIn("src/a.rs NEW: baseline row added over HEAD", out)
        self.assertEqual(self.run_main([["src/a.rs", "NEW"], ["src/a.rs", "OLD"]])[0], 0)
        git("rm", "--quiet", "--cached", BASELINE.relative_to(ratchet.ROOT).as_posix())
        git("commit", "--quiet", "-m", "no list")
        self.assertEqual(self.run_main([["src/a.rs", "NEW"], ["src/a.rs", "OLD"]], "--base-ref", "HEAD")[0], 0)


if __name__ == "__main__":
    unittest.main()
