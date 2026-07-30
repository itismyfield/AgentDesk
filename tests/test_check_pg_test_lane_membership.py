"""Mutation fixtures for the PostgreSQL test-lane membership gate (#4979)."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/check_pg_test_lane_membership.py"
INTEGRITY_SCRIPT = REPO_ROOT / "scripts/check_test_target_integrity.py"
_spec = importlib.util.spec_from_file_location("check_pg_test_lane_membership", SCRIPT)
assert _spec and _spec.loader
membership = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = membership
_spec.loader.exec_module(membership)
_integrity_spec = importlib.util.spec_from_file_location(
    "check_test_target_integrity_cross_fixture", INTEGRITY_SCRIPT
)
assert _integrity_spec and _integrity_spec.loader
integrity = importlib.util.module_from_spec(_integrity_spec)
sys.modules[_integrity_spec.name] = integrity
_integrity_spec.loader.exec_module(integrity)


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        (root / "src").mkdir()
        (root / "scripts").mkdir()
        (root / ".github/workflows").mkdir(parents=True)
        (root / "scripts/check_test_lane_coverage.py").write_text(
            (REPO_ROOT / "scripts/check_test_lane_coverage.py").read_text("utf-8"), "utf-8"
        )
        (root / "justfile").write_text(
            "test-postgres:\n    cargo test -- _pg pg_ postgres --test-threads=1\n",
            "utf-8",
        )
        (root / ".github/workflows/ci-main.yml").write_text(
            self.workflow("cargo test postgres_ -- --test-threads=1", require=True), "utf-8"
        )
        (root / ".github/workflows/ci-nightly.yml").write_text(
            self.workflow("cargo test --all-targets -- --skip _pg_ --skip postgres_"), "utf-8"
        )
        self.write_pr(("src/db/**",))
        (root / "scripts/pg_test_lane_allowlist.txt").write_text("", "utf-8")

    @staticmethod
    def workflow(command: str, *, require: bool = False, start: bool = False) -> str:
        env = "    env:\n      AGENTDESK_REQUIRE_PG: \"1\"\n" if require else ""
        start_step = "      - run: ./scripts/ci/postgres-service.sh start\n" if (start or require) else ""
        return (
            "jobs:\n  lane:\n" + env + "    steps:\n" + start_step
            + f"      - run: {command}\n"
        )

    def write_pr(self, patterns: tuple[str, ...]) -> None:
        rendered = "\n".join(f"              - '{pattern}'" for pattern in patterns)
        (self.root / ".github/workflows/ci-pr.yml").write_text(
            "jobs:\n  changes:\n    steps:\n      - with:\n          filters: |\n"
            f"            pg_db:\n{rendered}\n            rust:\n              - 'src/**'\n",
            "utf-8",
        )

    def write_source(self, path: str, source: str, lib: str | None = None) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(source, "utf-8")
        if lib is not None:
            (self.root / "src/lib.rs").write_text(lib, "utf-8")
        elif not (self.root / "src/lib.rs").exists():
            (self.root / "src/lib.rs").write_text("", "utf-8")

    def debts(self):
        return membership.analyze(self.root).debts


class FixtureTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fx = Fixture(self.root)


class DetectionMutation(FixtureTestCase):
    def test_unmarked_seed_test_is_detected_and_production_pgpool_is_not(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "use sqlx::PgPool;\n#[cfg(test)] mod tests {\n"
            "#[test] fn bad() { create_test_database(); }\n"
            "#[test] fn counterexample() { assert!(true); }\n}\n",
            "mod service;\n",
        )
        inventory = membership.discover_pg_inventory(self.root)
        self.assertEqual(set(inventory.tests), {"service::tests::bad"})

    def test_one_hop_struct_closure_detects_use_but_pgpool_signature_does_not(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "struct Db; impl Db { fn make() { connect_test_pool(); } }\n"
            "struct Mock; impl Mock { fn pool(&self) -> Option<&PgPool> { None } }\n"
            "#[test] fn bad() { let _ = Db; }\n"
            "#[test] fn counterexample() { let _ = Mock; }\n}\n",
            "mod service;\n",
        )
        inventory = membership.discover_pg_inventory(self.root)
        self.assertEqual(set(inventory.tests), {"service::tests::bad"})


class RuleMutations(FixtureTestCase):
    def test_rule1_unmarked_module_fails_and_postgres_module_passes(self) -> None:
        for module, bad in (("tests", True), ("postgres_tests", False)):
            with self.subTest(module=module):
                self.fx.write_source(
                    "src/db/service.rs",
                    f"#[cfg(test)] mod {module} {{ #[test] fn case() {{ create_test_database(); }} }}\n",
                    "mod db { pub mod service; }\n",
                )
                self.assertEqual(bool(self.fx.debts()["rule1"]), bad)

    def test_rule2_bare_pg_tests_fails_and_canonical_names_pass(self) -> None:
        for module, bad in (("pg_tests", True), ("thing_pg_tests", False), ("postgres_tests", False)):
            with self.subTest(module=module):
                self.fx.write_source(
                    "src/db/service.rs",
                    f"#[cfg(test)] mod {module} {{ #[test] fn case() {{ create_test_database(); }} }}\n",
                    "mod db { pub mod service; }\n",
                )
                self.assertEqual(bool(self.fx.debts()["rule2"]), bad)

    def test_rule3_outside_filter_fails_explicit_glob_and_db_tree_pass(self) -> None:
        cases = (
            ("src/service.rs", ("src/db/**",), True),
            ("src/service.rs", ("src/service.rs",), False),
            ("src/db/service.rs", ("src/db/**",), False),
        )
        for path, patterns, bad in cases:
            with self.subTest(path=path, patterns=patterns), tempfile.TemporaryDirectory() as temp:
                fx = Fixture(Path(temp))
                fx.write_pr(patterns)
                fx.write_source(
                    path,
                    "#[cfg(test)] mod postgres_tests { #[test] fn case() { create_test_database(); } }\n",
                    "#[path = \"%s\"] mod service;\n" % path.removeprefix("src/"),
                )
                self.assertEqual(bool(fx.debts()["rule3"]), bad)

    def test_rule4_start_without_env_fails_and_pgless_job_passes(self) -> None:
        workflow = self.root / ".github/workflows/ci-main.yml"
        workflow.write_text(self.fx.workflow("cargo test postgres_", start=True), "utf-8")
        self.assertEqual(self.fx.debts()["rule4"], {".github/workflows/ci-main.yml:lane"})
        workflow.write_text(self.fx.workflow("cargo test --all-targets"), "utf-8")
        self.assertEqual(self.fx.debts()["rule4"], set())


class ParserMutations(FixtureTestCase):
    def test_path_alias_and_inline_nested_modules_use_logical_names(self) -> None:
        (self.root / "src/physical").mkdir()
        self.fx.write_source(
            "src/lib.rs", '#[path = "physical/leaf.rs"] mod logical;\n'
        )
        self.fx.write_source(
            "src/physical/leaf.rs",
            "mod nested { #[cfg(test)] mod postgres_tests {\n"
            "#[test] fn case() { create_test_database(); } } }\n",
        )
        inventory = membership.discover_pg_inventory(self.root)
        self.assertEqual(
            set(inventory.tests), {"logical::nested::postgres_tests::case"}
        )

    def test_pg_db_negation_overrides_positive_and_normal_case_passes(self) -> None:
        self.fx.write_source(
            "src/db/service.rs",
            "#[cfg(test)] mod postgres_tests { #[test] fn case() { create_test_database(); } }\n",
            "mod db { pub mod service; }\n",
        )
        self.fx.write_pr(("src/db/**", "!src/db/service.rs"))
        self.assertEqual(self.fx.debts()["rule3"], {"src/db/service.rs"})
        self.fx.write_pr(("src/db/**",))
        self.assertEqual(self.fx.debts()["rule3"], set())

    def test_bin_target_command_is_excluded_without_crashing(self) -> None:
        command = "cargo test --bin agentdesk foo:: -- --test-threads=1"
        self.assertIsNone(
            membership._load_coverage_module(self.root).cargo_test_filter(command)
        )
        (self.root / ".github/workflows/ci-main.yml").write_text(
            self.fx.workflow(command, require=True), "utf-8"
        )
        self.assertEqual(membership.analyze(self.root).inventory.tests, {})

    def test_same_bin_command_is_integrity_mismatch_but_not_membership_lane(self) -> None:
        command = "cargo test --bin agentdesk foo:: -- --test-threads=1"
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\n\n[lib]\npath = "src/lib.rs"\n\n'
            '[[bin]]\nname = "agentdesk"\npath = "src/main.rs"\n',
            "utf-8",
        )
        (self.root / "src/lib.rs").write_text("mod foo;\n", "utf-8")
        (self.root / "src/foo.rs").write_text("#[cfg(test)] mod tests {}\n", "utf-8")
        (self.root / "src/main.rs").write_text("fn main() {}\n", "utf-8")
        workflow = self.root / ".github/workflows/cross.yml"
        workflow.write_text(self.fx.workflow(command), "utf-8")
        violations = integrity.check_workflows(
            self.root, [workflow], set(), with_list_check=False
        )
        self.assertEqual([violation.kind for violation in violations], ["target-mismatch"])
        self.assertIsNone(
            membership._load_coverage_module(self.root).cargo_test_filter(command)
        )


class BaselineAndAllowlistContract(FixtureTestCase):
    def test_allowlist_requires_inline_reason(self) -> None:
        path = self.root / "scripts/pg_test_lane_allowlist.txt"
        path.write_text("test:service::tests::case\n", "utf-8")
        with self.assertRaisesRegex(ValueError, "reason comment"):
            membership.load_allowlist(path)
        path.write_text("test:service::tests::case # tracked by #999\n", "utf-8")
        self.assertEqual(membership.load_allowlist(path)[0], {"service::tests::case"})

    def test_manifest_and_sectioned_baseline_are_sorted(self) -> None:
        inventory = membership.PgInventory({"z::tests::b": "src/z.rs", "a::tests::a": "src/a.rs"})
        manifest = membership.render_manifest(inventory)
        self.assertLess(manifest.index("src/a.rs"), manifest.index("src/z.rs"))
        baseline = membership.render_baseline({section: {"z", "a"} for section in membership.SECTIONS})
        parsed = membership.parse_baseline(baseline, "fixture")
        self.assertEqual(parsed["rule1"], {"a", "z"})


class RealRepositoryContract(unittest.TestCase):
    def test_rederived_counts_match_design_revision_two(self) -> None:
        analysis = membership.analyze(REPO_ROOT)
        self.assertEqual(len(analysis.inventory.tests), 419)
        self.assertEqual(len(analysis.inventory.files), 70)
        self.assertEqual(len(analysis.debts["rule1"]), 118)
        self.assertEqual(len({name.rpartition("::")[0] for name in analysis.debts["rule1"]}), 25)
        self.assertEqual(len(analysis.debts["rule2"]), 260)
        self.assertEqual(len({name.rpartition("::")[0] for name in analysis.debts["rule2"]}), 55)
        self.assertEqual(len(analysis.debts["rule3"]), 30)
        self.assertEqual(len(analysis.debts["rule4"]), 7)


if __name__ == "__main__":
    unittest.main()
