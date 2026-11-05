from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "audit_state_lint_hardening.py"

_SPEC = importlib.util.spec_from_file_location("audit_state_lint_hardening", SCRIPT_PATH)
AUDIT = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = AUDIT
_SPEC.loader.exec_module(AUDIT)


class TestRegionTests(unittest.TestCase):
    def classified_lines(self, source: str) -> set[int]:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.rs"
            path.write_text(source, encoding="utf-8")
            return AUDIT.test_region_lines(str(path))

    def test_cfg_test_attribute_marks_non_tests_module_as_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg(test)]\nmod postgres_tests {\n    probe().unwrap();\n}\n"
        )

        self.assertIn(3, lines)

    def test_cfg_all_test_attribute_marks_module_as_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg(all(test, unix))]\nmod platform_probes {\n    probe().unwrap();\n}\n"
        )

        self.assertIn(3, lines)

    def test_cfg_any_test_only_attribute_marks_module_as_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg(any(test))]\nmod alternate_probes {\n    probe().unwrap();\n}\n"
        )

        self.assertIn(3, lines)

    def test_nested_all_any_test_attribute_marks_module_as_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg(all(unix, any(test)))]\n"
            "mod nested_probes {\n    probe().unwrap();\n}\n"
        )

        self.assertIn(3, lines)

    def test_cfg_attr_with_effective_test_gate_marks_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg_attr(not(test), cfg(test))]\n"
            "mod conditional_probes {\n    probe().unwrap();\n}\n"
        )

        self.assertIn(3, lines)

    def test_cfg_attr_test_non_gate_remains_production_visible(self) -> None:
        lines = self.classified_lines(
            "#[cfg_attr(test, allow(dead_code))]\n"
            "mod conditional_lints {\n    production_probe().unwrap();\n}\n"
        )

        self.assertNotIn(3, lines)

    def test_not_test_predicates_remain_production_visible(self) -> None:
        for predicate in ("not(test)", "all(not(test), unix)"):
            with self.subTest(predicate=predicate):
                lines = self.classified_lines(
                    f"#[cfg({predicate})]\n"
                    "mod production_probes {\n    production_probe().unwrap();\n}\n"
                )

                self.assertNotIn(3, lines)

    def test_test_like_feature_string_remains_production_visible(self) -> None:
        lines = self.classified_lines(
            '#[cfg(feature = "test-tools")]\n'
            "mod feature_probes {\n    production_probe().unwrap();\n}\n"
        )

        self.assertNotIn(3, lines)

    def test_tests_module_name_remains_a_test_region(self) -> None:
        lines = self.classified_lines("mod tests {\n    probe().unwrap();\n}\n")

        self.assertIn(2, lines)

    def test_production_module_without_cfg_test_is_not_a_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg(test)] fn inline_test_helper() {}\n"
            "mod production {\n    production_probe().unwrap();\n}\n"
        )

        self.assertNotIn(3, lines)

    def test_non_code_braces_do_not_extend_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg(test)]\n"
            "mod postgres_tests {\n"
            "    let normal = \"{\";\n"
            "    let raw = r#\"}\"#;\n"
            "    let byte_raw = br#\"{\"#;\n"
            "    let character = '{';\n"
            "    // }\n"
            "    /* { */\n"
            "}\n"
            "fn production() {\n    production_probe().unwrap();\n}\n"
        )

        self.assertNotIn(11, lines)

    def test_single_line_test_module_does_not_extend_test_region(self) -> None:
        lines = self.classified_lines(
            "#[cfg(test)] mod inline_tests { fn probe() {} }\n"
            "fn production() {\n    production_probe().unwrap();\n}\n"
        )

        self.assertNotIn(3, lines)


class MigrationIntegerAuditTests(unittest.TestCase):
    def test_add_column_if_not_exists_integer_is_flagged(self) -> None:
        fixture = AUDIT.AddedLine(
            "migrations/postgres/0099_fixture.sql",
            7,
            "ALTER TABLE pr_tracking ADD COLUMN IF NOT EXISTS retry_count INTEGER NOT NULL DEFAULT 0;",
        )

        findings = AUDIT.audit_migration_integers([fixture])

        self.assertEqual(len(findings), 1)
        self.assertIn("retry_count INTEGER", findings[0])
        self.assertIn("use BIGINT", findings[0])


class SqlVersionDependentEscapeAuditTests(unittest.TestCase):
    CLEAN_RUST = 'fn noop() {}\n'
    CLEAN_SQL = "SELECT 1;\n"

    def audit(self, rust: str, sql: str, sql_name: str = "0999_fixture.sql") -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "fixture.rs").write_text(rust, encoding="utf-8")
            (root / "migrations" / "postgres").mkdir(parents=True)
            (root / "migrations" / "postgres" / sql_name).write_text(sql, encoding="utf-8")
            findings, scanned = AUDIT.audit_sql_version_dependent_escapes(root)
        self.assertEqual(scanned, 2)
        return findings

    def test_vertical_tab_escape_in_e_string_fails_until_written_as_hex(self) -> None:
        cases = [
            ("rust string", 'fn q() {\n    query("SELECT BTRIM(k, E\' \\t\\\\f\\\\v\')");\n}\n', None, "src/fixture.rs:2"),
            ("rust raw string", 'const Q: &str = r#"\nWHERE k = E\'\\v\'"#;\n', None, "src/fixture.rs:2"),
            ("rust hex backslash", 'const Q: &str = "e\'\\x5cv\'";\n', None, "src/fixture.rs:1"),
            ("migration", None, "-- trims\nSELECT btrim(k, E' \\t\\v');\n", "0999_fixture.sql:2"),
            ("dollar-quoted body", None, "DO $$ BEGIN PERFORM E'\\v'; END $$;\n", "0999_fixture.sql:1"),
        ]
        for name, rust, sql, location in cases:
            with self.subTest(name):
                findings = self.audit(rust or self.CLEAN_RUST, sql or self.CLEAN_SQL)
                self.assertEqual(len(findings), 1, findings)
                self.assertIn(location, findings[0])
                fixed_rust = (rust or self.CLEAN_RUST).replace("\\v", "\\x0b").replace("\\x5cv", "\\x0b")
                fixed_sql = (sql or self.CLEAN_SQL).replace("\\v", "\\x0b")
                self.assertEqual(self.audit(fixed_rust, fixed_sql), [])

    def test_vertical_tab_outside_e_string_literals_is_not_flagged(self) -> None:
        cases = [
            ("rust comments", '// query("E\'\\\\v\'")\n/// "E\'\\\\v\'"\n/* "E\'\\\\v\'" */\nfn noop() {}\n', self.CLEAN_SQL),
            ("sql comment in rust string", 'const Q: &str = "-- E\'\\\\v\'\\nSELECT 1";\n', self.CLEAN_SQL),
            ("non-sql rust string", 'const P: &str = "C:\\\\vendor";\nconst T: &str = "ONE\'\\\\v\'";\n', self.CLEAN_SQL),
            ("escaped backslash", 'const Q: &str = "E\'\\\\\\\\v\'";\n', self.CLEAN_SQL),
            ("sql comments", self.CLEAN_RUST, "-- E'\\v'\n/* E'\\v' /* nested */ */\nSELECT 1;\n"),
            ("standard string", self.CLEAN_RUST, "SELECT '\\v', \"E'\\v\";\n"),
        ]
        for name, rust, sql in cases:
            with self.subTest(name):
                self.assertEqual(self.audit(rust, sql), [])

    def test_only_the_immutable_deploy_gate_migration_is_exempt(self) -> None:
        sql = "SELECT btrim(k, E'\\v');\n"
        self.assertEqual(self.audit(self.CLEAN_RUST, sql, "0100_block_untrusted_deploy_gate.sql"), [])
        self.assertEqual(len(self.audit(self.CLEAN_RUST, sql, "0186_fixture.sql")), 1)

    def test_current_tree_scans_runtime_sql_and_migrations_without_findings(self) -> None:
        findings, scanned = AUDIT.audit_sql_version_dependent_escapes(REPO_ROOT)
        rust, migrations = AUDIT.sql_escape_targets(REPO_ROOT)

        self.assertEqual(findings, [])
        self.assertIn(REPO_ROOT / "src" / "dispatch" / "dispatch_status.rs", rust)
        self.assertIn(
            REPO_ROOT / "migrations" / "postgres" / "0100_block_untrusted_deploy_gate.sql",
            migrations,
        )
        self.assertEqual(scanned, len(rust) + len(migrations))

    def test_empty_root_fails_instead_of_passing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            findings, scanned = AUDIT.audit_sql_version_dependent_escapes(Path(directory))

        self.assertEqual(scanned, 0)
        self.assertEqual(len(findings), 1)


if __name__ == "__main__":
    unittest.main()
