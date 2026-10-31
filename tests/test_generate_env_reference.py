from pathlib import Path
import re
import shutil
import sys
import tempfile
import textwrap
import unittest
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import generate_env_reference as gen  # noqa: E402


class BlankTestModulesTests(unittest.TestCase):
    def test_cfg_test_module_is_blanked_but_line_numbers_survive(self) -> None:
        source = textwrap.dedent(
            """\
            const KEEP_ENV: &str = "AGENTDESK_KEEP";

            #[cfg(test)]
            mod tests {
                const DROP_ENV: &str = "AGENTDESK_DROP";
                fn f() { let _ = std::env::var("AGENTDESK_DROP_READ"); }
            }
            fn after() { let _ = std::env::var("AGENTDESK_AFTER"); }
            """
        )
        blanked = gen.blank_test_modules(source)
        self.assertEqual(blanked.count("\n"), source.count("\n"))
        self.assertIn("AGENTDESK_KEEP", blanked)
        self.assertNotIn("AGENTDESK_DROP", blanked)
        self.assertIn("AGENTDESK_AFTER", blanked)
        after_line = blanked.splitlines().index('fn after() { let _ = std::env::var("AGENTDESK_AFTER"); }') + 1
        self.assertEqual(after_line, 8)

    def test_not_test_cfg_module_is_kept(self) -> None:
        source = '#[cfg(not(test))]\nmod prod {\n    const X: &str = "AGENTDESK_PROD";\n}\n'
        self.assertIn("AGENTDESK_PROD", gen.blank_test_modules(source))


class DescriptionTests(unittest.TestCase):
    def test_prefers_comment_that_names_the_variable(self) -> None:
        lines = [
            "/// Default ON; set `AGENTDESK_FLAG` to `0` to disable. Second sentence.",
            "fn enabled() -> bool {",
            "    // unrelated note",
            '    std::env::var("AGENTDESK_FLAG").is_ok()',
            "}",
        ]
        self.assertEqual(
            gen.describe_site(lines, 4, "AGENTDESK_FLAG"),
            "Default ON; set `AGENTDESK_FLAG` to `0` to disable.",
        )

    def test_falls_back_to_adjacent_then_enclosing_fn_doc(self) -> None:
        adjacent = [
            "/// Fn doc.",
            "fn f() {",
            "    // Adjacent note. More.",
            '    std::env::var("AGENTDESK_X")',
            "}",
        ]
        self.assertEqual(gen.describe_site(adjacent, 4, "AGENTDESK_X"), "Adjacent note.")
        enclosing = ["/// Fn doc only.", "fn f() {", '    std::env::var("AGENTDESK_X")', "}"]
        self.assertEqual(gen.describe_site(enclosing, 3, "AGENTDESK_X"), "Fn doc only.")
        bare = ["fn f() {", '    std::env::var("AGENTDESK_X")', "}"]
        self.assertEqual(gen.describe_site(bare, 2, "AGENTDESK_X"), "")


class PatternTests(unittest.TestCase):
    def test_read_const_and_helper_patterns(self) -> None:
        source = textwrap.dedent(
            """\
            const STAMP_ENV: &str = "AGENTDESK_STAMP";
            pub(crate) static OTHER: &'static str = "ADK_OTHER";
            fn f() {
                let _ = std::env::var("HOME");
                let _ = env::var_os(STAMP_ENV);
                let _ = explicit_env_path("AGENTDESK_PATHY");
                let _ = resolve_with_env_pg(pool, "ADK_CHANNEL");
                std::env::set_var("AGENTDESK_NOT_A_READ", "1");
            }
            """
        )
        literal = {m.group("name") for m in gen._LITERAL_READ_RE.finditer(source)}
        consts = {m.group("ident"): m.group("name") for m in gen._CONST_DEF_RE.finditer(source)}
        helpers = {m.group("name") for m in gen._HELPER_READ_RE.finditer(source)}
        self.assertEqual(literal, {"HOME"})
        self.assertEqual(consts, {"STAMP_ENV": "AGENTDESK_STAMP", "OTHER": "ADK_OTHER"})
        self.assertEqual(helpers, {"AGENTDESK_PATHY", "ADK_CHANNEL"})
        self.assertNotIn("AGENTDESK_NOT_A_READ", helpers)


class SourceTreeStabilityTests(unittest.TestCase):
    """Render a scratch ``src/`` tree through the real file walk and renderer."""

    DEFINING = textwrap.dedent(
        """\
        /// `AGENTDESK_ALPHA` picks the alpha mode.
        const ALPHA_ENV: &str = "AGENTDESK_ALPHA";
        fn alpha() { let _ = std::env::var(ALPHA_ENV); }
        fn alpha_again() { let _ = std::env::var(ALPHA_ENV); }
        """
    )
    READER = 'fn beta() { let _ = std::env::var("AGENTDESK_BETA"); let _ = std::env::var("AGENTDESK_ALPHA"); }\n'

    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        self.write("src/config/alpha.rs", self.DEFINING)
        self.write("src/reader.rs", self.READER)

    def write(self, rel: str, text: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def render(self) -> str:
        with mock.patch.object(gen, "REPO_ROOT", self.root), mock.patch.object(
            gen, "SRC_ROOT", self.root / "src"
        ):
            return gen.render(gen.collect_variables(gen.production_rust_files()))

    def row(self, rendered: str, name: str) -> str:
        return next(line for line in rendered.splitlines() if line.startswith(f"| `{name}` |"))

    def test_cfg_test_item_does_not_enter_production_reference(self) -> None:
        self.write("src/items.rs", '''
            #[cfg(test)]
            fn only_test() { let _ = std::env::var("ADK_TEST_ITEM"); }
            #[cfg(all(unix, test))]
            pub(crate) async fn helper() { env_flag("ADK_TEST_HELPER", false); }
            #[cfg(test)] const NAME: &str = "ADK_TEST_CONST";
            #[cfg(test)] static VALUE: &str = "ADK_TEST_STATIC";
            #[cfg(not(test))]
            fn prod() { env_flag("AGENTDESK_ITEM_PROD", false); }
            #[cfg(any(test, unix))]
            fn shared() { env_flag("AGENTDESK_ITEM_SHARED", false); }
            struct Fields { #[cfg(test)] probe: bool, live: bool }
            fn after_field() { env_flag("AGENTDESK_AFTER_FIELD", false); }
        ''')
        output = self.render()
        self.assertIn("AGENTDESK_ITEM_PROD", output)
        self.assertIn("AGENTDESK_ITEM_SHARED", output)
        self.assertIn("AGENTDESK_AFTER_FIELD", output)
        for name in ("ADK_TEST_ITEM", "ADK_TEST_HELPER", "ADK_TEST_CONST", "ADK_TEST_STATIC"):
            self.assertNotIn(name, output)

    def test_named_helper_argument_records_the_reader_file(self) -> None:
        self.write("src/names.rs", 'pub const FLAG_ENV: &str = "AGENTDESK_DYNAMIC";')
        self.write("src/consumer.rs", 'fn read() -> bool { crate::utils::env_flag(FLAG_ENV, false) }')
        for prefix in ("crate::names::", "super::", "self::"):
            suffix = prefix.split("::")[0]
            self.write(f"src/direct_{suffix}.rs", f'fn read() {{ std::env::var({prefix}FLAG_ENV); }}')
            self.write(f"src/helper_{suffix}.rs", f'fn read() {{ env_flag({prefix}FLAG_ENV, false); }}')
        row = self.row(self.render(), "AGENTDESK_DYNAMIC")
        self.assertIn("`src/names.rs`", row)
        self.assertIn("`src/consumer.rs`", row)
        for suffix in ("crate", "super", "self"):
            self.assertIn(f"`src/direct_{suffix}.rs`", row)
            self.assertIn(f"`src/helper_{suffix}.rs`", row)

    def test_cfg_test_list_elements_preserve_production_readers(self) -> None:
        for source in (
            'fn f(k: K) { match k { #[cfg(test)] K::T => true, _ => std::env::var("AGENTDESK_PROD_LIST").is_ok() } }',
            'fn f(#[cfg(test)] mut p: u8) { std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { K::Pair(#[cfg(test)] K::T, || std::env::var("AGENTDESK_PROD_LIST")); }',
            'fn f(k: K) { match k { #[cfg(test)] K::T => { env_flag("ADK_TEST_HIDDEN", false) }, _ => std::env::var("AGENTDESK_PROD_LIST").is_ok() } }',
            'fn f(k: K) { match k { #[cfg(test)] K::T => { env_flag("ADK_TEST_HIDDEN", false) } _ => std::env::var("AGENTDESK_PROD_LIST").is_ok() } }',
            '#[cfg(test)] #[inline] fn gone() { env_flag("ADK_TEST_HIDDEN", false); } fn kept() { std::env::var("AGENTDESK_PROD_LIST"); }',
            '#[cfg(test)] union Probe { byte: u8 } fn kept() { std::env::var("AGENTDESK_PROD_LIST"); }',
            '#[cfg(test)] gone! { env_flag("ADK_TEST_HIDDEN", false); } fn kept() { std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] if tests::bypass() { return; } std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] if let Some(p) = tests::probe() { return; } std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] { env_flag("ADK_TEST_HIDDEN", false); } #[cfg(not(test))] { std::env::var("AGENTDESK_PROD_LIST"); } }',
            'fn f() { #[cfg(test)] match tests::probe() { _ => {} } std::env::var("AGENTDESK_PROD_LIST"); }',
            'struct Fields { #[cfg(test)] pub probe: bool, live: field_type!(std::env::var("AGENTDESK_PROD_LIST")), }',
            'fn f() { #[cfg(test)] if tests::bypass() {} else if tests::probe() {} else { env_flag("ADK_TEST_HIDDEN", false); } std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] let probe = if tests::bypass() {} else { env_flag("ADK_TEST_HIDDEN", false); }; std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f(k: K) { match k { #[cfg(test)] K::T { field: _ } | K::U => env_flag("ADK_TEST_HIDDEN", false), _ => std::env::var("AGENTDESK_PROD_LIST").is_ok() } }',
            'fn f() { #[cfg(test)] for p in tests::probes() {} std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] while tests::bypass() {} std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] loop { break; } std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] K { field: 0 }.probe(env_flag("ADK_TEST_HIDDEN", false))?; std::env::var("AGENTDESK_PROD_LIST"); }',
            'fn f() { #[cfg(test)] K { field: 0 } + env_flag("ADK_TEST_HIDDEN", false); std::env::var("AGENTDESK_PROD_LIST"); }',
        ):
            with self.subTest(source=source):
                self.write("src/list.rs", source)
                output = self.render()
                self.assertIn("AGENTDESK_PROD_LIST", output)
                self.assertNotIn("ADK_TEST_HIDDEN", output)

    def test_external_helper_names_survive_parser_migration(self) -> None:
        self.write("src/external.rs", '''
            fn read() { env_flag("COKACDIR_DEBUG", false); env_flag("REQUIRE_WAKE_WORD", true); }
        ''')
        output = self.render()
        for name in ("COKACDIR_DEBUG", "REQUIRE_WAKE_WORD"):
            self.assertIn(f"| `{name}` |", output)
            self.assertIn("`src/external.rs`", self.row(output, name))

    def test_line_shift_in_source_files_leaves_output_unchanged(self) -> None:
        before = self.render()
        self.assertEqual(
            self.row(before, "AGENTDESK_ALPHA"),
            "| `AGENTDESK_ALPHA` | `src/config/alpha.rs`, `src/reader.rs` "
            "| `AGENTDESK_ALPHA` picks the alpha mode. |",
        )
        self.assertIsNone(re.search(r"\.rs:\d", before))
        self.write("src/config/alpha.rs", "\n" + self.DEFINING)
        self.write("src/reader.rs", "\n\n" + self.READER)
        self.assertEqual(self.render(), before)

    def test_variable_add_and_remove_change_output(self) -> None:
        before = self.render()
        self.write("src/reader.rs", self.READER + 'fn gamma() { let _ = std::env::var("AGENTDESK_GAMMA"); }\n')
        added = self.render()
        self.assertNotEqual(added, before)
        self.assertIn("| `AGENTDESK_GAMMA` | `src/reader.rs` |", added)
        self.write("src/reader.rs", 'fn beta() { let _ = std::env::var("AGENTDESK_ALPHA"); }\n')
        removed = self.render()
        self.assertNotEqual(removed, before)
        self.assertNotIn("AGENTDESK_BETA", removed)

    def test_defining_file_move_changes_output(self) -> None:
        before = self.render()
        (self.root / "src/config/alpha.rs").unlink()
        self.write("src/settings/alpha.rs", self.DEFINING)
        moved = self.render()
        self.assertNotEqual(moved, before)
        self.assertIn("`src/settings/alpha.rs`", self.row(moved, "AGENTDESK_ALPHA"))
        self.assertNotIn("src/config/alpha.rs", moved)


class RepositoryTests(unittest.TestCase):
    def test_explicit_config_override_has_a_reference_description(self) -> None:
        variables = gen.collect_variables([REPO_ROOT / "src/config/config_path/selection.rs"])
        row = next(line for line in gen.render(variables).splitlines()
                   if line.startswith("| `AGENTDESK_CONFIG` |"))
        self.assertTrue(row.rsplit("|", 2)[1].strip(), row)

    def test_generated_doc_is_deterministic_and_covers_known_variables(self) -> None:
        variables = gen.collect_variables(gen.production_rust_files())
        first = gen.render(variables)
        second = gen.render(gen.collect_variables(gen.production_rust_files()))
        self.assertEqual(first, second)
        project = [name for name in variables if name.startswith(gen.PROJECT_PREFIXES)]
        self.assertIsNone(re.search(r"\.rs:\d", first))
        self.assertGreaterEqual(len(project), 80)
        for expected in (
            "AGENTDESK_ROOT_DIR",
            "AGENTDESK_API_URL",
            "AGENTDESK_TOKEN",
            "AGENTDESK_RELAY_CIRCUIT_STAMP",
            "RUST_LOG",
        ):
            self.assertIn(expected, variables, expected)
        # README used to document this one; nothing in src/ reads it.
        self.assertNotIn("AGENTDESK_SERVER_PORT", variables)
        for variable in variables.values():
            for site in variable.sites:
                self.assertFalse(site.path.startswith("target/"), site.path)
                self.assertNotIn("/tests/", site.path)


if __name__ == "__main__":
    unittest.main()
