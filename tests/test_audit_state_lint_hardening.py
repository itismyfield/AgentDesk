from __future__ import annotations

import importlib.util
import os
import subprocess
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


KEPT_GUARD_ROW = "agentdesk kept::tests::kept_source_guard | include_str | fixture reason\n"
ALLOWLIST_HEADER = "# kept guards\n# <crate> <module path>::<test> | <tool> | <reason>\n"


class AuditRepoCase(unittest.TestCase):
    """Run the real script on a throwaway repository whose HEAD adds the given files."""

    BASE_FILES = {
        "src/lib.rs": "pub fn production() {}\n",
        "src/kept_tests.rs": "#[test]\nfn kept_source_guard() {\n    let _ = 1;\n}\n",
        "scripts/ci/source_text_guard_allowlist.txt": ALLOWLIST_HEADER + KEPT_GUARD_ROW,
        "migrations/postgres/0001_fixture.sql": "SELECT 1;\n",
    }

    def git(self, root: Path, *args: str) -> str:
        return subprocess.run(
            ["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", *args],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def write(self, root: Path, files: dict[str, str]) -> None:
        for name, text in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")

    def run_audit(
        self, added: dict[str, str], *, base: bool = True, base_files: dict[str, str] | None = None
    ) -> tuple[int, str]:
        """``base=False`` is a one-commit clone: no main, no origin/main and no HEAD^."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.git(root, "init", "-q", "-b", "work")
            self.write(root, {**self.BASE_FILES, **(base_files or {})})
            if base:
                self.git(root, "add", "-A")
                self.git(root, "commit", "-q", "-m", "base")
                base_sha = self.git(root, "rev-parse", "HEAD")
            self.write(root, added)
            self.git(root, "add", "-A")
            self.git(root, "commit", "-q", "-m", "change")
            env = {key: value for key, value in os.environ.items() if key != "AGENTDESK_AUDIT_BASE"}
            if base:
                env["AGENTDESK_AUDIT_BASE"] = base_sha
            result = subprocess.run(
                [sys.executable, str(SCRIPT_PATH)], cwd=root, env=env, capture_output=True, text=True
            )
        return result.returncode, result.stdout + result.stderr

    def assert_flagged(self, added: dict[str, str], needle: str) -> None:
        rc, output = self.run_audit(added)
        self.assertEqual(rc, 1, output)
        self.assertIn(needle, output)

    def assert_passes(self, added: dict[str, str]) -> None:
        rc, output = self.run_audit(added)
        self.assertEqual(rc, 0, output)


class MutantHookAuditTests(AuditRepoCase):
    def test_mutant_env_in_production_code_fails(self) -> None:
        self.assert_flagged(
            {"src/lib.rs": 'pub fn production() {\n    let _ = std::env::var("ADK_NEW_MUTANT");\n}\n'},
            "src/lib.rs:2: new mutant env `ADK_NEW_MUTANT`",
        )

    def test_mutant_hook_fn_in_a_test_module_file_fails(self) -> None:
        # The generic form has no `mutant(` call text, so only rule B can catch it.
        for signature in ("fn mutant(name: &str)", "fn mutant<T: AsRef<str>>(name: T)"):
            with self.subTest(signature):
                self.assert_flagged(
                    {"src/owner/tests/mod.rs": f"pub(crate) {signature} -> bool {{\n    false\n}}\n"},
                    "src/owner/tests/mod.rs:1: new mutant hook fn `mutant`",
                )

    def test_env_rule_compares_whole_literal_values(self) -> None:
        prose = 'pub const HELP: &str =\n    r#"The switch "ADK_NEW_MUTANT" is no longer used."#;\n'
        self.assert_passes({"src/help.rs": prose})
        for literal in ('r#"ADK_NEW_MUTANT"#', '"ADK_NEW_MUTANT"'):
            with self.subTest(literal):
                self.assert_flagged(
                    {"src/help.rs": f"pub const KEY: &str = {literal};\n"},
                    "src/help.rs:1: new mutant env `ADK_NEW_MUTANT`",
                )

    def test_comments_naming_a_mutant_pass(self) -> None:
        self.assert_passes(
            {
                "src/lib.rs": (
                    "// ADK_OLD_MUTANT was removed with its mutant(\"x\") calls\n"
                    "/* \"ADK_OLD_MUTANT\" */\n"
                    "/// fn mutant(name: &str) is gone\n"
                    "pub fn production() {}\n"
                )
            }
        )

    def test_production_fn_ending_in_mutation_passes(self) -> None:
        self.assert_passes(
            {"src/lib.rs": "pub fn production() {}\npub fn begin_sink_delivery_mutation(seq: u64) -> u64 {\n    seq\n}\n"}
        )

    def test_reformatting_an_existing_hook_passes_but_a_second_read_fails(self) -> None:
        existing = {"src/lib.rs": 'pub fn production() {\n    let _ = std::env::var("ADK_OLD_MUTANT");\n}\n'}
        reformatted = 'pub fn production() {\n    let _old = std::env::var("ADK_OLD_MUTANT");\n}\n'
        rc, output = self.run_audit({"src/lib.rs": reformatted}, base_files=existing)
        self.assertEqual(rc, 0, output)
        rc, output = self.run_audit(
            {"src/lib.rs": reformatted[: -len("}\n")] + '    let _ = std::env::var("ADK_OLD_MUTANT");\n}\n'},
            base_files=existing,
        )
        self.assertEqual(rc, 1, output)
        self.assertIn("new mutant env `ADK_OLD_MUTANT`", output)

    def test_deleted_comments_and_strings_do_not_offset_a_new_hook(self) -> None:
        cases = {
            "env in a block comment": (
                '/*\nUse "ADK_NEW_MUTANT" for the experiment.\n*/\npub fn production() {}\n',
                'pub fn production() {\n    let _ = std::env::var("ADK_NEW_MUTANT");\n}\n',
                "new mutant env `ADK_NEW_MUTANT`",
            ),
            "fn in a block comment": (
                "/*\nfn mutant(name: &str) -> bool {\n*/\npub fn production() {}\n",
                "pub fn production() {}\nfn mutant(name: &str) -> bool {\n    false\n}\n",
                "new mutant hook fn `mutant`",
            ),
        }
        for name, (before, after, needle) in cases.items():
            with self.subTest(name):
                rc, output = self.run_audit({"src/lib.rs": after}, base_files={"src/lib.rs": before})
                self.assertEqual(rc, 1, output)
                self.assertIn(needle, output)

    def test_missing_diff_base_fails_instead_of_passing(self) -> None:
        rc, output = self.run_audit({"src/next.rs": "pub fn next() {}\n"}, base=False)
        self.assertNotEqual(rc, 0, output)
        self.assertIn("no diff base", output)


class SourceTextGuardAuditTests(AuditRepoCase):
    def test_a_new_test_reading_rust_source_fails(self) -> None:
        reads = {
            "include_str": '    let text = include_str!("lib.rs");\n',
            "multi-line include_str": '    let text = include_str!(\n        "lib.rs"\n    );\n',
            "lexer": "    let text = crate::test_support::strip_rust_comments(TEXT);\n",
            "manifest-rooted tree walk": (
                '    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));\n'
                '    let text = std::fs::read_dir(root.join("src"));\n'
            ),
            "cwd-relative read": '    let text = std::fs::read_to_string("src/lib.rs");\n',
        }
        for name, body in reads.items():
            with self.subTest(name):
                self.assert_flagged(
                    {"src/owner/reader_tests.rs": f"#[test]\nfn new_guard() {{\n{body}}}\n"},
                    "in test `new_guard`",
                )

    def test_retargeting_an_existing_include_to_a_rust_file_fails(self) -> None:
        before = '#[test]\nfn data() {\n    let text = include_str!(\n        "data.txt"\n    );\n}\n'
        rc, output = self.run_audit(
            {"src/owner/data_tests.rs": before.replace("data.txt", "../lib.rs")},
            base_files={"src/owner/data_tests.rs": before},
        )
        self.assertEqual(rc, 1, output)
        self.assertIn("src/owner/data_tests.rs:4: include_str! of a .rs file in test `data`", output)

    def test_a_helper_reading_source_outside_a_kept_guard_file_fails(self) -> None:
        self.assert_flagged(
            {"src/owner/reader_tests.rs": 'const TEXT: &str = include_str!("../lib.rs");\n'},
            "src/owner/reader_tests.rs:1: include_str! of a .rs file in code outside",
        )

    def test_the_listed_guard_and_its_file_helpers_may_read_source(self) -> None:
        kept = self.BASE_FILES["src/kept_tests.rs"].replace(
            "    let _ = 1;\n", '    let _ = include_str!("lib.rs");\n'
        )
        self.assert_passes(
            {"src/kept_tests.rs": kept + 'fn helper() -> &\'static str {\n    include_str!("lib.rs")\n}\n'}
        )
        self.assert_flagged(
            {"src/kept_tests.rs": kept + '#[test]\nfn other_guard() {\n    let _ = include_str!("lib.rs");\n}\n'},
            "in test `other_guard`",
        )

    def test_ordinary_tests_and_fixtures_pass(self) -> None:
        self.assert_passes(
            {
                "src/owner/plain_tests.rs": (
                    "// include_str!(\"lib.rs\") is not used here\n"
                    "#[test]\nfn plain() {\n"
                    '    let fixture = include_str!("../../tests/fixtures/upstream.rs");\n'
                    "    let dir = tempfile::tempdir().unwrap();\n"
                    '    let _ = (fixture, dir.path().join("src"), dir.path().join("src/x.rs"));\n'
                    '    let paths = serde_json::json!({"allowed_write_paths": ["src"]});\n'
                    '    assert_eq!(paths.to_string(), "src/x.rs");\n'
                    "}\n"
                ),
                "src/owner/prod.rs": (
                    "pub fn root() -> std::path::PathBuf {\n"
                    '    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")\n}\n'
                ),
            }
        )

    def test_a_row_naming_no_fn_fails(self) -> None:
        stale = "agentdesk gone::tests::removed_guard | direct | fixture reason\n"
        self.assert_flagged(
            {"scripts/ci/source_text_guard_allowlist.txt": ALLOWLIST_HEADER + KEPT_GUARD_ROW + stale},
            "`removed_guard` names no fn",
        )


class SourceTextGuardAllowListFormatTests(unittest.TestCase):
    def findings(self, text: str) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "allowlist.txt"
            path.write_text(text, encoding="utf-8")
            return AUDIT.load_source_guard_allowlist(path)[1]

    def test_well_formed_rows_pass(self) -> None:
        self.assertEqual(self.findings(ALLOWLIST_HEADER + KEPT_GUARD_ROW), [])

    def test_malformed_files_fail(self) -> None:
        cases = {
            "missing header": (KEPT_GUARD_ROW, "header comment"),
            "duplicate row": (ALLOWLIST_HEADER + KEPT_GUARD_ROW * 2, "listed twice"),
            "empty reason": (ALLOWLIST_HEADER + "agentdesk a::b | lexer |  \n", "needs a reason"),
            "unknown tool": (ALLOWLIST_HEADER + "agentdesk a::b | grep | why\n", "tool `grep`"),
            "comment after header": (ALLOWLIST_HEADER + "# late\n" + KEPT_GUARD_ROW, "expected `<crate>"),
            "blank row": (ALLOWLIST_HEADER + "\n" + KEPT_GUARD_ROW, "expected `<crate>"),
        }
        for name, (text, needle) in cases.items():
            with self.subTest(name):
                findings = self.findings(text)
                self.assertTrue(any(needle in finding for finding in findings), findings)

    def test_missing_file_fails(self) -> None:
        names, findings = AUDIT.load_source_guard_allowlist(Path("/nonexistent/allowlist.txt"))
        self.assertEqual(names, set())
        self.assertIn("missing", findings[0])


class MutantHookAllowListTests(unittest.TestCase):
    def test_allowed_pair_passes_and_other_paths_still_fail(self) -> None:
        line = '    if std::env::var("ADK_SEAM_MUTANT").is_ok() {'
        allowed = AUDIT.MUTANT_HOOK_ALLOWED
        AUDIT.MUTANT_HOOK_ALLOWED = frozenset({("src/seam.rs", "ADK_SEAM_MUTANT")})
        try:
            kept = AUDIT.audit_mutant_hooks([AUDIT.AddedLine("src/seam.rs", 1, line)], True)
            other = AUDIT.audit_mutant_hooks([AUDIT.AddedLine("src/other.rs", 1, line)], True)
        finally:
            AUDIT.MUTANT_HOOK_ALLOWED = allowed
        self.assertEqual(kept, [])
        self.assertEqual(len(other), 1, other)
        self.assertIn("ADK_SEAM_MUTANT", other[0])



class MutantHookOldSnapshotTests(unittest.TestCase):
    def test_removed_line_without_a_readable_old_file_offsets_nothing(self) -> None:
        line = '    let _ = std::env::var("ADK_OLD_MUTANT");'
        added = [AUDIT.AddedLine("src/missing_fixture.rs", 1, line)]
        for rev in (None, "0" * 40):
            with self.subTest(rev=rev):
                removed = [AUDIT.RemovedLine("src/missing_fixture.rs", 1, line, rev)]
                findings = AUDIT.audit_mutant_hooks(added, True, removed)
                self.assertEqual(len(findings), 1, findings)
                self.assertIn("ADK_OLD_MUTANT", findings[0])


if __name__ == "__main__":
    unittest.main()
