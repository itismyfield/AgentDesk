"""Self-tests for scripts/check_comment_only_change.py.

The checker authorises skipping human review, so its false negatives are the
expensive direction: every case below that must FAIL is a change the campaign
would otherwise wave through unread.
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

import check_comment_only_change as checker  # noqa: E402


def normal_form(text: str) -> str:
    return checker.normalize(text)[0]


class NormalFormTests(unittest.TestCase):
    def assertSameCode(self, before: str, after: str) -> None:
        self.assertIsNone(
            checker.first_divergence(before, after),
            f"expected comment-only:\n{normal_form(before)!r}\n{normal_form(after)!r}",
        )

    def assertCodeDiffers(self, before: str, after: str) -> None:
        self.assertIsNotNone(
            checker.first_divergence(before, after),
            f"expected a code difference:\n{normal_form(before)!r}",
        )

    def test_trailing_comment_edit_is_comment_only(self) -> None:
        self.assertSameCode("let x = 1; // why\n", "let x = 1; // a better why\n")

    def test_trailing_comment_removal_is_comment_only(self) -> None:
        self.assertSameCode("let x = 1; // why\n", "let x = 1;\n")

    def test_whole_comment_line_removal_is_comment_only(self) -> None:
        before = "// leading\nfn a() {}\n// trailing\nfn b() {}\n"
        self.assertSameCode(before, "fn a() {}\nfn b() {}\n")

    def test_doc_comment_forms_are_all_stripped(self) -> None:
        before = "//! module\n/// item\n/** block doc */\n/*! inner doc */\nfn a() {}\n"
        self.assertSameCode(before, "fn a() {}\n")

    def test_single_character_code_change_is_detected(self) -> None:
        self.assertCodeDiffers("let x = 1;\n", "let x = 2;\n")

    def test_identifier_rename_is_detected(self) -> None:
        self.assertCodeDiffers("fn alpha() {}\n", "fn alpah() {}\n")

    def test_string_literal_content_change_is_detected(self) -> None:
        self.assertCodeDiffers('let s = "hello";\n', 'let s = "hallo";\n')

    def test_whitespace_inside_a_literal_is_significant(self) -> None:
        self.assertCodeDiffers('let s = "a  b";\n', 'let s = "a b";\n')

    def test_slash_slash_inside_a_string_is_not_a_comment(self) -> None:
        self.assertCodeDiffers('let s = "http://a";\n', 'let s = "http://b";\n')

    def test_slash_slash_inside_a_raw_string_is_not_a_comment(self) -> None:
        before = 'let s = r"a // b";\n'
        self.assertCodeDiffers(before, 'let s = r"a // c";\n')
        self.assertSameCode(before, 'let s = r"a // b"; // note\n')

    def test_hashed_raw_string_keeps_its_quote_and_hash_payload(self) -> None:
        before = 'let s = r##"has "# inside // too"##;\n'
        self.assertSameCode(before, 'let s = r##"has "# inside // too"##; // note\n')
        self.assertCodeDiffers(before, 'let s = r##"has "# inside // two"##;\n')

    def test_byte_string_and_byte_char_literals_survive(self) -> None:
        self.assertCodeDiffers('let b = b"//x";\n', 'let b = b"//y";\n')
        self.assertCodeDiffers("let c = b'/';\n", "let c = b'*';\n")

    def test_char_literal_slash_does_not_open_a_comment(self) -> None:
        before = "let c = '/'; let d = 1;\n"
        self.assertSameCode(before, "let c = '/'; let d = 1; // tail\n")
        self.assertCodeDiffers(before, "let c = '/'; let d = 2;\n")

    def test_escaped_char_literals_do_not_desync_the_scanner(self) -> None:
        before = "let q = '\\''; let b = '\\\\'; let u = '\\u{2F}'; let n = 1;\n"
        self.assertSameCode(before, before.replace("let n = 1;", "let n = 1; // c"))
        self.assertCodeDiffers(before, before.replace("let n = 1;", "let n = 2;"))

    def test_lifetimes_are_not_read_as_char_literals(self) -> None:
        before = "fn f<'a, 'b>(x: &'a str) -> &'b str { let c = '/'; x }\n"
        self.assertSameCode(before, before.rstrip("\n") + " // tail\n")
        self.assertCodeDiffers(before, before.replace("'b>", "'c>"))

    def test_nested_block_comments_are_removed_as_one_comment(self) -> None:
        before = "fn a() { /* outer /* inner */ still outer */ let x = 1; }\n"
        self.assertSameCode(before, "fn a() { let x = 1; }\n")

    def test_multi_line_nested_block_comment_is_removed(self) -> None:
        before = "fn a() {\n/* outer\n/* inner */\nstill outer */\nlet x = 1;\n}\n"
        self.assertSameCode(before, "fn a() {\nlet x = 1;\n}\n")

    def test_inner_close_does_not_terminate_a_nested_block_comment(self) -> None:
        # Under C's non-nesting rule the first `*/` would end the comment and
        # `let leaked = 1;` would be code; Rust nests, so it stays commented.
        self.assertSameCode("/* /* */ let leaked = 1; */\nfn a() {}\n", "fn a() {}\n")
        self.assertCodeDiffers("/* */ let leaked = 1;\nfn a() {}\n", "fn a() {}\n")

    def test_quote_inside_a_block_comment_does_not_open_a_string(self) -> None:
        before = 'fn a() { /* an " unclosed quote */ let x = 1; }\n'
        self.assertSameCode(before, "fn a() { let x = 1; }\n")

    def test_comment_markers_inside_a_line_comment_are_inert(self) -> None:
        self.assertSameCode('fn a() {} // /* "\nfn b() {}\n', "fn a() {}\nfn b() {}\n")

    def test_inline_comment_removal_keeps_token_separation(self) -> None:
        self.assertSameCode("foo/*c*/bar\n", "foo bar\n")
        self.assertCodeDiffers("foo/*c*/bar\n", "foobar\n")

    def test_multi_line_string_keeps_its_internal_newline(self) -> None:
        self.assertCodeDiffers('let s = "a\nb";\n', 'let s = "a b";\n')

    def test_line_numbers_do_not_affect_the_verdict(self) -> None:
        before = "// one\n// two\n// three\nfn a() {}\n"
        self.assertSameCode(before, "fn a() {}\n")

    def test_indentation_and_blank_line_reflow_reads_as_comment_only(self) -> None:
        self.assertSameCode("fn a() {\n    let x = 1;\n}\n", "fn a() {\n\nlet x = 1;\n}\n")

    def test_reported_lines_point_at_the_diverging_source(self) -> None:
        before = "// header\nfn a() {\n    let x = 1;\n}\n"
        after = "fn a() {\n    let x = 2;\n}\n"
        self.assertEqual(checker.first_divergence(before, after), (3, 2))


class GitIntegrationTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="comment-only-change-")
        self.root = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        (self.root / "scripts").mkdir()
        for name in ("check_comment_only_change.py", "rust_lex.py"):
            (self.root / "scripts" / name).write_bytes(
                (REPO_ROOT / "scripts" / name).read_bytes()
            )
        (self.root / "src").mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "gate@example.invalid")
        self.git("config", "user.name", "gate")

    def git(self, *args: str) -> None:
        subprocess.run(
            ["git", "-C", str(self.root), *args],
            check=True,
            capture_output=True,
        )

    def write(self, rel: str, body: str) -> None:
        target = self.root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(textwrap.dedent(body), encoding="utf-8")

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return subprocess.run(
            ["git", "-C", str(self.root), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def run_checker(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(self.root / "scripts" / "check_comment_only_change.py"), *args],
            cwd=self.root,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
            capture_output=True,
            text=True,
            check=False,
        )

    def seed(self) -> str:
        self.write("src/lib.rs", '''\
            //! Header.
            pub fn greet() -> &'static str {
                // chatty
                "hello // world"
            }
            ''')
        return self.commit("seed")

    def test_comment_only_commit_passes(self) -> None:
        base = self.seed()
        self.write("src/lib.rs", '''\
            pub fn greet() -> &'static str {
                "hello // world"
            }
            ''')
        self.commit("drop comments")
        result = self.run_checker(base, "HEAD")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("changed Rust files: 1", result.stdout)

    def test_code_change_fails_and_names_the_file_and_lines(self) -> None:
        base = self.seed()
        self.write("src/lib.rs", '''\
            pub fn greet() -> &'static str {
                "hello // worlds"
            }
            ''')
        self.commit("edit the literal")
        result = self.run_checker(base, "HEAD")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("src/lib.rs: code differs after comment removal", result.stdout)
        self.assertIn(":4:", result.stdout)
        self.assertIn(":2:", result.stdout)

    def test_working_tree_is_the_default_head(self) -> None:
        base = self.seed()
        self.write("src/lib.rs", '''\
            pub fn greet() -> &'static str {
                "hello // world"
            }
            ''')
        self.assertEqual(self.run_checker(base).returncode, 0)
        self.write("src/lib.rs", '''\
            pub fn greet() -> &'static str {
                "bye // world"
            }
            ''')
        self.assertEqual(self.run_checker(base).returncode, 1)

    def test_added_deleted_and_renamed_files_fail(self) -> None:
        base = self.seed()
        self.write("src/extra.rs", "pub fn extra() {}\n")
        self.commit("add a file")
        self.assertIn("added file", self.run_checker(base, "HEAD").stdout)

        base = self.commit_state()
        self.git("rm", "-q", "src/extra.rs")
        self.commit("delete a file")
        self.assertIn("deleted file", self.run_checker(base, "HEAD").stdout)

        base = self.commit_state()
        self.git("mv", "src/lib.rs", "src/renamed.rs")
        self.commit("rename a file")
        result = self.run_checker(base, "HEAD")
        self.assertEqual(result.returncode, 1)
        self.assertIn("renamed or copied", result.stdout)

    def commit_state(self) -> str:
        return subprocess.run(
            ["git", "-C", str(self.root), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def test_non_rust_change_fails_unless_allowed(self) -> None:
        base = self.seed()
        self.write("README.md", "# hi\n")
        self.commit("touch a doc")
        self.assertEqual(self.run_checker(base, "HEAD").returncode, 1)
        self.assertEqual(self.run_checker(base, "HEAD", "--allow-non-rust").returncode, 0)

    def test_unresolvable_ref_exits_two(self) -> None:
        self.seed()
        result = self.run_checker("no-such-ref", "HEAD")
        self.assertEqual(result.returncode, 2)
        self.assertIn("ERROR:", result.stderr)


if __name__ == "__main__":
    unittest.main()
