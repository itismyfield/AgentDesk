"""Self-tests for production Rust added-comment budgets and density warnings."""

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
            "check_comment_only_change.py",
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

    def run_gate(self, base: str, head: str = "HEAD") -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(self.root / "scripts" / "check_new_file_comment_ratio.py"),
                "--base-ref", base,
                "--head-ref", head,
            ],
            cwd=self.root,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
            capture_output=True, text=True, check=False,
        )

    def seed_with_an_over_cap_file(self) -> str:
        self.write("src/legacy.rs", self.OVER_CAP)
        return self.commit("seed")

    def test_a_new_over_cap_file_with_equal_comments_and_code_warns(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/fresh.rs", self.OVER_CAP)
        self.commit("add a prose-heavy file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("::warning file=src/fresh.rs::", result.stdout)
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

    def test_pure_new_comments_in_an_existing_file_fail(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/legacy.rs", self.OVER_CAP + "// one more comment\n")
        self.commit("grow the comments of an existing file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("FAIL: added comments (1) exceed added code (0)", result.stdout)
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

    def test_the_warning_names_the_comment_density_budget(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/fresh.rs", self.OVER_CAP)
        self.commit("add a prose-heavy file")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("25%", result.stdout)
        self.assertIn("::warning file=src/fresh.rs::", result.stdout)

    def test_an_unresolvable_base_exits_two(self) -> None:
        self.seed_with_an_over_cap_file()
        result = self.run_gate("no-such-ref")
        self.assertEqual(result.returncode, 2)
        self.assertIn("ERROR:", result.stderr)

    def test_added_comments_are_aggregated_across_production_files(self) -> None:
        self.seed_with_an_over_cap_file()
        for comments, code, expected in ((2, 1, 1), (1, 1, 0), (1, 2, 0)):
            with self.subTest(comments=comments, code=code):
                base = self.git("rev-parse", "HEAD").strip()
                self.write(f"src/comment_{comments}_{code}.rs", "// explanation\n" * comments)
                self.write(f"src/code_{comments}_{code}.rs", "fn b() {}\n" * code)
                self.commit("aggregate production additions")
                self.assertEqual(self.run_gate(base).returncode, expected)

    def test_same_code_comment_correction_and_deletion_pass_but_expansion_fails(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/legacy.rs", self.OVER_CAP.replace("// prose\n", "// corrected\n", 1))
        self.commit("correct one comment")
        self.assertEqual(self.run_gate(base).returncode, 0)
        self.write("src/legacy.rs", "// corrected\n" + "fn a() {}\n" * 30)
        self.commit("delete stale comments")
        self.assertEqual(self.run_gate(base).returncode, 0)
        self.write("src/legacy.rs", self.OVER_CAP + "// expanded narrative\n")
        self.commit("expand comments without changing code")
        self.assertEqual(self.run_gate(base).returncode, 1)

    def test_exact_comment_moves_cancel_across_files(self) -> None:
        self.write("src/one.rs", "// moved\nfn a() {}\n")
        self.write("src/two.rs", "fn b() {}\n")
        base = self.commit("seed move")
        self.write("src/one.rs", "fn a() {}\n")
        self.write("src/two.rs", "// moved\nfn b() {}\n")
        self.commit("move existing explanation")
        self.assertEqual(self.run_gate(base).returncode, 0)

    def test_generated_outputs_outside_the_source_scope_are_excluded(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("target/generated/bindings.rs", "// generated\n" * 30)
        self.commit("add generator output")
        self.assertEqual(self.run_gate(base).returncode, 0)

    def test_build_script_and_claimed_generated_source_remain_production(self) -> None:
        for path in ("build.rs", "src/generated.rs"):
            with self.subTest(path=path):
                base = self.seed_with_an_over_cap_file() if path == "build.rs" else self.git("rev-parse", "HEAD").strip()
                self.write(path, "// @generated DO NOT EDIT\n// new prose\nfn a() {}\n")
                self.commit("add production source")
                self.assertEqual(self.run_gate(base).returncode, 1)

    def test_cfg_test_comments_do_not_consume_the_production_budget(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/probe.rs", "fn b() {}\n#[cfg(test)]\nmod tests {\n" + "// test prose\n" * 30 + "}\n")
        self.commit("add production and tests")
        self.assertEqual(self.run_gate(base).returncode, 0)

    def test_added_rows_are_lexed_in_full_file_context(self) -> None:
        self.write("src/probe.rs", 'fn a() { let s = r#"\nold literal\n"#; }\n/* old\nblock */\n')
        base = self.commit("seed multiline syntax")
        self.write("src/probe.rs", 'fn a() { let s = r#"\n// literal data\n"#; }\n/* old\nblock\nmore block */\n')
        self.commit("change literal and block comment")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("added comments (2) exceed added code (1)", result.stdout)

    def test_inline_comments_and_comment_markers_inside_literals_count_as_code(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/probe.rs", 'let s = "// literal"; // inline\nlet c = \'/\'; /* inline */\n')
        self.commit("add mixed rows")
        self.assertEqual(self.run_gate(base).returncode, 0)

    def test_cfg_test_marker_inside_a_raw_literal_does_not_exclude_code(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/probe.rs", 'let s = r#"\n#[cfg(test)]\nmod tests {\n// literal data\n}\n"#;\n' + "// production comment\n" * 5)
        self.commit("add literal fixture and comments")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("comments=5 code=6", result.stdout)

    def test_literal_changes_cannot_use_the_comment_correction_exception(self) -> None:
        self.write("src/probe.rs", '// old one\n// old two\nlet s = "old";\n')
        base = self.commit("seed literal")
        self.write("src/probe.rs", '// new one\n// new two\nlet s = "new";\n')
        self.commit("change literal with corrected comments")
        self.assertEqual(self.run_gate(base).returncode, 1)

    def test_unreadable_production_source_is_an_error(self) -> None:
        base = self.seed_with_an_over_cap_file()
        (self.root / "src" / "bad.rs").write_bytes(b"\xff\n")
        self.commit("add non UTF-8 source")
        result = self.run_gate(base)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("ERROR:", result.stderr)

    def test_invalid_head_and_broken_worktree_source_are_errors(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.assertEqual(self.run_gate(base, "no-such-head").returncode, 2)
        source = self.root / "src" / "legacy.rs"
        source.unlink()
        source.symlink_to("missing.rs")
        result = self.run_gate(base, gate.WORKING_TREE)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("ERROR:", result.stderr)

    def test_unexpected_checker_exception_never_reports_success(self) -> None:
        base = self.seed_with_an_over_cap_file()
        self.write("src/probe.rs", "fn b() {}\n")
        self.commit("add source for failed lexer")
        lexer = self.root / "scripts" / "rust_lex.py"
        with lexer.open("a") as output:
            output.write("\ndef lex_segments(line, state):\n    raise RuntimeError('injected lexer failure')\n")
        result = self.run_gate(base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("injected lexer failure", result.stderr)
        self.assertNotIn("OK:", result.stdout)


if __name__ == "__main__":
    unittest.main()
