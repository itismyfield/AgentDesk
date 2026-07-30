"""Hermetic mutation fixtures for the PostgreSQL lane membership gate (#4979)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/check_pg_test_lane_membership.py"
INTEGRITY_SCRIPT = REPO_ROOT / "scripts/check_test_target_integrity.py"


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


membership = load_module("check_pg_test_lane_membership", SCRIPT)
integrity = load_module("check_test_target_integrity_cross_fixture", INTEGRITY_SCRIPT)


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
            "test-postgres:\n    cargo test -- _pg pg_ postgres --test-threads=1\n", "utf-8"
        )
        (root / ".github/workflows/ci-main.yml").write_text(
            self.workflow("cargo test postgres_ -- --test-threads=1", require=True), "utf-8"
        )
        (root / ".github/workflows/ci-nightly.yml").write_text(
            self.workflow("cargo test --all-targets -- --skip _pg_ --skip postgres_"), "utf-8"
        )
        self.write_pr(("src/db/**",))
        (root / "scripts/pg_test_lane_allowlist.txt").write_text("", "utf-8")
        (root / "src/lib.rs").write_text("", "utf-8")

    @staticmethod
    def workflow(command: str, *, require: bool = False, start: bool = False) -> str:
        env = "    env:\n      AGENTDESK_REQUIRE_PG: \"1\"\n" if require else ""
        start_step = "      - run: ./scripts/ci/postgres-service.sh start\n" if (start or require) else ""
        return "on:\n  push:\njobs:\n  lane:\n" + env + "    steps:\n" + start_step + f"      - run: {command}\n"

    def write_pr(self, patterns: tuple[str, ...], indent: int = 12) -> None:
        prefix = " " * indent
        rendered = "\n".join(f"{prefix}  - '{pattern}'" for pattern in patterns)
        (self.root / ".github/workflows/ci-pr.yml").write_text(
            "jobs:\n  changes:\n    steps:\n      - with:\n          filters: |\n"
            f"{prefix}pg_db:\n{rendered}\n{prefix}rust:\n{prefix}  - 'src/**'\n",
            "utf-8",
        )

    def write_source(self, path: str, source: str, lib: str | None = None) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(source, "utf-8")
        if lib is not None:
            (self.root / "src/lib.rs").write_text(lib, "utf-8")

    def analysis(self):
        return membership.analyze(self.root)


class FixtureCase(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fx = Fixture(self.root)


class DetectionMutation(FixtureCase):
    def test_seed_detection_and_production_counterexample(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "use sqlx::PgPool;\n#[cfg(test)] mod tests {\n"
            "#[test] fn bad() { create_test_database(); }\n"
            "#[test] fn counterexample() { assert!(true); }\n}\n",
            "mod service;\n",
        )
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), {"service::tests::bad"})

    def test_struct_closure_and_pgpool_signature_counterexample(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "struct Db; impl Db { fn make() { connect_test_pool(); } }\n"
            "struct Mock; impl Mock { fn pool(&self) -> Option<&PgPool> { None } }\n"
            "#[test] fn bad() { let _ = Db; }\n"
            "#[test] fn counterexample() { let _ = Mock; }\n}\n",
            "mod service;\n",
        )
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), {"service::tests::bad"})

    def test_free_function_closure_and_seed_suffix_variant(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "async fn create_pool() { connect_and_migrate_with_max_connections(); }\n"
            "#[test] fn bad() { create_pool(); }\n}\n",
            "mod service;\n",
        )
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), {"service::tests::bad"})

    def test_seed_names_require_word_boundaries(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests { #[test] fn lazy() { let _ = PgPoolOptions::new(); } }\n",
            "mod service;\n",
        )
        self.assertEqual(membership.discover_pg_inventory(self.root).tests, {})


class RuleMutations(FixtureCase):
    def test_rule1_bad_and_good(self) -> None:
        for module, bad in (("tests", True), ("postgres_tests", False)):
            with self.subTest(module=module):
                self.fx.write_source("src/db/service.rs", f"#[cfg(test)] mod {module} {{ #[test] fn case() {{ create_test_database(); }} }}\n", "mod db { pub mod service; }\n")
                self.assertEqual(bool(self.fx.analysis().debts["rule1"]), bad)

    def test_rule2_bad_and_good(self) -> None:
        for module, bad in (("pg_tests", True), ("thing_pg_tests", False), ("postgres_tests", False)):
            with self.subTest(module=module):
                self.fx.write_source("src/db/service.rs", f"#[cfg(test)] mod {module} {{ #[test] fn case() {{ create_test_database(); }} }}\n", "mod db { pub mod service; }\n")
                self.assertEqual(bool(self.fx.analysis().debts["rule2"]), bad)

    def test_rule3_bad_explicit_and_db_glob(self) -> None:
        cases = (("src/service.rs", ("src/db/**",), True), ("src/service.rs", ("src/service.rs",), False), ("src/db/service.rs", ("src/db/**",), False))
        for path, patterns, bad in cases:
            with self.subTest(path=path), tempfile.TemporaryDirectory() as temp:
                fx = Fixture(Path(temp))
                fx.write_pr(patterns)
                fx.write_source(path, "#[cfg(test)] mod postgres_tests { #[test] fn case() { create_test_database(); } }\n", "#[path = \"%s\"] mod service;\n" % path.removeprefix("src/"))
                self.assertEqual(bool(fx.analysis().debts["rule3"]), bad)

    def test_rule4_bad_and_pgless_counterexample(self) -> None:
        workflow = self.root / ".github/workflows/ci-main.yml"
        workflow.write_text(self.fx.workflow("cargo test postgres_", start=True), "utf-8")
        self.assertEqual(self.fx.analysis().debts["rule4"], {".github/workflows/ci-main.yml:lane"})
        workflow.write_text(self.fx.workflow("cargo test --all-targets"), "utf-8")
        self.assertEqual(self.fx.analysis().debts["rule4"], set())


class ParserMutations(FixtureCase):
    def test_alias_and_nested_module(self) -> None:
        (self.root / "src/physical").mkdir()
        self.fx.write_source("src/lib.rs", '#[path = "physical/leaf.rs"] mod logical;\n')
        self.fx.write_source("src/physical/leaf.rs", "mod nested { #[cfg(test)] mod postgres_tests { #[test] fn case() { create_test_database(); } } }\n")
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), {"logical::nested::postgres_tests::case"})

    def test_negation_and_indentation_refactor(self) -> None:
        self.fx.write_source("src/db/service.rs", "#[cfg(test)] mod postgres_tests { #[test] fn case() { create_test_database(); } }\n", "mod db { pub mod service; }\n")
        self.fx.write_pr(("src/db/**", "!src/db/service.rs"), indent=8)
        self.assertEqual(self.fx.analysis().debts["rule3"], {"src/db/service.rs"})
        self.fx.write_pr(("src/db/**",), indent=8)
        self.assertEqual(self.fx.analysis().debts["rule3"], set())

    def test_step_name_is_not_a_cargo_command(self) -> None:
        text = "steps:\n  - name: cargo test (postgres bootstrap)\n    run: echo no\n  - run: cargo test postgres_\n"
        self.assertEqual(membership._cargo_commands(text), ["cargo test postgres_"])

    def test_realistic_jobs_whitespace_first_job_and_comment_separator(self) -> None:
        workflow = self.root / ".github/workflows/extra.yaml"
        workflow.write_text(
            "on:\n  push:\n  workflow_dispatch:\n\njobs:\n"
            "  first:\n    steps:\n      - run: echo first\n"
            "\n  # The comment must not make the following job disappear.\n"
            "  second:\n    steps:\n      - run: echo second\n",
            "utf-8",
        )
        jobs = membership.parse_jobs(workflow, self.root)
        self.assertEqual([job.name for job in jobs], ["first", "second"])
        self.assertNotIn("second", jobs[0].text)

    def test_first_job_rule4_violation_is_visible(self) -> None:
        workflow = self.root / ".github/workflows/ci-main.yml"
        workflow.write_text(
            "on:\n  push:\n\njobs:\n"
            "  first_pg_job:\n    steps:\n"
            "      - run: ./scripts/ci/postgres-service.sh start\n"
            "      - run: cargo test postgres_\n"
            "\n  # A separated second job exercises job boundaries.\n"
            "  second:\n    steps:\n      - run: echo ok\n",
            "utf-8",
        )
        self.assertEqual(
            self.fx.analysis().debts["rule4"],
            {".github/workflows/ci-main.yml:first_pg_job"},
        )

    def test_yaml_extension_is_seen(self) -> None:
        workflow = self.root / ".github/workflows/extra.yaml"
        workflow.write_text("jobs:\n  real:\n    steps:\n      - run: echo ok\n", "utf-8")
        with mock.patch.object(membership, "discover_pg_inventory", return_value=membership.PgInventory({})):
            self.assertEqual(self.fx.analysis().inventory.tests, {})

    def test_cross_checker_bin_command(self) -> None:
        command = "cargo test --bin agentdesk foo:: -- --test-threads=1"
        (self.root / "Cargo.toml").write_text('[package]\nname="fixture"\n[lib]\npath="src/lib.rs"\n[[bin]]\nname="agentdesk"\npath="src/main.rs"\n', "utf-8")
        (self.root / "src/lib.rs").write_text("mod foo;\n", "utf-8")
        (self.root / "src/foo.rs").write_text("#[cfg(test)] mod tests {}\n", "utf-8")
        (self.root / "src/main.rs").write_text("fn main() {}\n", "utf-8")
        workflow = self.root / ".github/workflows/cross.yml"
        workflow.write_text(self.fx.workflow(command), "utf-8")
        violations = integrity.check_workflows(self.root, [workflow], set(), False)
        self.assertEqual([violation.kind for violation in violations], ["target-mismatch"])
        self.assertIsNone(membership._load_coverage_module(self.root).cargo_test_filter(command))


class BaselineAndEnforcement(FixtureCase):
    def analysis(self, debts: dict[str, set[str]] | None = None):
        return membership.Analysis(
            membership.PgInventory({"service::postgres_tests::case": "src/db/service.rs"}),
            debts or {section: set() for section in membership.SECTIONS},
            0,
        )

    def run_check(self, analysis, baseline, reference, manifest=None):
        stderr = io.StringIO()
        stdout = io.StringIO()
        with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(stdout):
            rc = membership.check_analysis(
                analysis,
                baseline,
                reference,
                manifest if manifest is not None else membership.render_manifest(analysis.inventory),
                reference_label="fixture base",
                allowlist_label="scripts/pg_test_lane_allowlist.txt",
            )
        return rc, stdout.getvalue(), stderr.getvalue()

    def test_repair_passes_and_stale_baseline_fails(self) -> None:
        old = {section: set() for section in membership.SECTIONS}
        old["rule3"] = {"src/db/service.rs"}
        rc, _, _ = self.run_check(self.analysis(), {section: set() for section in membership.SECTIONS}, old)
        self.assertEqual(rc, 0)
        rc, _, error = self.run_check(self.analysis(), old, old)
        self.assertEqual(rc, 1)
        self.assertIn("Remove '-' entries", error)

    def test_new_violation_and_baseline_coverup_fail(self) -> None:
        empty = {section: set() for section in membership.SECTIONS}
        current = {section: set() for section in membership.SECTIONS}
        current["rule1"] = {"service::tests::case"}
        rc, _, error = self.run_check(self.analysis(current), empty, empty)
        self.assertEqual(rc, 1)
        self.assertIn("Fix '+' violations", error)
        rc, _, error = self.run_check(self.analysis(current), current, empty)
        self.assertEqual(rc, 1)
        self.assertIn("baseline growth forbidden", error)

    def test_manifest_drift_fails_with_complete_recovery_command(self) -> None:
        empty = {section: set() for section in membership.SECTIONS}
        rc, _, error = self.run_check(self.analysis(), empty, empty, "")
        self.assertEqual(rc, 1)
        self.assertIn("python3 scripts/check_pg_test_lane_membership.py --write-snapshots", error)
        self.assertIn("rewrites BOTH the manifest and baseline", error)
        self.assertIn("will not excuse", error)

    def test_inventory_change_passes_when_manifest_matches(self) -> None:
        empty = {section: set() for section in membership.SECTIONS}
        analysis = membership.Analysis(
            membership.PgInventory({
                "service::postgres_tests::case": "src/db/service.rs",
                "service::postgres_tests::new_case": "src/db/service.rs",
            }),
            empty,
            0,
        )
        self.assertEqual(self.run_check(analysis, empty, empty)[0], 0)

    def test_allowlist_requires_reason(self) -> None:
        path = self.root / "scripts/pg_test_lane_allowlist.txt"
        path.write_text("test:service::tests::case\n", "utf-8")
        with self.assertRaisesRegex(ValueError, "reason comment"):
            membership.load_allowlist(path)
        path.write_text("test:service::tests::case # tracked by #999\n", "utf-8")
        self.assertEqual(membership.load_allowlist(path)[0], {"service::tests::case"})


class MutationProof(FixtureCase):
    def test_detector_patch_is_caught_by_fixture_assertion(self) -> None:
        self.fx.write_source("src/service.rs", "#[cfg(test)] mod tests { #[test] fn bad() { create_test_database(); } }\n", "mod service;\n")
        expected = {"service::tests::bad"}
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)
        with mock.patch.object(membership, "discover_pg_inventory", return_value=membership.PgInventory({})):
            with self.assertRaises(AssertionError):
                self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)


if __name__ == "__main__":
    unittest.main()
