"""Unit tests for the #3036 production/test LoC split and giant-file registry.

Covers ``scripts/generate_inventory_docs.py``:
  * ``test_line_count`` only counts lines inside ``#[cfg(test)] mod`` blocks
    (inline ``#[cfg(test)]`` guards on production items stay production);
  * the giant-file flag keys off production LoC;
  * the registry loader and validator reject ghosts, unregistered new giants,
    and deadline-less entries.
"""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "generate_inventory_docs.py"

_SPEC = importlib.util.spec_from_file_location("generate_inventory_docs", SCRIPT_PATH)
GEN = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = GEN
_SPEC.loader.exec_module(GEN)


def _src(body: str) -> str:
    return textwrap.dedent(body).lstrip("\n")


class ProdTestSplitTest(unittest.TestCase):
    def test_cfg_test_mod_block_counts_as_test(self) -> None:
        text = _src(
            """
            pub fn a() {}
            pub fn b() {}

            #[cfg(test)]
            mod tests {
                #[test]
                fn t() {}
            }
            """
        )
        prod, test = GEN.split_prod_test_lines(text)
        self.assertEqual(test, 5)  # the #[cfg(test)] line through closing brace
        self.assertEqual(prod, GEN.line_count(text) - 5)

    def test_inline_cfg_test_guard_stays_production(self) -> None:
        # Inline #[cfg(test)] guards on struct fields / conditional logic shape
        # the production surface and must NOT be counted as test LoC.
        text = _src(
            """
            pub struct Runtime {
                #[cfg(test)]
                test_hook: bool,
                value: u32,
            }

            impl Runtime {
                pub fn run(&self) {
                    #[cfg(test)]
                    self.note();
                }
            }
            """
        )
        _prod, test = GEN.split_prod_test_lines(text)
        self.assertEqual(test, 0)

    def test_cfg_all_test_feature_module_counts_as_test(self) -> None:
        # Whole test modules gated by a compound cfg expression (common in this
        # tree, e.g. legacy-sqlite-tests) must also count as test LoC.
        text = _src(
            """
            pub fn a() {}

            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
            mod tests {
                #[test]
                fn t() {}
            }
            """
        )
        _prod, test = GEN.split_prod_test_lines(text)
        self.assertEqual(test, 5)

    def test_nested_all_test_not_feature_counts_as_test(self) -> None:
        # Balanced-paren predicate as used in src/server/routes/escalation.rs.
        text = _src(
            """
            pub fn a() {}

            #[cfg(all(test, not(feature = "legacy-sqlite-tests")))]
            mod tests {
                #[test]
                fn t() {}
            }
            """
        )
        _prod, test = GEN.split_prod_test_lines(text)
        self.assertEqual(test, 5)

    def test_quoted_test_feature_is_not_treated_as_test_cfg(self) -> None:
        # A feature literally containing "test" must NOT be mistaken for the
        # `test` cfg flag, and predicates that do not gate the module out of
        # production builds must not count as test.
        for predicate in (
            '#[cfg(feature = "legacy-sqlite-tests")]',
            '#[cfg(feature = "test")]',
            '#[cfg(all(unix, feature = "legacy-sqlite-tests"))]',
            '#[cfg(not(test))]',
            '#[cfg(any(test, feature = "x"))]',
        ):
            text = predicate + "\nmod m {\n    fn f() {}\n}\n"
            _prod, test = GEN.split_prod_test_lines(text)
            self.assertEqual(test, 0, predicate)

    def test_cfg_requires_test_predicate_logic(self) -> None:
        self.assertTrue(GEN.cfg_requires_test("test"))
        self.assertTrue(
            GEN.cfg_requires_test('all(test, feature = "legacy-sqlite-tests")')
        )
        self.assertTrue(
            GEN.cfg_requires_test('all(test, not(feature = "legacy-sqlite-tests"))')
        )
        self.assertTrue(GEN.cfg_requires_test("any(test, all(test, unix))"))
        self.assertFalse(GEN.cfg_requires_test("not(test)"))
        self.assertFalse(GEN.cfg_requires_test('any(test, feature = "x")'))
        self.assertFalse(GEN.cfg_requires_test('feature = "test"'))

    def test_giant_flag_uses_production_lines(self) -> None:
        # 1200 raw lines, but 400 of them are inside a #[cfg(test)] mod block,
        # leaving 800 production lines -> below the 1000 threshold -> not giant.
        prod_body = "\n".join(f"fn f{i}() {{}}" for i in range(800))
        test_lines = "\n".join("    // t" for _ in range(398))
        text = prod_body + "\n#[cfg(test)]\nmod tests {\n" + test_lines + "\n}\n"
        prod, test = GEN.split_prod_test_lines(text)
        self.assertLess(prod, GEN.GIANT_FILE_THRESHOLD)
        self.assertGreaterEqual(test, 1)


class CharLiteralLengthTest(unittest.TestCase):
    """Regression coverage for #3028 escape handling (PR #3234 review).

    ``char_literal_length`` must recognise *every* Rust char-literal escape
    form and return the full literal length, while still returning ``None``
    for lifetimes. The earlier implementation failed two escapes:
      * ``'\\''`` (escaped single quote) was reported as length 3, because the
        first char after ``\\`` was mistaken for the closing quote;
      * ``'\\\\'`` (escaped backslash) returned ``None``, because the escape
        consumed two chars and overran the closing quote.
    Both must now parse as length 4.
    """

    def test_simple_escapes(self) -> None:
        for literal in ("'\\n'", "'\\t'", "'\\r'", "'\\0'", "'\\\"'"):
            self.assertEqual(
                GEN.char_literal_length(literal, 0), 4, repr(literal)
            )

    def test_escaped_single_quote(self) -> None:
        # '\'' -- the bug: previously returned 3.
        self.assertEqual(GEN.char_literal_length("'\\''", 0), 4)

    def test_escaped_backslash(self) -> None:
        # '\\' -- the bug: previously returned None.
        self.assertEqual(GEN.char_literal_length("'\\\\'", 0), 4)

    def test_hex_escape(self) -> None:
        # '\x41' -> 6 chars.
        self.assertEqual(GEN.char_literal_length("'\\x41'", 0), 6)

    def test_unicode_escape(self) -> None:
        # '\u{1F600}' -> 11 chars.
        self.assertEqual(GEN.char_literal_length("'\\u{1F600}'", 0), 11)

    def test_plain_char(self) -> None:
        self.assertEqual(GEN.char_literal_length("'a'", 0), 3)

    def test_lifetimes_are_not_chars(self) -> None:
        # The original #3234 fix: lifetimes must not be parsed as char literals.
        for lifetime in ("'a", "'static", "'_", "'a,", "'a>"):
            self.assertIsNone(
                GEN.char_literal_length(lifetime, 0), repr(lifetime)
            )

    def test_escape_fix_does_not_break_lifetime_detection(self) -> None:
        # Escapes and lifetimes interleaved in one snippet: the char literals
        # must measure correctly and the lifetimes must still read as None.
        text = "let q = '\\''; fn f<'a>(x: &'a u8) { let b = '\\\\'; }"
        # Locate each interesting quote and assert the classification.
        q1 = text.index("'\\''")
        self.assertEqual(GEN.char_literal_length(text, q1), 4)  # '\''
        b1 = text.index("'\\\\'")
        self.assertEqual(GEN.char_literal_length(text, b1), 4)  # '\\'
        # The lifetimes <'a> and &'a must read as None.
        lt1 = text.index("<'a>") + 1
        self.assertIsNone(GEN.char_literal_length(text, lt1))
        lt2 = text.index("&'a") + 1
        self.assertIsNone(GEN.char_literal_length(text, lt2))


class BraceScannerTokenTest(unittest.TestCase):
    """Regression coverage for #3028.

    The brace scanner that bounds each ``#[cfg(test)] mod`` block must skip
    Rust tokens that contain stray ``{``/``}`` or quotes so the test block does
    not over-extend into the production code that follows it. Each case puts the
    problematic token *inside* an early test mod and then asserts production
    code after the block is still counted as production (i.e. the test block
    closed at its real brace).
    """

    def _split(self, body: str) -> tuple[int, int]:
        return GEN.split_prod_test_lines(_src(body))

    def test_lifetime_not_treated_as_char_literal(self) -> None:
        # `&'a self` and `impl<'a>` must not open a char literal that swallows
        # the closing brace (the original #3028 break inside turn_bridge/mod.rs).
        body = """
        #[cfg(test)]
        mod tests {
            struct W;
            impl<'a> Trait<'a> for W {
                fn make(&'a self) -> u32 {
                    0
                }
            }
        }

        pub fn production_after() -> u32 {
            1
        }
        """
        prod, test = self._split(body)
        self.assertEqual(test, 9)  # cfg(test) line through the mod's closing }
        self.assertEqual(prod, GEN.line_count(_src(body)) - 9)

    def _assert_block_does_not_overrun(self, body: str, block_lines: int) -> None:
        # The cfg(test) mod occupies exactly ``block_lines`` lines; the blank
        # line and ``production_after`` below it must stay production. If the
        # brace scanner over-ran, ``test`` would exceed ``block_lines``.
        text = _src(body)
        prod, test = self._split(body)
        self.assertEqual(test, block_lines)
        self.assertEqual(prod, GEN.line_count(text) - block_lines)

    def test_static_lifetime_not_treated_as_char(self) -> None:
        body = """
        #[cfg(test)]
        mod tests {
            const S: &'static str = "x";
            fn t() {}
        }

        pub fn production_after() {}
        """
        self._assert_block_does_not_overrun(body, 5)

    def test_char_literal_brace_ignored(self) -> None:
        # `'{'` / `'}'` char literals must not change brace depth.
        body = """
        #[cfg(test)]
        mod tests {
            fn t() {
                let open = '{';
                let close = '}';
                assert_ne!(open, close);
            }
        }

        pub fn production_after() {}
        """
        self._assert_block_does_not_overrun(body, 8)

    def test_escaped_char_literal_quote_ignored(self) -> None:
        # `'\''` (escaped single quote) must close correctly.
        body = """
        #[cfg(test)]
        mod tests {
            fn t() {
                let q = '\\'';
                let nl = '\\n';
                let _ = (q, nl);
            }
        }

        pub fn production_after() {}
        """
        self._assert_block_does_not_overrun(body, 8)

    def test_all_escape_forms_and_lifetimes_mixed(self) -> None:
        # Every escape form (escaped quote, escaped backslash, hex, unicode,
        # newline) interleaved with lifetimes inside one test mod. If any
        # escape were mis-measured the brace scanner would over-run into
        # ``production_after`` and ``test`` would exceed the block length.
        body = """
        #[cfg(test)]
        mod tests {
            fn t<'a>(x: &'a str) -> &'a str {
                let q = '\\'';
                let bs = '\\\\';
                let hx = '\\x41';
                let em = '\\u{1F600}';
                let nl = '\\n';
                let _ = (q, bs, hx, em, nl, x);
                x
            }
        }

        pub fn production_after() {}
        """
        self._assert_block_does_not_overrun(body, 12)

    def test_line_comment_brace_ignored(self) -> None:
        body = """
        #[cfg(test)]
        mod tests {
            fn t() {
                // a stray } in a comment must not close the block {
            }
        }

        pub fn production_after() {}
        """
        self._assert_block_does_not_overrun(body, 6)

    def test_raw_string_brace_ignored(self) -> None:
        body = """
        #[cfg(test)]
        mod tests {
            fn t() {
                let sql = r#"SELECT json_object('k', '}}}') WHERE x = '{'"#;
                let _ = sql;
            }
        }

        pub fn production_after() {}
        """
        self._assert_block_does_not_overrun(body, 7)

    def test_byte_literals_ignored(self) -> None:
        body = """
        #[cfg(test)]
        mod tests {
            fn t() {
                let b = b'}';
                let s = b"unbalanced { brace";
                let _ = (b, s);
            }
        }

        pub fn production_after() {}
        """
        self._assert_block_does_not_overrun(body, 8)

    def test_interleaved_test_and_production_blocks(self) -> None:
        # A test mod with a lifetime, followed by production, followed by a
        # second test mod: the first block must not absorb the production code
        # nor the second test block.
        body = """
        #[cfg(test)]
        mod first {
            fn helper<'a>(x: &'a str) -> &'a str { x }
        }

        pub fn middle_production() -> u32 { 42 }

        #[cfg(test)]
        mod second {
            #[test]
            fn t() { assert_eq!(super::middle_production(), 42); }
        }
        """
        text = _src(body)
        prod, test = self._split(body)
        # First block = 4 lines (cfg..closing }), second block = 5 lines: 9
        # test lines total. middle_production and the surrounding blanks stay
        # production (the first block must close at its real brace).
        self.assertEqual(test, 9)
        self.assertEqual(prod, GEN.line_count(text) - 9)
        self.assertGreaterEqual(prod, 1)


class TestOnlySubtreeTest(unittest.TestCase):
    def _with_src(self, files: dict[str, str]):
        from contextlib import contextmanager
        from tempfile import TemporaryDirectory
        from unittest import mock

        @contextmanager
        def ctx():
            with TemporaryDirectory() as tmp:
                root = Path(tmp)
                for rel, body in files.items():
                    target = root / rel
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(_src(body), encoding="utf-8")
                src_root = root / "src"
                with mock.patch.object(GEN, "REPO_ROOT", root), mock.patch.object(
                    GEN, "SRC_ROOT", src_root
                ):
                    yield root

        return ctx()

    def test_child_of_cfg_test_parent_mod_is_test_only(self) -> None:
        files = {
            "src/server/routes/mod.rs": (
                """
                pub mod kanban;
                #[cfg(all(test, feature = "legacy-sqlite-tests"))]
                mod routes_tests;
                """
            ),
            "src/server/routes/kanban.rs": "pub fn k() {}\n",
            "src/server/routes/routes_tests/common.rs": "pub fn helper() {}\n",
        }
        with self._with_src(files):
            test_only = {GEN.rel_posix(p) for p in GEN.test_only_module_files()}
        self.assertIn("src/server/routes/routes_tests/common.rs", test_only)
        self.assertNotIn("src/server/routes/kanban.rs", test_only)

    def test_module_declared_in_both_contexts_stays_production(self) -> None:
        # `schema` is declared once unconditionally and once under test; it must
        # remain production (declared in a non-test context wins).
        files = {
            "src/db/mod.rs": (
                """
                pub mod schema;
                #[cfg(all(test, feature = "legacy-sqlite-tests"))]
                mod schema_extra;
                """
            ),
            "src/db/schema.rs": "pub fn s() {}\n",
            "src/db/schema_extra.rs": "pub fn e() {}\n",
        }
        with self._with_src(files):
            test_only = {GEN.rel_posix(p) for p in GEN.test_only_module_files()}
        self.assertNotIn("src/db/schema.rs", test_only)
        self.assertIn("src/db/schema_extra.rs", test_only)

    def test_inline_cfg_test_mod_file_child_uses_inline_module_base(self) -> None:
        # Gap A / #4394: a file module declared inside an inline cfg(test) mod is
        # test-only, and Rust resolves it under <parent-stem>/<inline-mod>/.
        files = {
            "src/services/discord/inflight.rs": (
                """
                pub fn production_surface() {}

                #[cfg(test)]
                mod stall_recovery_tests {
                    mod flake_isolation_4361;
                }
                """
            ),
            "src/services/discord/inflight/stall_recovery_tests/flake_isolation_4361.rs": (
                "pub fn helper() {}\n"
            ),
            "src/services/discord/flake_isolation_4361.rs": (
                "pub fn wrong_old_base_must_stay_prod() {}\n"
            ),
        }
        with self._with_src(files):
            test_only = {GEN.rel_posix(p) for p in GEN.test_only_module_files()}
            modules = {entry.file_path: entry for entry in GEN.collect_modules()}
        self.assertIn(
            "src/services/discord/inflight/stall_recovery_tests/flake_isolation_4361.rs",
            test_only,
        )
        self.assertNotIn("src/services/discord/flake_isolation_4361.rs", test_only)
        child = modules[
            "src/services/discord/inflight/stall_recovery_tests/flake_isolation_4361.rs"
        ]
        self.assertEqual((child.prod_line_count, child.test_line_count), (0, 1))
        self.assertEqual(child.prod_line_count + child.test_line_count, child.line_count)

    def test_test_named_parent_file_declarations_seed_test_only_children(self) -> None:
        # Gap B / #4394: *_tests.rs files are excluded from inventory rows, but
        # their child file modules must still seed the test-only graph.
        files = {
            "src/server/routes/tests/auto_queue_preflight_harness_tests.rs": (
                """
                #[path = "preflight_harness/types.rs"]
                mod types;
                #[path = "preflight_harness/validation.rs"]
                mod validation;
                """
            ),
            "src/server/routes/tests/preflight_harness/types.rs": "pub fn ty() {}\n",
            "src/server/routes/tests/preflight_harness/validation.rs": (
                "pub fn validate() {}\n"
            ),
        }
        with self._with_src(files):
            test_only = {GEN.rel_posix(p) for p in GEN.test_only_module_files()}
            modules = {entry.file_path: entry for entry in GEN.collect_modules()}
        self.assertIn("src/server/routes/tests/preflight_harness/types.rs", test_only)
        self.assertIn("src/server/routes/tests/preflight_harness/validation.rs", test_only)
        self.assertNotIn("src/server/routes/tests/auto_queue_preflight_harness_tests.rs", modules)
        for rel in (
            "src/server/routes/tests/preflight_harness/types.rs",
            "src/server/routes/tests/preflight_harness/validation.rs",
        ):
            row = modules[rel]
            self.assertEqual(row.prod_line_count, 0)
            self.assertEqual(row.test_line_count, row.line_count)

    def test_path_attribute_order_resolves_test_only_targets(self) -> None:
        # Gap C / #4394: #[path] can appear before or after #[cfg(test)].
        # Without parsing the attr blob, these support files fall back to the
        # wrong default module path and remain production.
        files = {
            "src/lib.rs": (
                """
                #[cfg(test)]
                #[path = "support/cfg_then_path.rs"]
                mod cfg_then_path;

                #[path = "support/path_then_cfg.rs"]
                #[cfg(all(test, feature = "fixture"))]
                mod path_then_cfg;
                """
            ),
            "src/support/cfg_then_path.rs": "pub fn a() {}\n",
            "src/support/path_then_cfg.rs": "pub fn b() {}\n",
        }
        with self._with_src(files):
            test_only = {GEN.rel_posix(p) for p in GEN.test_only_module_files()}
        self.assertIn("src/support/cfg_then_path.rs", test_only)
        self.assertIn("src/support/path_then_cfg.rs", test_only)

    def test_inline_cfg_test_path_attr_uses_inline_module_base(self) -> None:
        # Gap A x C / #4394: inside an inline mod, #[path] is relative to the
        # inline module base, not to the parent file directory.
        files = {
            "src/widget.rs": (
                """
                pub fn production_surface() {}

                #[cfg(test)]
                mod tests {
                    #[path = "harness_support.rs"]
                    mod harness_support;
                }
                """
            ),
            "src/widget/tests/harness_support.rs": "pub fn helper() {}\n",
        }
        with self._with_src(files):
            test_only = {GEN.rel_posix(p) for p in GEN.test_only_module_files()}
            modules = {entry.file_path: entry for entry in GEN.collect_modules()}
        self.assertIn("src/widget/tests/harness_support.rs", test_only)
        row = modules["src/widget/tests/harness_support.rs"]
        self.assertEqual((row.prod_line_count, row.test_line_count), (0, 1))

    def test_non_test_path_declaration_keeps_child_production(self) -> None:
        files = {
            "src/lib.rs": (
                """
                #[cfg(test)]
                #[path = "shared.rs"]
                mod shared_for_tests;

                #[path = "shared.rs"]
                mod shared_for_prod;
                """
            ),
            "src/shared.rs": "pub fn shared() {}\n",
        }
        with self._with_src(files):
            test_only = {GEN.rel_posix(p) for p in GEN.test_only_module_files()}
            modules = {entry.file_path: entry for entry in GEN.collect_modules()}
        self.assertNotIn("src/shared.rs", test_only)
        self.assertEqual(modules["src/shared.rs"].prod_line_count, 1)
        self.assertEqual(modules["src/shared.rs"].test_line_count, 0)


class RegistryParseTest(unittest.TestCase):
    def test_strip_comment_keeps_hash_inside_string(self) -> None:
        self.assertEqual(
            GEN._strip_toml_comment('decompose_issue = "#3036"  # note'),
            'decompose_issue = "#3036"',
        )


class RegistryValidationTest(unittest.TestCase):
    def setUp(self) -> None:
        self._original_issue_metadata = GEN.load_giant_file_issue_metadata
        GEN.load_giant_file_issue_metadata = lambda: {
            number: {
                "number": number,
                "state": "open",
                "title": f"test tracker {number}",
                "owners": ["team"],
                "files": ["src/a.rs", "src/first.rs", "src/tracked.rs"],
            }
            for number in (1, 3036)
        }

    def tearDown(self) -> None:
        GEN.load_giant_file_issue_metadata = self._original_issue_metadata

    def _module(self, path: str, prod: int, *, giant: bool) -> "GEN.ModuleEntry":
        flags = ("giant-file",) if giant else ()
        return GEN.ModuleEntry(
            module_path=path.replace("/", "::"),
            file_path=path,
            line_count=prod,
            prod_line_count=prod,
            test_line_count=0,
            flags=flags,
        )

    def _patch_registry(self, grandfathered, entries, baseline_paths=None):
        if baseline_paths is None:
            baseline_paths = list(grandfathered)
        return (
            lambda: (
                list(grandfathered),
                [dict(e) for e in entries],
                list(baseline_paths) if baseline_paths is not None else None,
            )
        )

    def test_unregistered_giant_fails(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("unregistered giant", str(ctx.exception))

    def test_ghost_registration_fails(self) -> None:
        modules = [self._module("src/a.rs", 200, giant=False)]
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry(["src/a.rs"], [])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("ghost registration", str(ctx.exception))

    def test_entry_missing_deadline_fails(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {"file": "src/a.rs", "owner": "team", "decompose_issue": "#1"}
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("deadline", str(ctx.exception))

    def test_grandfather_swap_in_outside_baseline_fails(self) -> None:
        # `src/b.rs` is a current giant grandfathered in, but it is NOT in the
        # frozen baseline (a swap-in for the retired `src/a.rs`). Even though the
        # count did not grow, this must fail: new giants need an [[entry]].
        modules = [
            self._module("src/b.rs", 1500, giant=True),
        ]
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry(
            ["src/b.rs"], [], baseline_paths=["src/a.rs"]
        )
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("frozen", str(ctx.exception))
        self.assertIn("src/b.rs", str(ctx.exception))

    def test_grandfather_shrink_within_baseline_still_requires_backfill(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry(
            ["src/a.rs"], [], baseline_paths=["src/a.rs", "src/retired.rs"]
        )
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("metadata backfill is closed", str(ctx.exception))

    def test_missing_baseline_paths_fails(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = lambda: (["src/a.rs"], [], None)
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("grandfathered_baseline_paths", str(ctx.exception))

    def test_awaiting_backfill_records_are_rejected_after_4519(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry(["src/a.rs"], [])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("metadata backfill is closed", str(ctx.exception))

    def test_registry_has_zero_awaiting_backfill_records(self) -> None:
        grandfathered, _entries, _baseline = GEN.load_giant_file_registry()
        self.assertEqual(grandfathered, [])

    def test_legacy_decision_omission_normalizes_to_shrink(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "owner": "team",
            "deadline": "2026-08-31",
            "decompose_issue": "#1",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            regs = GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertEqual(regs[0].decision, "shrink")

    def test_explicit_valid_shrink_builds_row(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "shrink",
            "owner": "team",
            "deadline": "2026-08-31",
            "decompose_issue": "#1",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            regs = GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertEqual(regs[0].decision, "shrink")

    def test_explicit_valid_keep_builds_row(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "keep",
            "owner": "team",
            "keep_reason": "protocol | schema authority",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            regs = GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertEqual(regs[0].keep_reason, "protocol | schema authority")

    def test_shrink_missing_owner_fails(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "shrink",
            "deadline": "2026-08-31",
            "decompose_issue": "#1",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("real owner", str(ctx.exception))

    def test_keep_missing_owner_fails(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "keep",
            "keep_reason": "protocol authority",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("real owner", str(ctx.exception))

    def test_shrink_forbids_keep_reason(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "shrink",
            "owner": "team",
            "deadline": "2026-08-31",
            "decompose_issue": "#1",
            "keep_reason": "not applicable",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("forbids keep_reason", str(ctx.exception))

    def test_markdown_renderer_escapes_metadata_cells(self) -> None:
        rendered = GEN.render_giant_file_registry(
            [
                GEN.GiantFileRegistration(
                    file_path="src/a.rs",
                    decision="keep",
                    owner="team\\ops|primary\r\nline",
                    deadline="",
                    decompose_issue="",
                    keep_reason="protocol | schema authority",
                    prod_line_count=1500,
                )
            ]
        )
        self.assertIn("team\\\\ops\\|primary line", rendered)
        self.assertIn("protocol \\| schema authority", rendered)

    def test_keep_requires_reason_and_forbids_shrink_metadata(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "keep",
            "owner": "team",
            "keep_reason": "TBD",
            "deadline": "2026-08-31",
            "decompose_issue": "#1",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        error = str(ctx.exception)
        self.assertIn("keep_reason", error)
        self.assertIn("forbids deadline", error)
        self.assertIn("forbids decompose_issue", error)

    def test_shrink_rejects_malformed_calendar_deadline(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "shrink",
            "owner": "team",
            "deadline": "2026-02-30",
            "decompose_issue": "#1",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("calendar-valid", str(ctx.exception))

    def test_shrink_deadline_is_overdue_only_before_fixed_today(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        orig_registry = GEN.load_giant_file_registry
        orig_today = GEN.today_utc
        GEN.today_utc = lambda: __import__("datetime").date(2026, 8, 31)
        try:
            same_day = {
                "file": "src/a.rs",
                "decision": "shrink",
                "owner": "team",
                "deadline": "2026-08-31",
                "decompose_issue": "#1",
            }
            GEN.load_giant_file_registry = self._patch_registry([], [same_day])
            self.assertEqual(len(GEN.build_giant_registrations(modules)), 1)
            overdue = dict(same_day, deadline="2026-08-30")
            GEN.load_giant_file_registry = self._patch_registry([], [overdue])
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig_registry
            GEN.today_utc = orig_today
        self.assertIn("overdue", str(ctx.exception))

    def test_shrink_rejects_fake_or_self_referential_issues(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        orig = GEN.load_giant_file_registry
        try:
            for issue in ("TBD", "unknown", "none", "umbrella", "4519", "#4519"):
                entry = {
                    "file": "src/a.rs",
                    "decision": "shrink",
                    "owner": "team",
                    "deadline": "2026-08-31",
                    "decompose_issue": issue,
                }
                GEN.load_giant_file_registry = self._patch_registry([], [entry])
                with self.assertRaises(GEN.ParseError) as ctx:
                    GEN.build_giant_registrations(modules)
                self.assertIn("decompose_issue", str(ctx.exception), issue)
        finally:
            GEN.load_giant_file_registry = orig

    def test_shrink_rejects_issue_absent_from_checked_in_metadata(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "shrink",
            "owner": "team",
            "deadline": "2026-08-31",
            "decompose_issue": "#999",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("absent from checked-in open issue metadata", str(ctx.exception))

    def test_shrink_rejects_issue_outside_owner_scope(self) -> None:
        modules = [self._module("src/a.rs", 1500, giant=True)]
        entry = {
            "file": "src/a.rs",
            "decision": "shrink",
            "owner": "voice-runtime",
            "deadline": "2026-08-31",
            "decompose_issue": "#1",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("outside decompose_issue #1 owner scope", str(ctx.exception))

    def test_shrink_rejects_file_absent_from_issue_candidate_scope(self) -> None:
        modules = [self._module("src/not-listed.rs", 1500, giant=True)]
        entry = {
            "file": "src/not-listed.rs",
            "decision": "shrink",
            "owner": "team",
            "deadline": "2026-08-31",
            "decompose_issue": "#1",
        }
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], [entry])
        try:
            with self.assertRaises(GEN.ParseError) as ctx:
                GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        self.assertIn("not an explicit candidate", str(ctx.exception))

    def test_checked_in_issue_metadata_covers_every_shrink_entry(self) -> None:
        GEN.load_giant_file_issue_metadata = self._original_issue_metadata
        _grandfathered, entries, _baseline = GEN.load_giant_file_registry()
        issues = GEN.load_giant_file_issue_metadata()
        problems = [
            GEN.validate_decompose_issue_metadata(
                entry["file"], entry["owner"], entry["decompose_issue"], issues
            )
            for entry in entries
            if entry.get("decision", "shrink") == "shrink"
        ]
        self.assertEqual([problem for problem in problems if problem], [])

    def test_valid_registry_builds_rows(self) -> None:
        modules = [
            self._module("src/first.rs", 1200, giant=True),
            self._module("src/tracked.rs", 1500, giant=True),
        ]
        entries = [
            {
                "file": "src/first.rs",
                "decision": "keep",
                "owner": "team",
                "keep_reason": "cohesive generated parser table",
            },
            {
                "file": "src/tracked.rs",
                "owner": "team",
                "deadline": "2026-08-31",
                "decompose_issue": "#3036",
            },
        ]
        orig = GEN.load_giant_file_registry
        GEN.load_giant_file_registry = self._patch_registry([], entries)
        try:
            regs = GEN.build_giant_registrations(modules)
        finally:
            GEN.load_giant_file_registry = orig
        by_path = {r.file_path: r for r in regs}
        self.assertEqual(by_path["src/first.rs"].decision, "keep")
        self.assertEqual(by_path["src/tracked.rs"].deadline, "2026-08-31")
        self.assertEqual(by_path["src/tracked.rs"].owner, "team")


if __name__ == "__main__":
    unittest.main()
