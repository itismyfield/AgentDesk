"""Boundary facts of the CI shell lexer."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "ci_shell_lex.py"
_spec = importlib.util.spec_from_file_location("ci_shell_lex", SCRIPT)
assert _spec and _spec.loader
lex = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = lex
_spec.loader.exec_module(lex)


def outer(text: str) -> list[tuple[str, str]]:
    """The outer shell's (kind, raw) tokens."""
    return [(token.kind, token.raw) for token in lex.scan_shell(text).tokens if not token.depth]


class WordBoundaries(unittest.TestCase):
    def test_a_closed_substitution_leaves_its_word_open(self) -> None:
        # The `#` after `)` or `))` is literal, so the `;` and the second command survive.
        for word in ("foo$(true)#tag", "foo$((1+2))#tag", "foo$(echo $(true))#tag",
                     "foo`true`#tag"):
            with self.subTest(word=word):
                text = f"cargo test --logfile {word}; cargo test --lib missing"
                scan = lex.scan_shell(text)
                self.assertEqual(scan.comment_masked, text)
                self.assertEqual(outer(text)[3:], [
                    ("WORD", word), ("OP", ";"), ("WORD", "cargo"), ("WORD", "test"),
                    ("WORD", "--lib"), ("WORD", "missing")])
                self.assertEqual(scan.issues, ())

    def test_backslash_parity_decides_whether_an_operator_is_escaped(self) -> None:
        for text, tokens in (
                ("ok\\;#tag &&", [("WORD", "ok\\;#tag"), ("OP", "&&")]),
                ("ok\\|#tag &&", [("WORD", "ok\\|#tag"), ("OP", "&&")]),
                ("ok\\&#tag &&", [("WORD", "ok\\&#tag"), ("OP", "&&")]),
                ("ok\\\\;# note &&", [("WORD", "ok\\\\"), ("OP", ";")]),
                ("ok\\\\\\;#tag &&", [("WORD", "ok\\\\\\;#tag"), ("OP", "&&")])):
            with self.subTest(text=text):
                self.assertEqual(outer(text), tokens)
        self.assertEqual([token.value for token in lex.scan_shell("a\\;b 'c\\' \"d\\\"e\"")
                          .tokens], ["a;b", "c\\", 'd"e'])

    def test_a_comment_opens_only_where_a_word_would(self) -> None:
        self.assertEqual(outer("true &&# note\nsource x"), [
            ("WORD", "true"), ("OP", "&&"), ("NEWLINE", "\n"), ("WORD", "source"),
            ("WORD", "x")])
        self.assertEqual(lex.scan_shell("a x#tag # note").comment_masked, "a x#tag       ")
        for text in ("'x#tag'", "x'#tag'", '"x#; || y"'):
            with self.subTest(text=text):
                self.assertEqual(outer(text), [("WORD", text)])
        self.assertEqual(lex.comment_start("echo ok;# note"), 8)

    def test_redirections_are_not_control_operators(self) -> None:
        scan = lex.scan_shell("cargo test 2>&1 >&2 &>log >out")
        self.assertEqual([(token.kind, token.raw) for token in scan.tokens][2:], [
            ("REDIR", "2>&1"), ("REDIR", ">&2"), ("REDIR", "&>"), ("WORD", "log"),
            ("REDIR", ">"), ("WORD", "out")])
        self.assertNotIn("&", scan.syntax_masked)


class Nesting(unittest.TestCase):
    def test_operators_inside_a_substitution_stay_inside_it(self) -> None:
        for text in ("x=$(true &&\nfalse)", "echo `true; false | cat`",
                     'echo "$(echo `date`; true)"'):
            with self.subTest(text=text):
                scan = lex.scan_shell(text)
                self.assertEqual([token.kind for token in scan.tokens
                                  if not token.depth and token.kind != "WORD"], [])
                self.assertTrue(any(token.kind == "OP" for token in scan.tokens if token.depth))
                self.assertFalse(set(";&|") & set(scan.syntax_masked))
                self.assertEqual(scan.issues, ())

    def test_a_nested_command_is_lexed_at_its_own_depth(self) -> None:
        scan = lex.scan_shell("echo \"$(cargo test --lib x)\" `cargo build`")
        self.assertEqual([(token.raw, token.depth) for token in scan.tokens if token.depth], [
            ("cargo", 1), ("test", 1), ("--lib", 1), ("x", 1), ("cargo", 1), ("build", 1)])


class UnprovenBoundaries(unittest.TestCase):
    def test_unsupported_syntax_is_an_issue_not_a_guess(self) -> None:
        for text, kind in (
                ("cat <<EOF\nsource x\nEOF", "unsupported-heredoc"),
                ("cat <<-EOF", "unsupported-heredoc"),
                ("diff <(a) >(b)", "unsupported-process-substitution"),
                ("echo 'open", "unclosed-context"),
                ('echo "open', "unclosed-context"),
                ("echo $(open", "unclosed-context"),
                ("echo `open", "unclosed-context"),
                ("echo ${open", "unclosed-context"),
                ("echo `echo \\`date\\``", "ambiguous-boundary"),
                ("x=$(case a in a) b ;; esac)", "ambiguous-boundary"),
                ("echo ${x:-$(cargo test)}", "unsupported-expansion"),
                ("echo $((1 + $(cargo test)))", "unsupported-expansion"),
                ("echo $'a\\nb'", "unsupported-expansion")):
            with self.subTest(text=text):
                self.assertIn(kind, [issue.kind for issue in lex.scan_shell(text).issues])

    def test_heredoc_data_is_never_lexed_as_commands(self) -> None:
        scan = lex.scan_shell("cat <<EOF\n# ${PG_INCLUDE_ARGS[@]}\nsource x\nEOF")
        self.assertEqual([token.raw for token in scan.tokens], ["cat"])
        self.assertIn("# ${PG_INCLUDE_ARGS[@]}", scan.comment_masked)

    def test_supported_ci_forms_raise_no_issue(self) -> None:
        for text in ('echo "${PKG_CONFIG_PATH:-}" ${#A[@]} ${{ matrix.shard }}',
                     'cargo test -- "${SKIP_ARGS[@]}" 2>&1',
                     "for ((i = 1; i < 3; i += 2)); do :; done",
                     "limit=$((10 * 1024 * 1024))", "a <<< here"):
            with self.subTest(text=text):
                self.assertEqual(lex.scan_shell(text).issues, ())


    def test_a_cargo_the_lexer_could_not_bound_may_run(self) -> None:
        for text in ("cat <<EOF; cargo test --lib x", "echo ${x:-$(cargo test --lib x)}",
                     "cat <<EOF && cargo test"):
            with self.subTest(text=text):
                self.assertTrue(lex.may_execute_cargo(lex.scan_shell(text)))
        for text in ("python - <<'PY'", "echo ${x:-$(date)}", "cargo test --lib x",
                     "echo ${x:-cargo-deny}"):
            with self.subTest(text=text):
                self.assertFalse(lex.may_execute_cargo(lex.scan_shell(text)))


class ShellWords(unittest.TestCase):
    def test_only_plain_words_are_read_as_array_values(self) -> None:
        self.assertEqual(lex.shell_words("\n  --skip a#b  # note\n  --skip 'c d'\n"),
                         ("--skip", "a#b", "--skip", "c d"))
        for text in ("--skip a; --skip b", "--skip 'open", "--skip a >log", "--skip <(a)",
                     "--skip $X", '--skip "${X:-a}"'):
            with self.subTest(text=text):
                self.assertIsNone(lex.shell_words(text))


if __name__ == "__main__":
    unittest.main()
