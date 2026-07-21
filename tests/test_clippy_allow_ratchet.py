import importlib.util
import tempfile
import unittest
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_clippy_allow_ratchet", ROOT / "scripts" / "check_clippy_allow_ratchet.py"
)
assert SPEC and SPEC.loader
RATCHET = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RATCHET)


class ClippyAllowRatchetTest(unittest.TestCase):
    def test_checked_in_baseline_matches_current_occurrences(self) -> None:
        problems = RATCHET.validate_occurrences(
            RATCHET.collect_occurrences(), RATCHET.load_baseline()
        )
        self.assertEqual(problems, [])

    def test_new_allow_occurrence_fails(self) -> None:
        baseline = Counter({("src/example.rs", "too_many_arguments"): 1})
        actual = baseline.copy()
        actual[("src/example.rs", "too_many_arguments")] += 1
        problems = RATCHET.validate_occurrences(actual, baseline)
        self.assertEqual(len(problems), 1)
        self.assertIn("baseline 1", problems[0])

    def test_new_path_or_lint_occurrence_fails(self) -> None:
        actual = Counter({("src/new.rs", "type_complexity"): 1})
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), 1)
        self.assertIn("baseline 0", problems[0])

    def _collect_source(self, text: str) -> Counter[tuple[str, str]]:
        with tempfile.TemporaryDirectory(dir=ROOT) as temp_dir:
            root = Path(temp_dir)
            source = root / "sample.rs"
            source.write_text(text, encoding="utf-8")
            original_root = RATCHET.REPO_ROOT
            try:
                RATCHET.REPO_ROOT = root
                return RATCHET.collect_occurrences(root)
            finally:
                RATCHET.REPO_ROOT = original_root

    def test_only_attributes_are_counted(self) -> None:
        actual = self._collect_source(
            "// clippy::too_many_arguments\n"
            "#[allow(\n    clippy::too_many_arguments,\n    clippy::type_complexity\n)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)
        self.assertEqual(actual[("sample.rs", "type_complexity")], 1)
        self.assertEqual(sum(actual.values()), 2)

    def test_clippy_all_group_allow_is_ratchet_visible(self) -> None:
        actual = self._collect_source("#![allow(clippy::all)]\nfn sample() {}\n")
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], 1)
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), len(RATCHET.LINTS))

    def test_cfg_attr_nested_allow_is_ratchet_visible(self) -> None:
        actual = self._collect_source(
            "#[cfg_attr(all(), allow(clippy::too_many_arguments))]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), 1)
        self.assertIn("too_many_arguments", problems[0])

    def test_cfg_attr_nested_group_allow_is_ratchet_visible(self) -> None:
        actual = self._collect_source(
            "#[cfg_attr(all(), expect(clippy::complexity))]\n"
            "fn sample() {}\n"
        )
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], 1)
        self.assertEqual(len(RATCHET.validate_occurrences(actual, Counter())), 4)

    def test_reason_string_parentheses_do_not_hide_allow(self) -> None:
        actual = self._collect_source(
            '#[allow(clippy::too_many_arguments, reason = "legacy (tracked)")]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), 1)
        self.assertIn("too_many_arguments", problems[0])

    # ------------------------------------------------------------------
    # Lexical pre-pass: comments
    # ------------------------------------------------------------------
    def test_block_comment_bracket_does_not_split_attribute(self) -> None:
        # The `]` inside the block comment must not close the attribute early.
        actual = self._collect_source(
            "#[allow(clippy::too_many_arguments /* ] */)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_block_comment_quote_and_bracket_are_neutralized(self) -> None:
        # A quote AND a bracket together inside a block comment.
        cleaned, ambiguous = RATCHET.neutralize_source('#[allow(/* " ] */ x)]\n')
        self.assertFalse(ambiguous)
        self.assertNotIn('"', cleaned)
        self.assertNotIn("]", cleaned[: cleaned.index("x")])  # bracket gone from comment
        actual = self._collect_source(
            '#[allow(/* " ] */ clippy::too_many_arguments)]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_nested_block_comment_is_fully_neutralized(self) -> None:
        # `]` sits between the inner and outer close; only nesting-aware parsing
        # keeps it inside the comment.
        actual = self._collect_source(
            "#[allow(clippy::too_many_arguments /* a /* b */ ] */)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_line_comment_bracket_and_quote_are_neutralized(self) -> None:
        cleaned, ambiguous = RATCHET.neutralize_source('let _ = 0; // ] " ) note\n')
        self.assertFalse(ambiguous)
        self.assertNotIn("]", cleaned)
        self.assertNotIn('"', cleaned)
        actual = self._collect_source(
            "#[allow(clippy::too_many_arguments)] // trailing ] \" ) note\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    # ------------------------------------------------------------------
    # Lexical pre-pass: string / raw string / byte string literals
    # ------------------------------------------------------------------
    def test_raw_string_with_brackets_parens_and_quote(self) -> None:
        # r#"a ) ] " b"# must be consumed whole (no escapes, hash-counted end).
        actual = self._collect_source(
            '#[allow(a, r#"a ) ] " b"#, clippy::too_many_arguments)]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_raw_byte_string_with_brackets(self) -> None:
        # Internal `"` ensures a plain-string misread would desync (proving the
        # raw rule is doing the work), plus brackets/parens inside.
        actual = self._collect_source(
            '#[allow(a, br#"] ) " ["#, clippy::too_many_arguments)]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_byte_string_with_brackets(self) -> None:
        actual = self._collect_source(
            '#[allow(a, b") ] \\" (", clippy::too_many_arguments)]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_c_string_with_brackets(self) -> None:
        # Rust 2024 C string c"..." (has \" escapes like a plain string).
        actual = self._collect_source(
            '#[allow(a, c") ] \\" (", clippy::too_many_arguments)]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_raw_c_string_with_brackets_and_quote(self) -> None:
        actual = self._collect_source(
            '#[allow(a, cr#"a" ) ] "b"#, clippy::too_many_arguments)]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_raw_c_string_exact_repro_counts_one(self) -> None:
        # Exact reviewer repro: cr raw string must not early-terminate and expose
        # the ) ] inside as real structure; the nested allow must count as 1.
        actual = self._collect_source(
            '#[foo(cr#"a" ) ] "b"#, allow(clippy::too_many_arguments))]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_c_and_cr_identifiers_do_not_trigger_string(self) -> None:
        # Variables/idents named c / cr and identifiers ending in c/cr must NOT
        # be misread as C-string prefixes.
        cleaned, ambiguous = RATCHET.neutralize_source(
            "let c = 1; let cr = 2; let sync = 3; let incr = c + cr;\n"
        )
        self.assertFalse(ambiguous)
        self.assertEqual(cleaned, "let c = 1; let cr = 2; let sync = 3; let incr = c + cr;\n")

    # ------------------------------------------------------------------
    # Lexical pre-pass: char / byte-char literals and the lifetime split
    # ------------------------------------------------------------------
    def test_char_literal_close_paren_does_not_end_body(self) -> None:
        # The ')' inside the char literal must not close the allow(...) body and
        # drop the trailing lint.
        actual = self._collect_source(
            "#[allow(a, ')', clippy::too_many_arguments)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_char_literal_close_bracket_does_not_split_attribute(self) -> None:
        actual = self._collect_source(
            "#[allow(a, ']', clippy::too_many_arguments)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_char_literal_quote_is_neutralized(self) -> None:
        cleaned, ambiguous = RATCHET.neutralize_source("let _ = '\"'; let x = 0;\n")
        self.assertFalse(ambiguous)
        self.assertNotIn('"', cleaned)
        actual = self._collect_source(
            "#[allow(a, '\"', clippy::too_many_arguments)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_byte_char_literal_bracket_is_neutralized(self) -> None:
        actual = self._collect_source(
            "#[allow(a, b']', clippy::too_many_arguments)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_lifetime_is_not_a_char_literal(self) -> None:
        # 'a is a lifetime (no closing quote); the quote must be left in place and
        # the following bracket structure must parse normally.
        cleaned, ambiguous = RATCHET.neutralize_source("struct S<'a>(&'a [u8]);\n")
        self.assertFalse(ambiguous)
        self.assertIn("[u8]", cleaned)  # brackets survive intact
        actual = self._collect_source(
            "struct S<'a>(&'a u8);\n"
            "#[allow(clippy::too_many_arguments)]\n"
            "fn sample<'a>() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)

    def test_escaped_char_literals_are_neutralized(self) -> None:
        for lit in ("'\\''", "'\\n'", "'\\\\'", "'\\x5d'", "'\\u{5d}'"):
            actual = self._collect_source(
                f"#[allow(a, {lit}, clippy::too_many_arguments)]\n"
                "fn sample() {}\n"
            )
            self.assertEqual(
                actual[("sample.rs", "too_many_arguments")], 1, msg=lit
            )

    # ------------------------------------------------------------------
    # Fail-closed: ambiguity must inflate the count, never silently zero it.
    # ------------------------------------------------------------------
    def test_unterminated_string_fails_closed(self) -> None:
        actual = self._collect_source(
            "#[allow(clippy::too_many_arguments)]\n"
            'fn sample() { let s = "oops; }\n'
        )
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], RATCHET.AMBIGUOUS_SENTINEL)
        self.assertEqual(len(RATCHET.validate_occurrences(actual, Counter())), 4)

    def test_unterminated_block_comment_fails_closed(self) -> None:
        actual = self._collect_source(
            "#[allow(clippy::too_many_arguments)]\n"
            "fn sample() {} /* never closed\n"
        )
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], RATCHET.AMBIGUOUS_SENTINEL)

    def test_unbalanced_allow_paren_fails_closed(self) -> None:
        # allow( with no closing paren: must NOT silently drop the body to zero.
        actual = self._collect_source(
            "#[allow(clippy::too_many_arguments]\n"
            "fn sample() {}\n"
        )
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], RATCHET.AMBIGUOUS_SENTINEL)

    def test_unbalanced_attribute_bracket_fails_closed(self) -> None:
        actual = self._collect_source(
            "#[allow(clippy::too_many_arguments)\n"
            "fn sample() {}\n"
        )
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], RATCHET.AMBIGUOUS_SENTINEL)


if __name__ == "__main__":
    unittest.main()
