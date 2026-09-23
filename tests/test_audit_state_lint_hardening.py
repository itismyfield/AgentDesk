from __future__ import annotations

import contextlib
import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


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


class ChangedPathAuditTests(unittest.TestCase):
    NAMES = ("child.rs", "한글.rs", "my module.rs", "a\nb.rs", 'a"b.rs')

    def setUp(self) -> None:
        AUDIT._TEST_REGION_CACHE.clear()

    @contextlib.contextmanager
    def repository(self, base_files: dict[str, str]):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)

            def git(*args: str) -> str:
                return subprocess.run(["git", *args], cwd=repo, check=True,
                                      capture_output=True, text=True).stdout.strip()

            git("init", "-q")
            git("config", "user.email", "audit@example.invalid")
            git("config", "user.name", "Audit Test")
            git("config", "diff.renames", "true")
            for path, text in base_files.items():
                (repo / path).parent.mkdir(parents=True, exist_ok=True)
                (repo / path).write_text(text, encoding="utf-8")
            git("add", "-A")
            git("commit", "-qm", "base")
            base = git("rev-parse", "HEAD")
            with contextlib.chdir(repo), mock.patch.dict(os.environ, {"AGENTDESK_AUDIT_BASE": base}):
                yield repo, git

    def assert_unwrap_findings(self, expected: list[str]) -> None:
        findings = AUDIT.audit_unwrap_panic(AUDIT.collect_added_lines())
        for location in expected:
            with self.subTest(location=location):
                self.assertTrue(any(finding.startswith(f"{location}: new production unwrap")
                                    for finding in findings), findings)
        self.assertEqual(len(findings), len(expected), findings)

    def write_git_quoted_children(self, repo: Path) -> None:
        for name in self.NAMES:
            (repo / "src/db" / name).write_text("fn f() {}\nfn g() { x.unwrap(); }\n", encoding="utf-8")

    def test_committed_git_quoted_paths_keep_their_added_lines(self) -> None:
        with self.repository({"src/db/base.rs": "fn base() {}\n"}) as (repo, git):
            self.write_git_quoted_children(repo)
            git("add", "-A")
            git("commit", "-qm", "candidate")
            self.assert_unwrap_findings([f"src/db/{name}:2" for name in self.NAMES])

    def test_untracked_git_quoted_paths_keep_their_added_lines(self) -> None:
        with self.repository({"src/db/base.rs": "fn base() {}\n"}) as (repo, _git):
            self.write_git_quoted_children(repo)
            self.assert_unwrap_findings([f"src/db/{name}:2" for name in self.NAMES])

    def test_renamed_path_reports_only_lines_added_after_the_rename(self) -> None:
        legacy = "".join(f"fn legacy_{index}() {{ x.unwrap(); }}\n" for index in range(20))
        with self.repository({"src/db/old name.rs": legacy}) as (repo, git):
            git("mv", "src/db/old name.rs", "src/db/new name.rs")
            with (repo / "src/db/new name.rs").open("a", encoding="utf-8") as handle:
                handle.write("fn added() { y.unwrap(); }\n")
            git("commit", "-qam", "candidate")
            self.assert_unwrap_findings(["src/db/new name.rs:21"])


if __name__ == "__main__":
    unittest.main()
