"""Conservative production coverage for ambiguous cfg ranges and missing module paths."""
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from ci.rust_test_sources import production_tokens, walk


class ModuleLayerMeasurementTests(unittest.TestCase):
    def test_closure_test_parameter_keeps_production_body_and_reports_uncertainty(self):
        bodies = (
            "let closure = |#[cfg(test)] arg: crate::hidden::Hide| {\ncrate::prod::keep();\n};",
            "let closure = |\n#[cfg(test)] arg: crate::hidden::Hide\n| {\ncrate::prod::keep();\n};",
            "let closure = |keep: usize, #[cfg(test)] arg: crate::hidden::Hide| {\ncrate::prod::keep();\n};",
            "let closure = |#[cfg(test)] arg| {\ncrate::prod::keep();\n};",
            "let closure = |#[cfg(test)] arg: usize| { #[cfg(test)] let only_test = 0;\ncrate::prod::keep();\n};",
            *(f"let closure = |#[cfg(test)] r#{name}: crate::hidden::Hide| {{\ncrate::prod::keep();\n}};"
              for name in ("type", "async", "let")),
        )
        for body in bodies:
            with self.subTest(body=body), TemporaryDirectory() as temporary:
                source = Path(temporary) / "consumer.rs"
                source.write_text("fn run() {\n" + body + "\n}", encoding="utf-8")
                uncertainty = []
                tokens = production_tokens(source, unreadable=uncertainty, diagnostic_path="src/consumer.rs")
                values = [token.value for token in tokens]
                self.assertIn("prod", values)
                self.assertNotIn("hidden", values)
                self.assertTrue(uncertainty)
                self.assertTrue(all(row.startswith("src/consumer.rs:") for row in uncertainty))

    def test_test_only_closure_statement_stays_excluded_without_uncertainty(self):
        with TemporaryDirectory() as temporary:
            source = Path(temporary) / "consumer.rs"
            source.write_text("""fn run() {
                #[cfg(test)] let hidden = |arg: crate::hidden::Hide| {
                    crate::hidden::keep();
                };
                crate::prod::keep();
            }""", encoding="utf-8")
            uncertainty = []
            tokens = production_tokens(source, unreadable=uncertainty)
            values = [token.value for token in tokens]
            self.assertIn("prod", values)
            self.assertNotIn("hidden", values)
            self.assertEqual(uncertainty, [])

    def test_test_only_statement_closures_are_not_parameter_uncertainties(self):
        bodies = (
            "if let Some(value) = maybe.with(|cell| cell.get()) {\ncrate::hidden::keep();\n}",
            "invoke(|value| {\ncrate::hidden::keep();\n});",
            "hidden_macro!(|value| {\ncrate::hidden::keep();\n});",
        )
        for body in bodies:
            with self.subTest(body=body), TemporaryDirectory() as temporary:
                source = Path(temporary) / "consumer.rs"
                source.write_text("fn run() {\n#[cfg(test)] " + body + "\ncrate::prod::keep();\n}", encoding="utf-8")
                uncertainty = []
                tokens = production_tokens(source, unreadable=uncertainty)
                values = [token.value for token in tokens]
                self.assertIn("prod", values)
                self.assertNotIn("hidden", values)
                self.assertEqual(uncertainty, [])

    def test_missing_outside_module_is_a_coverage_error_for_both_walks(self):
        with TemporaryDirectory() as temporary:
            repo = Path(temporary) / "repo"
            source = repo / "src/lib.rs"
            source.parent.mkdir(parents=True)
            outside = Path(temporary) / "absent/external.rs"
            source.write_text(f'#[path="{outside.as_posix()}"] mod external;\n', encoding="utf-8")
            for production in (False, True):
                with self.subTest(production=production):
                    inventory = walk(source, repo, production=production)
                    self.assertEqual(len(inventory.errors), 1)
                    self.assertIn("src/lib.rs:1 mod external: no file at", inventory.errors[0])
                    self.assertIn("external.rs", inventory.errors[0])
                    self.assertNotIn(("external",), inventory.logical_modules)


if __name__ == "__main__":
    unittest.main()
