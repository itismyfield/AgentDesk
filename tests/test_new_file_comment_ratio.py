"""Self-tests for scripts/check_new_file_comment_ratio.py.

Two directions matter equally: the gate must go red on the shape that caused
PR #5953 (a large new file that is mostly prose), and it must stay green on
every existing file, or retroactive enforcement would red 200+ files at once
and the gate would be reverted rather than obeyed.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import check_new_file_comment_ratio as gate  # noqa: E402


def measure(body: str, path: str = "src/probe.rs"):
    return gate.measure(path, textwrap.dedent(body))


class LineClassificationTests(unittest.TestCase):
    def test_blank_lines_count_for_neither_side(self) -> None:
        result = measure("""\
            // one

            fn a() {}

            """)
        self.assertEqual((result.comment_lines, result.code_lines), (1, 1))

    def test_trailing_comment_leaves_the_line_as_code(self) -> None:
        result = measure("fn a() {} // why\n")
        self.assertEqual((result.comment_lines, result.code_lines), (0, 1))

    def test_every_doc_comment_form_counts_as_comment(self) -> None:
        result = measure("""\
            //! module
            /// item
            /*! inner */
            /** outer */
            fn a() {}
            """)
        self.assertEqual((result.comment_lines, result.code_lines), (4, 1))

    def test_multi_line_block_comment_counts_every_line(self) -> None:
        result = measure("""\
            /* one
               two
               three */
            fn a() {}
            """)
        self.assertEqual((result.comment_lines, result.code_lines), (3, 1))

    def test_comment_markers_inside_a_string_are_not_comments(self) -> None:
        result = measure('let s = "// not a comment";\n')
        self.assertEqual((result.comment_lines, result.code_lines), (0, 1))

    def test_lines_inside_a_multi_line_literal_are_not_comments(self) -> None:
        result = measure('''\
            let s = r#"
            // still string content
            "#;
            ''')
        self.assertEqual((result.comment_lines, result.code_lines), (0, 3))

    def test_cfg_test_module_lines_leave_the_measurement(self) -> None:
        result = measure("""\
            fn a() {}
            #[cfg(test)]
            mod tests {
                // a comment that must not count
                // another
                #[test]
                fn t() {}
            }
            """)
        self.assertEqual((result.comment_lines, result.code_lines), (0, 1))

    def test_a_file_that_is_only_a_cfg_test_module_measures_nothing(self) -> None:
        result = measure("""\
            #[cfg(test)]
            mod tests {
                // prose
                #[test]
                fn t() {}
            }
            """)
        self.assertEqual(result.measured, 0)
        self.assertEqual(result.ratio, 0.0)


class PathClassificationTests(unittest.TestCase):
    def test_test_file_names_and_suffixes_are_excluded(self) -> None:
        for path in (
            "src/a_test.rs",
            "src/a_tests.rs",
            "src/a/tests.rs",
            "src/a/integration_tests.rs",
            "tests/thing.rs",
            "src/server/routes/routes_tests/common.rs",
        ):
            self.assertTrue(gate.is_test_path(path), path)

    def test_production_paths_that_merely_look_test_like_are_included(self) -> None:
        for path in ("src/latest.rs", "src/protest.rs", "src/testing.rs", "src/a/test_support.rs"):
            self.assertFalse(gate.is_test_path(path), path)


class ThresholdTests(unittest.TestCase):
    def build(self, comments: int, code: int) -> gate.Measurement:
        body = "// c\n" * comments + "fn a() {}\n" * code
        return gate.measure("src/probe.rs", body)

    def test_exactly_at_the_cap_passes(self) -> None:
        result = self.build(10, 30)
        self.assertEqual(result.ratio, 0.25)
        self.assertFalse(result.ratio > gate.MAX_COMMENT_RATIO)

    def test_one_line_over_the_cap_fails(self) -> None:
        result = self.build(11, 30)
        self.assertGreater(result.ratio, gate.MAX_COMMENT_RATIO)

    def test_blank_padding_cannot_dilute_the_ratio(self) -> None:
        dense = self.build(11, 30)
        padded = gate.measure("src/probe.rs", ("// c\n" * 11) + ("fn a() {}\n\n" * 30))
        self.assertEqual(dense.ratio, padded.ratio)

    def test_the_measurable_size_floor_is_the_declared_constant(self) -> None:
        self.assertEqual(gate.MIN_MEASURED_LINES, 20)
        self.assertEqual(gate.MAX_COMMENT_RATIO, 0.25)


class GitGateTests(unittest.TestCase):
    OVER_CAP = "// prose\n" * 30 + "fn a() {}\n" * 30
    UNDER_CAP = "// prose\n" * 5 + "fn a() {}\n" * 30

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="new-file-comment-ratio-")
        self.root = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        (self.root / "scripts").mkdir()
        for name in (
            "check_new_file_comment_ratio.py",
            "generate_inventory_docs.py",
            "rust_lex.py",
        ):
            (self.root / "scripts" / name).write_bytes(
                (REPO_ROOT / "scripts" / name).read_bytes()
            )
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "gate@example.invalid")
        self.git("config", "user.name", "gate")

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.root), *args],
            check=True, capture_output=True, text=True,
        ).stdout

    def write(self, rel: str, body: str) -> None:
        target = self.root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body, encoding="utf-8")

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD").strip()

    def run_gate(self, base: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(self.root / "scripts" / "check_new_file_comment_ratio.py"),
                "--base-ref", base,
                "--head-ref", "HEAD",
            ],
            cwd=self.root,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
            capture_output=True, text=True, check=False,
        )

    def seed_with_an_over_cap_file(self) -> str:
        self.write("src/legacy.rs", self.OVER_CAP)
        return self.commit("seed")

    def test_a_new_over_cap_production_file_is_red(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/fresh.rs", self.OVER_CAP)
        self.commit("add a prose-heavy file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("src/fresh.rs", result.stdout)
        self.assertIn("50.0%", result.stdout)
        self.assertNotIn("src/legacy.rs", result.stdout)

    def test_a_new_compliant_production_file_is_green(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/fresh.rs", self.UNDER_CAP)
        self.commit("add a lean file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("added production Rust files: 1", result.stdout)

    def test_editing_an_existing_over_cap_file_stays_green(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/legacy.rs", self.OVER_CAP + "// one more comment\n")
        self.commit("grow the comments of an existing file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("added production Rust files: 0", result.stdout)

    def test_moving_an_existing_over_cap_file_stays_green(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.git("mv", "src/legacy.rs", "src/moved.rs")
        self.commit("move an existing file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_new_over_cap_test_files_are_out_of_scope(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/fresh_tests.rs", self.OVER_CAP)
        self.write("src/other_test.rs", self.OVER_CAP)
        self.write("tests/integration.rs", self.OVER_CAP)
        self.commit("add test files")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("added production Rust files: 0", result.stdout)

    def test_a_tiny_new_file_under_the_size_floor_is_out_of_scope(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/tiny.rs", "//! header\n//! second line\nmod a;\nmod b;\n")
        self.commit("add a tiny module file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_the_failure_names_the_allowed_comment_budget(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/fresh.rs", self.OVER_CAP)
        self.commit("add a prose-heavy file")
        result = self.run_gate(base)
        self.assertIn("at most 15 are allowed", result.stdout)
        self.assertIn("do not raise the cap", result.stdout)

    def test_an_unresolvable_base_exits_two(self) -> None:
        self.seed_with_an_over_cap_file()
        result = self.run_gate("no-such-ref")
        self.assertEqual(result.returncode, 2)
        self.assertIn("ERROR:", result.stderr)


if __name__ == "__main__":
    unittest.main()
