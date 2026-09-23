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


if __name__ == "__main__":
    unittest.main()
