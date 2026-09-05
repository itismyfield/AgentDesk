from pathlib import Path
import sys
import textwrap
import unittest

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


class RepositoryTests(unittest.TestCase):
    def test_generated_doc_is_deterministic_and_covers_known_variables(self) -> None:
        variables = gen.collect_variables(gen.production_rust_files())
        first = gen.render(variables)
        second = gen.render(gen.collect_variables(gen.production_rust_files()))
        self.assertEqual(first, second)
        project = [name for name in variables if name.startswith(gen.PROJECT_PREFIXES)]
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
