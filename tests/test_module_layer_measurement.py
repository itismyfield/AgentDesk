"""Conservative production coverage for ambiguous cfg ranges and missing module paths."""
from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
import subprocess
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
            "let closure = |#[cfg(test)] arg: [u8; {1}]| {\ncrate::prod::keep();\n};",
            "let closure = |#[cfg(test)] arg: crate::hidden::Wrapper<{1}>| {\ncrate::prod::keep();\n};",
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

    def test_closure_parameter_offsets_survive_preceding_line_comments(self):
        contexts = (
            ("// harmless standalone comment\n", "", "#[cfg(test)]"),
            ("const BEFORE: () = (); // harmless trailing comment\n", "", "#[cfg(test)]"),
            ("", "let _before = (); // harmless local trailing comment\n", "#[cfg(test)]"),
            ("", "", "#[cfg(test)] // harmless attribute trailing comment\n"),
        )
        for prefix, local, gate in contexts:
            with self.subTest(context=(prefix, local, gate)), TemporaryDirectory() as temporary:
                source = Path(temporary) / "consumer.rs"
                source.write_text(prefix + "fn run() {\n" + local
                                  + "let closure = |" + gate + " arg: crate::hidden::Hide| {\n"
                                  + "crate::prod::keep();\n};\n}", encoding="utf-8")
                uncertainty = []
                values = [token.value for token in production_tokens(
                    source, unreadable=uncertainty, diagnostic_path="src/consumer.rs")]
                self.assertIn("prod", values)
                self.assertNotIn("hidden", values)
                self.assertTrue(uncertainty)

    def test_test_only_parameter_types_do_not_become_production_references(self):
        types = ("[u8; {crate::param_only::N}]", "Pair<{1}, crate::param_only::Marker>",
                 "TypeMacro!({crate::param_only::N})", "TypeMacro!{crate::param_only::N}",
                 "TypeMacro![crate::param_only::N]", "Outer<Pair<{1}, crate::param_only::Marker>>",
                 "[u8; {match 1 { 1 | 2 => crate::param_only::N, _ => 1 }}]",
                 "TypeMacro!{1 | 2 => crate::param_only::N}",
                 "Outer<fn() -> crate::param_only::Marker>")
        for parameter in types:
            with self.subTest(parameter=parameter), TemporaryDirectory() as temporary:
                source = Path(temporary) / "consumer.rs"
                source.write_text("fn run() {\nlet _f = |#[cfg(test)] _x: " + parameter
                                  + "| {\ncrate::prod::keep();\n};\n}", encoding="utf-8")
                uncertainty = []
                values = [token.value for token in production_tokens(source, unreadable=uncertainty)]
                self.assertIn("prod", values)
                self.assertNotIn("param_only", values)
                self.assertTrue(uncertainty)

    def test_following_production_parameter_survives_uncertain_header_masking(self):
        types = ("crate::param_only::Marker", "Pair<{1}, crate::param_only::Marker>", "\nfn()",
                 "Outer<fn()->crate::param_only::Marker>",
                 "\nPair<{1}, crate::param_only::Marker>", "\nTypeMacro!{crate::param_only::N}")
        for parameter in types:
            with self.subTest(parameter=parameter), TemporaryDirectory() as temporary:
                source = Path(temporary) / "consumer.rs"
                source.write_text("fn run() {\nlet _first = |#[cfg(test)] hidden: " + parameter
                                  + ", visible: crate::visible_only::Marker| {\ncrate::prod::keep();\n};\n"
                                  + "let _second = |#[cfg(test)] other: ()| {\ncrate::prod::keep();\n};\n}",
                                  encoding="utf-8")
                uncertainty = []
                values = [token.value for token in production_tokens(source, unreadable=uncertainty)]
                self.assertIn("prod", values)
                self.assertIn("visible_only", values)
                self.assertNotIn("param_only", values)
                self.assertTrue(uncertainty)

    def test_production_const_type_does_not_hide_a_later_test_parameter_boundary(self):
        for visible in ("[u8; {crate::visible_only::N}]", "Wrapper<{crate::visible_only::N}>"):
            for unknown in (False, True):
                with self.subTest(visible=visible, other_uncertainty=unknown), TemporaryDirectory() as temporary:
                    source = Path(temporary) / "consumer.rs"
                    prefix = "struct Record {\n#[cfg(test)] hidden: Pair<u8, u16>,\n}\n" if unknown else ""
                    source.write_text(prefix + "fn run() {\nlet _f = |#[cfg(test)] first: (), visible: " + visible
                                      + ", #[cfg(test)] last: ()| {\ncrate::prod::keep();\n};\n}", encoding="utf-8")
                    uncertainty = []
                    values = [token.value for token in production_tokens(source, unreadable=uncertainty)]
                    self.assertIn("visible_only", values)
                    self.assertIn("prod", values)
                    self.assertTrue(uncertainty)
                    if unknown:
                        self.assertTrue(any("generic field boundary" in row for row in uncertainty))

    def test_test_only_match_arms_following_or_patterns_are_not_parameters(self):
        cases = (
            ("enum V { A, B, T, U }", "V::A | V::B if true", "V::T"),
            ("enum V { A(u8), B(u8), T(u8), U(u8) }", "V::A(_) | V::B(_)", "V::T(_)"),
            ("enum V { A, B, T, U }", "| V::A", "| V::T | V::U"),
        )
        for declaration, normal, test_only in cases:
            with self.subTest(pattern=(normal, test_only)), TemporaryDirectory() as temporary:
                source = Path(temporary) / "consumer.rs"
                source.write_text(declaration + "\nfn run(value: V) {\nmatch value {\n" + normal
                                  + " => (),\n#[cfg(test)]\n" + test_only + " => crate::param_only::keep(),\n"
                                  + "_ => crate::prod::keep(),\n}\n}", encoding="utf-8")
                uncertainty = []
                values = [token.value for token in production_tokens(source, unreadable=uncertainty)]
                self.assertIn("prod", values)
                self.assertNotIn("param_only", values)
                self.assertEqual(uncertainty, [])

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

    @staticmethod
    def _source(body="crate::upper::call();", *, prefix="", local="", gate="#[cfg(test)]", parameter="()"):
        return (prefix + "mod lower {\nstruct Wrapper<const N: usize>;\npub fn run() {\n" + local
                + "let _f = |" + gate + " _x: " + parameter + "| {\n" + body
                + "\n};\n}\n}\nmod upper { pub const N: usize = 1; pub struct Marker; pub fn call() {} }\n")

    @staticmethod
    def _tree(source):
        from tests.test_audit_maintainability import _FakeSrcTree
        return _FakeSrcTree({
            "src/lib.rs": source,
            "scripts/audit_maintainability/layers.toml": """
                schema_version=1
                [layers]
                foundation=1
                assembly=5
                [[rules]]
                prefix=""
                layer="assembly"
                reason="fixture assembly"
                [[rules]]
                prefix="lower"
                layer="foundation"
                reason="fixture lower layer"
                [[rules]]
                prefix="upper"
                layer="assembly"
                reason="fixture upper layer"
            """,
            "scripts/audit_maintainability/layer_moves.toml": "schema_version=1\nmoves=[]\n",
            "scripts/audit_allowlist.toml": "",
        })

    @staticmethod
    def _snapshot(root):
        from audit_maintainability.checks import module_layer_baseline as provider, module_layers
        occurrences = []
        graph = module_layers.analyze(root, occurrence_projection=occurrences)
        return provider.baseline(graph, provider.load_mapping(root), "fixture", occurrences)

    @staticmethod
    def _counts(snapshot):
        return {section: {row["id"]: row["count"] for row in snapshot[section]}
                for section in ("reverse_dependencies", "indeterminate_dependencies", "uncertainties")}

    @staticmethod
    def _git(root, *args):
        return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, check=True).stdout.strip()

    def _generate_initial_baseline(self, root):
        from audit_maintainability.checks import module_layer_baseline as provider
        self._git(root, "init", "-q")
        self._git(root, "config", "user.name", "Fixture")
        self._git(root, "config", "user.email", "fixture@example.invalid")
        self._git(root, "add", "src", "scripts")
        self._git(root, "commit", "-qm", "fixture baseline input")
        result = subprocess.run([sys.executable, str(Path(provider.__file__).resolve()), "--repo", str(root),
                                 "--input-sha", "HEAD", "--output", str(root / provider.BASELINE)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    @staticmethod
    def _runner():
        from tests.test_audit_maintainability import HARNESS
        import audit_maintainability.common as common
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            result = HARNESS.main(["--check", "--format", "json", "--allowlist",
                                   str(common.REPO_ROOT / "scripts/audit_allowlist.toml")])
        return result, json.loads(stdout.getvalue()), stderr.getvalue()

    def test_comment_forms_preserve_reverse_and_uncertainty_identifier_counts(self):
        contexts = ({}, {"prefix": "// standalone comment\n"},
                    {"prefix": "const BEFORE: () = (); // trailing comment\n"},
                    {"local": "let _before = (); // local trailing comment\n"},
                    {"gate": "#[cfg(test)] // attribute trailing comment\n"})
        snapshots = []
        for context in contexts:
            with self.subTest(context=context), self._tree(self._source(**context)) as root:
                snapshot = self._snapshot(root)
                self.assertEqual(len(snapshot["reverse_dependencies"]), 1)
                self.assertTrue(snapshot["uncertainties"])
                snapshots.append(self._counts(snapshot))
        self.assertTrue(all(snapshot == snapshots[0] for snapshot in snapshots))

    def test_runner_blocks_new_body_dependency_with_comments_or_const_parameter_types(self):
        contexts = ({}, {"prefix": "// standalone comment\n"},
                    {"prefix": "const BEFORE: () = (); // trailing comment\n"},
                    {"local": "let _before = (); // local trailing comment\n"},
                    {"parameter": "[u8; {1}]"}, {"parameter": "Wrapper<{1}>"})
        for context in contexts:
            with self.subTest(context=context), self._tree(self._source("", **context)) as root:
                self._generate_initial_baseline(root)
                self.assertEqual(self._runner()[0], 0)
                (root / "src/lib.rs").write_text(self._source(**context), encoding="utf-8")
                self._git(root, "commit", "-qam", "add production dependency")
                result, report, stderr = self._runner()
                self.assertEqual(result, 1, stderr)
                self.assertTrue(any(row["severity"] == "error"
                                    for row in report["checks"]["module_layers"]["findings"]))
                self.assertEqual(len(report["checks"]), 15)
                self.assertFalse(any(row["severity"] == "error" for key, check in report["checks"].items()
                                     if key != "module_layers" for row in check["findings"]))

    def test_runner_allows_reference_changes_inside_test_only_const_parameter_types(self):
        types = (("[u8; {1}]", "[u8; {crate::upper::N}]"),
                 ("Pair<{1}, ()>", "Pair<{1}, crate::upper::Marker>"),
                 ("TypeMacro!({1})", "TypeMacro!({crate::upper::N})"),
                 ("TypeMacro!{1}", "TypeMacro!{crate::upper::N}"),
                 ("TypeMacro![1]", "TypeMacro![crate::upper::N]"))
        for before, after in types:
            with self.subTest(parameter=after), self._tree(self._source("", parameter=before)) as root:
                self._generate_initial_baseline(root)
                original = self._counts(self._snapshot(root))
                self.assertEqual(self._runner()[0], 0)
                (root / "src/lib.rs").write_text(self._source("", parameter=after), encoding="utf-8")
                self._git(root, "commit", "-qam", "change test-only type reference")
                self.assertEqual(self._counts(self._snapshot(root)), original)
                self.assertEqual(self._runner()[0], 0)

    def test_runner_allows_comment_only_removal_from_existing_body_dependency(self):
        contexts = (
            ({"prefix": "// standalone comment\n"}, {}),
            ({"prefix": "const BEFORE: () = (); // trailing comment\n"},
             {"prefix": "const BEFORE: () = ();\n"}),
            ({"local": "let _before = (); // local trailing comment\n"},
             {"local": "let _before = ();\n"}),
            ({"gate": "#[cfg(test)] // attribute trailing comment\n"},
             {"gate": "#[cfg(test)]\n"}),
        )
        for before, after in contexts:
            with self.subTest(context=before), self._tree(self._source(**before)) as root:
                self._generate_initial_baseline(root)
                original = self._counts(self._snapshot(root))
                self.assertEqual(self._runner()[0], 0)
                (root / "src/lib.rs").write_text(self._source(**after), encoding="utf-8")
                self._git(root, "commit", "-qam", "remove comment only")
                self.assertEqual(self._counts(self._snapshot(root)), original)
                self.assertEqual(self._runner()[0], 0)

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
