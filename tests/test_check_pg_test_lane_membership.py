"""Hermetic mutation fixtures for the PostgreSQL lane membership gate (#4979)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import re
import shlex
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/check_pg_test_lane_membership.py"
INTEGRITY_SCRIPT = REPO_ROOT / "scripts/check_test_target_integrity.py"
FIXTURES = REPO_ROOT / "tests/fixtures/pg_lane"


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

    def write_pr(
        self,
        patterns: tuple[str, ...],
        indent: int = 12,
        *,
        sweep: str | None = None,
        sweep_starts_service: bool = False,
        sweep_block: str | None = None,
    ) -> None:
        prefix = " " * indent
        rendered = "\n".join(f"{prefix}  - '{pattern}'" for pattern in patterns)
        extra = ""
        if sweep is not None:
            start = "      - run: ./scripts/ci/postgres-service.sh start\n" if sweep_starts_service else ""
            env = "    env:\n      AGENTDESK_REQUIRE_PG: \"1\"\n" if sweep_starts_service else ""
            extra = f"  sweep:\n{env}    steps:\n{start}      - run: {sweep}\n"
        if sweep_block is not None:
            extra += sweep_block
        (self.root / ".github/workflows/ci-pr.yml").write_text(
            "jobs:\n  changes:\n    steps:\n      - with:\n          filters: |\n"
            f"{prefix}pg_db:\n{rendered}\n{prefix}rust:\n{prefix}  - 'src/**'\n"
            + extra,
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


class NonPgFilterContract(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts/ci").mkdir(parents=True)
        (self.root / ".github/workflows").mkdir(parents=True)
        (self.root / membership.NON_PG_FILTER_REL).write_text(
            (REPO_ROOT / membership.NON_PG_FILTER_REL).read_text("utf-8"), "utf-8"
        )
        (self.root / membership.LIB_TEST_INVENTORY_REL).write_text(
            (REPO_ROOT / membership.LIB_TEST_INVENTORY_REL).read_text("utf-8"),
            "utf-8",
        )
        consumer = (
            "    steps:\n"
            "      - run: |\n"
            "          source scripts/ci/non-pg-test-filter.sh\n"
            "          cargo test --all-targets -- \"${NON_PG_SKIP_ARGS[@]}\"\n"
            "          run_non_pg_filter_false_positives\n"
        )
        (self.root / ".github/workflows/ci-pr.yml").write_text(
            "jobs:\n  library_sweep:\n" + consumer, "utf-8"
        )
        (self.root / ".github/workflows/ci-nightly.yml").write_text(
            "jobs:\n  full_macos:\n"
            + consumer
            + "  full_windows:\n"
            + consumer
            + "  postgres_full:\n"
            + "    steps:\n"
            + "      - run: |\n"
            + "          source scripts/ci/non-pg-test-filter.sh\n"
            + "          cargo test --all-targets -- \"${PG_INCLUDE_ARGS[@]}\"\n",
            "utf-8",
        )

    def jobs(self):
        findings = []
        return [
            job
            for workflow in membership.NON_PG_FILTER_WORKFLOWS
            for job in membership.parse_jobs(self.root / workflow, self.root, findings)
        ]

    def test_shared_filter_consumers_and_parser_expansion(self) -> None:
        self.assertEqual(
            membership.non_pg_filter_contract_errors(self.root, self.jobs()), ()
        )
        args = membership.load_non_pg_skip_args(self.root)
        self.assertEqual(
            membership._cargo_commands(
                'run: cargo test --all-targets -- "${NON_PG_SKIP_ARGS[@]}"',
                args,
            ),
            ["cargo test --all-targets -- " + shlex.join(args)],
        )
        self.assertEqual(
            membership._cargo_commands(
                'run: cargo test --all-targets -- "${PG_INCLUDE_ARGS[@]}"',
                args,
            ),
            ["cargo test --all-targets -- " + shlex.join(args[1::2])],
        )

    def test_mutating_only_nightly_to_a_literal_filter_is_rejected(self) -> None:
        path = self.root / ".github/workflows/ci-nightly.yml"
        path.write_text(
            path.read_text("utf-8").replace(
                '"${NON_PG_SKIP_ARGS[@]}"', "--skip mutated", 1
            ),
            "utf-8",
        )
        errors = membership.non_pg_filter_contract_errors(self.root, self.jobs())
        self.assertTrue(any("literal --skip" in error for error in errors))

    def test_redefining_sourced_array_is_rejected(self) -> None:
        path = self.root / ".github/workflows/ci-nightly.yml"
        path.write_text(
            path.read_text("utf-8").replace(
                "          cargo test --all-targets -- \"${NON_PG_SKIP_ARGS[@]}\"\n",
                "          NON_PG_SKIP_ARGS=(\"${NON_PG_SKIP_ARGS[@]:0:4}\")\n"
                "          cargo test --all-targets -- \"${NON_PG_SKIP_ARGS[@]}\"\n",
                1,
            ),
            "utf-8",
        )
        errors = membership.non_pg_filter_contract_errors(self.root, self.jobs())
        self.assertTrue(
            any("redefines canonical NON_PG_SKIP_ARGS" in error for error in errors)
        )

    def test_replay_id_must_exist_in_libtest_inventory(self) -> None:
        path = self.root / membership.NON_PG_FILTER_REL
        first = membership.load_non_pg_false_positives(self.root)[0]
        path.write_text(
            path.read_text("utf-8").replace(first, first + "_renamed"), "utf-8"
        )
        errors = membership.non_pg_filter_contract_errors(self.root, self.jobs())
        self.assertTrue(
            any(
                "replay id is absent" in error and "_renamed" in error
                for error in errors
            )
        )


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

    def test_helper_identity_is_limited_to_logical_module_scope(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod a { fn h() { create_test_database(); } }\n"
            "#[cfg(test)] mod b { fn h() {} #[test] fn plain() { h(); } }\n",
            "mod service;\n",
        )
        self.assertEqual(membership.discover_pg_inventory(self.root).tests, {})

    def test_qualified_path_segments_are_not_reinterpreted_in_caller_scope(self) -> None:
        self.fx.write_source(
            "src/lib.rs",
            "#[cfg(test)] mod other { pub fn h() {} }\n"
            "#[cfg(test)] mod caller {\n"
            "fn other() { create_test_database(); }\n"
            "fn h() { create_test_database(); }\n"
            "#[test] fn qualified_plain() { crate::other::h(); }\n"
            "}\n",
        )
        self.assertEqual(membership.discover_pg_inventory(self.root).tests, {})

    def test_impl_wide_seed_is_retained_and_sibling_method_debt_is_explicit(self) -> None:
        """F9/C3 remains deferred: impl-wide identity can over-classify siblings."""
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "struct TestDatabase;\n"
            "impl TestDatabase { async fn create() { create_test_database(); } fn pure() {} }\n"
            "#[test] fn direct_assoc() { let _db = TestDatabase::create().await; }\n"
            "#[test] fn sibling_pure() { TestDatabase::pure(); }\n"
            "}\n",
            "mod service;\n",
        )
        found = set(membership.discover_pg_inventory(self.root).tests)
        self.assertEqual(found, {
            "service::tests::direct_assoc",
            "service::tests::sibling_pure",
        })

    def test_qualified_free_helpers_are_detected(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "fn through_assoc() { TestDatabase::create(); }\n"
            "#[test] fn transitive_assoc() { through_assoc(); }\n"
            "}\n",
            "mod service;\n",
        )
        self.assertEqual(membership.discover_pg_inventory(self.root).tests, {})

    def test_impl_method_positive_is_not_claimed_fail_open(self) -> None:
        """The known impl sibling false positive is an explicit deferred debt."""
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests { struct T; impl T { fn pg() { create_test_database(); } fn pure() {} } #[test] fn case() { T::pure(); } }\n",
            "mod service;\n",
        )
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), {"service::tests::case"})

    def test_impl_wide_identity_and_qualified_free_helpers_are_detected(self) -> None:
        """Impl identity is deliberately type-wide; method dispatch remains deferred."""
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "struct TestDatabase;\n"
            "impl TestDatabase { async fn create() { create_test_database(); } }\n"
            "fn through_assoc() { TestDatabase::create(); }\n"
            "#[test] fn direct_assoc() { let _db = TestDatabase::create().await; }\n"
            "#[test] fn transitive_assoc() { through_assoc(); }\n"
            "}\n",
            "mod service;\n",
        )
        expected = {
            "service::tests::direct_assoc",
            "service::tests::transitive_assoc",
        }
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)

        self.fx.write_source(
            "src/lib.rs",
            "#[cfg(test)] mod support { pub fn h() { create_test_database(); } }\n"
            "#[cfg(test)] mod tests { #[test] fn qualified() { crate::support::h(); } }\n",
        )
        self.fx.write_source("src/service.rs", "")
        self.assertEqual(
            set(membership.discover_pg_inventory(self.root).tests),
            {"tests::qualified"},
        )

    def test_module_scoped_short_helper_collision_is_negative(self) -> None:
        self.fx.write_source(
            "src/lib.rs",
            "#[cfg(test)] mod pg_support { fn create() { create_test_database(); } }\n"
            "#[cfg(test)] mod unrelated { fn create() {} #[test] fn plain() { create(); } }\n",
        )
        self.assertEqual(membership.discover_pg_inventory(self.root).tests, {})

    def test_two_hop_cross_file_helper_is_positive(self) -> None:
        self.fx.write_source(
            "src/a.rs",
            "#[cfg(test)] mod tests { use crate::b::helpers::middle; fn outer() { middle(); } #[test] fn case() { outer(); } }\n",
            "mod a; mod b;\n",
        )
        self.fx.write_source(
            "src/b.rs",
            "#[cfg(test)] pub mod helpers { pub fn middle() { seeded(); } fn seeded() { create_test_database(); } }\n",
        )
        self.assertEqual(
            set(membership.discover_pg_inventory(self.root).tests),
            {"a::tests::case"},
        )

    def test_rust_2018_sibling_external_module_is_discovered(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod postgres_tests;\n",
            "mod service;\n",
        )
        self.fx.write_source(
            "src/service/postgres_tests.rs",
            "#[test] fn case() { create_test_database(); }\n",
        )
        self.fx.write_source(
            "src/postgres_tests.rs",
            "#[test] fn wrong_rust_2015_path() { create_test_database(); }\n",
        )
        self.assertEqual(
            set(membership.discover_pg_inventory(self.root).tests),
            {"service::postgres_tests::case"},
        )

    def test_path_redirect_is_read_raw_and_missing_module_fails_closed(self) -> None:
        self.fx.write_source(
            "src/lib.rs",
            '#[cfg(test)]\n#[path = "db/pg_case.rs"]\nmod renamed;\n'
            "#[cfg(test)]\nmod missing;\n",
        )
        self.fx.write_source(
            "src/db/pg_case.rs",
            "#[test] fn case() { create_test_database(); }\n",
        )
        analysis = self.fx.analysis()
        self.assertEqual(
            set(analysis.inventory.tests), {"renamed::case"},
        )
        unresolved = [
            finding for finding in analysis.findings
            if finding.kind == "unresolved-external-test-module"
        ]
        self.assertEqual(len(unresolved), 1)
        self.assertEqual(unresolved[0].source, "src/lib.rs:4")
        self.assertIn("src/missing.rs, src/missing/mod.rs", unresolved[0].detail)
        empty = {section: set() for section in membership.SECTIONS}
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            rc = membership.check_analysis(
                analysis, empty, empty, membership.render_manifest(analysis.inventory),
                reference_label="fixture base", allowlist_label="fixture allowlist",
            )
        self.assertEqual(rc, 1)
        self.assertIn("FAIL: [unresolved-external-test-module]", stderr.getvalue())
        voice = (REPO_ROOT / "src/services/discord/voice_barge_in.rs").read_text("utf-8")
        voice = voice.replace('    #[path = "pcm_harness_tests.rs"]\n    mod pcm_harness_tests;', '    #[cfg(test)]\n    #[path = "pcm_harness_tests.rs"]\n    mod pcm_harness_tests;', 1)
        self.fx.write_source("src/services/discord/voice_barge_in.rs", voice)
        target = "src/services/discord/voice_barge_in/tests/pcm_harness_tests.rs"
        self.fx.write_source(target, (REPO_ROOT / target).read_text("utf-8"))
        findings: list[membership.Finding] = []
        self.assertIn((self.root / target).resolve(), membership._external_test_files(self.root.resolve(), membership._load_coverage_module(self.root), findings=findings))
        self.assertFalse([finding for finding in findings if finding.source.startswith("src/services/discord/voice_barge_in.rs:")])
        self.fx.write_source("src/db/mod.rs", '#[cfg(test)]\n// #[path = "moved/elsewhere_tests.rs"]\nmod frag;\n', "mod db;\n")
        self.fx.write_source("src/db/frag.rs", "#[test] fn plain() {}\n")
        membership._external_test_files(self.root.resolve(), membership._load_coverage_module(self.root), findings=(findings := []))
        self.assertEqual(findings, [])
        self.assertEqual(membership.check_analysis(analysis := self.fx.analysis(), empty, empty, membership.render_manifest(analysis.inventory), reference_label="fixture base", allowlist_label="fixture allowlist"), 0)

    def test_commented_stale_path_does_not_hide_real_pg_test(self) -> None:
        self.fx.write_source("src/db/service.rs", '#[cfg(test)]\n// #[path = "legacy_tests.rs"]\nmod frag;\n', "mod db;\n")
        self.fx.write_source("src/db/legacy_tests.rs", "#[test] fn legacy_plain() {}\n")
        self.fx.write_source("src/db/service/frag.rs", "#[test] fn real_pg_case() { create_test_database(); }\n")
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), {"db::service::frag::real_pg_case"})

    def test_brace_ownership_excludes_adjacent_helper_body(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests { fn plain() {} fn seeded() { create_test_database(); } #[test] fn case() { plain(); } }\n",
            "mod service;\n",
        )
        self.assertEqual(membership.discover_pg_inventory(self.root).tests, {})

    def test_block_local_helper_and_impl_method_are_negative(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests { #[test] fn block() { fn h() { create_test_database(); } h(); } #[test] fn impl_case() { struct T; impl T { fn h() { create_test_database(); } } T::h(); } }\n",
            "mod service;\n",
        )
        self.assertEqual(membership.discover_pg_inventory(self.root).tests, {})


    def test_multi_segment_associated_and_ufcs_transitive_calls(self) -> None:
        self.fx.write_source(
            "src/lib.rs",
            (FIXTURES / "associated_calls.rs").read_text("utf-8"),
        )
        expected = {"tests::multi_segment_case", "tests::ufcs_case"}
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)

        legacy_call = re.compile(
            r"(?<![.:])\b(?P<path>(?:(?:crate|self|super)"
            r"(?:::[A-Za-z_][A-Za-z0-9_]*)+|[A-Za-z_][A-Za-z0-9_]*))\s*!?\s*\("
        )
        no_ufcs = re.compile(r"(?!)")
        with mock.patch.object(membership, "_CALL", legacy_call), mock.patch.object(
            membership, "_UFCS_CALL", no_ufcs
        ):
            self.assertEqual(
                set(membership.discover_pg_inventory(self.root).tests),
                {"tests::ufcs_case"},
            )
            self.assertNotEqual({"tests::ufcs_case"}, expected)

    def test_function_signature_braces_do_not_steal_body_ownership(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "fn generic<const N: usize>() where [(); { N }]: Sized { }\n"
            "fn seeded() { create_test_database(); }\n"
            "#[test] fn plain() { generic::<1>(); }\n"
            "#[test] fn case() { seeded(); }\n"
            "}\n",
            "mod service;\n",
        )
        self.assertEqual(
            set(membership.discover_pg_inventory(self.root).tests),
            {"service::tests::case"},
        )

    def test_nested_impl_mask_starts_at_the_consumed_opening_brace(self) -> None:
        """Block-local impls remain fail-open while their braces stay owned."""
        body = (
            "{ impl Local { fn pure() {} fn pg() { create_test_database(); } } "
            "after(); }"
        )
        no_functions = re.compile(r"(?!)")
        with mock.patch.object(membership, "_FN", no_functions):
            masked = membership._mask_nested_items(body)
        self.assertNotIn("create_test_database", masked)
        self.assertIn("after()", masked)

    def test_cross_file_three_hop_transitive_closure(self) -> None:
        self.fx.write_source(
            "src/xfile_a.rs",
            (FIXTURES / "xfile_a.rs").read_text("utf-8"),
            "mod xfile_a;\nmod xfile_b;\n",
        )
        self.fx.write_source(
            "src/xfile_b.rs",
            (FIXTURES / "xfile_b.rs").read_text("utf-8"),
        )
        expected = {"xfile_a::tests::transitive_case"}
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)
        with mock.patch.object(membership, "_transitive_closure", return_value=False):
            with self.assertRaises(AssertionError):
                self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)

    def test_brace_aware_edges_do_not_capture_the_next_function(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "fn bridge() {}\n"
            "fn seeded() { create_test_database(); }\n"
            "#[test] fn plain() { bridge(); }\n"
            "#[test] fn case() { seeded(); }\n"
            "}\n",
            "mod service;\n",
        )
        expected = {"service::tests::case"}
        self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)

        def unbounded_body(clean: str, opening: int, counter=None) -> str:
            return clean[opening:]

        with mock.patch.object(membership, "_edge_boundary", side_effect=unbounded_body):
            with self.assertRaises(AssertionError):
                self.assertEqual(set(membership.discover_pg_inventory(self.root).tests), expected)

    def test_block_local_items_remain_fail_open(self) -> None:
        self.fx.write_source(
            "src/service.rs",
            "#[cfg(test)] mod tests {\n"
            "#[test] fn pg() { fn h() { create_test_database(); } h(); }\n"
            "#[test] fn plain() { fn h() {} h(); }\n"
            "#[test] fn local_impl() {\n"
            "struct Local; impl Local { fn pure() {} fn pg() { create_test_database(); } }\n"
            "Local::pure();\n"
            "}\n"
            "}\n",
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

    # ------------------------------------------------------------------
    # rule5 (#5185): a ci-pr.yml job that runs cargo test without starting the
    # PostgreSQL service must select no PG-dependent test. The tests below pin
    # the three properties that make it discriminating rather than decorative:
    # it sees `--lib` lanes (rule2 cannot), it is scoped to the PR workflow,
    # and it fails with no baseline to ratchet against.
    # ------------------------------------------------------------------
    SWEEP = "cargo test --lib -- --skip _pg --skip pg_ --skip postgres"

    def write_pg_test(self, module: str = "tests") -> None:
        """A PG-dependent test whose id carries none of the skip substrings."""
        self.fx.write_source(
            "src/db/service.rs",
            f"#[cfg(test)] mod {module} {{ #[test] fn case() {{ create_test_database(); }} }}\n",
            "mod db { pub mod service; }\n",
        )

    def test_rule5_pr_lib_lane_without_service_is_debt_and_service_clears_it(self) -> None:
        self.write_pg_test()
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP)
        self.assertEqual(self.fx.analysis().debts["rule5"], {"db::service::tests::case"})
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP, sweep_starts_service=True)
        analysis = self.fx.analysis()
        self.assertEqual(analysis.debts["rule5"], set())
        # Starting the service also makes the sweep a PG lane, so the same test
        # stops being rule1 debt -- the coverage half of the same fix.
        self.assertEqual(analysis.debts["rule1"], set())
        self.assertEqual(analysis.debts["rule4"], set())

    def test_rule5_sees_lib_lanes_that_pgless_lane_filters_cannot(self) -> None:
        """The blind spot that let the defect ship: rule2 only reads --all-targets."""
        self.write_pg_test()
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP)
        jobs = [
            job
            for path in sorted((self.root / ".github/workflows").glob("*.yml"))
            for job in membership.parse_jobs(path, self.root, [])
        ]
        coverage = membership._load_coverage_module(self.root)
        sweep_lane = coverage.cargo_test_filter(self.SWEEP)
        self.assertNotIn(sweep_lane, membership.pgless_lane_filters(jobs, coverage))
        self.assertEqual(membership.pr_pgless_lane_filters(jobs, coverage), (sweep_lane,))
        # And this is why rule2 could not have caught it even by counting: the
        # nightly `--all-targets` lane already carries the same id, so ADDING
        # the PR sweep moves rule2 by nothing at all while rule5 goes 0 -> 1.
        with_sweep = self.fx.analysis()
        self.fx.write_pr(("src/db/**",))
        without_sweep = self.fx.analysis()
        self.assertEqual(with_sweep.debts["rule2"], without_sweep.debts["rule2"])
        self.assertEqual(without_sweep.debts["rule5"], set())
        self.assertEqual(with_sweep.debts["rule5"], {"db::service::tests::case"})

    def test_rule5_is_scoped_to_the_pr_workflow(self) -> None:
        self.write_pg_test()
        self.fx.write_pr(("src/db/**",))
        (self.root / ".github/workflows/ci-nightly.yml").write_text(
            self.fx.workflow(self.SWEEP), "utf-8"
        )
        self.assertEqual(self.fx.analysis().debts["rule5"], set())

    def test_rule5_fails_without_any_baseline_to_ratchet(self) -> None:
        inventory = membership.PgInventory({"db::service::tests::case": "src/db/service.rs"})
        debts = empty_debts()
        debts["rule5"] = {"db::service::tests::case"}
        empty = empty_debts()
        stderr, stdout = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(stdout):
            rc = membership.check_analysis(
                membership.Analysis(inventory, debts, 0),
                {section: set() for section in membership.SECTIONS},
                {section: set() for section in membership.SECTIONS},
                membership.render_manifest(inventory),
                reference_label="fixture base",
                allowlist_label="scripts/pg_test_lane_allowlist.txt",
            )
        self.assertEqual(rc, 1)
        self.assertIn("FAIL: [rule5]", stderr.getvalue())
        self.assertIn("db::service::tests::case", stderr.getvalue())
        # rule5 is not a baseline section, so a regenerated snapshot cannot
        # absorb it the way rule1-rule4 debt can.
        self.assertNotIn("rule5", membership.render_baseline(debts))

    # ------------------------------------------------------------------
    # The allowlist bypass. `analyze` subtracts allowlisted ids before any rule
    # runs, which is the right scope for rule1-rule4 -- they are budgets, and a
    # proven classifier false positive should not spend budget -- and was the
    # wrong scope for rule5, which is not a budget. The three tests below pin
    # the fix from both sides: rule5 stops reading the allowlist, and rule1-
    # rule4 keep reading it exactly as they did.
    # ------------------------------------------------------------------
    def test_rule5_ignores_the_allowlist_that_still_clears_rule1_and_rule2(self) -> None:
        self.write_pg_test()
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP)
        before = self.fx.analysis()
        self.assertEqual(before.allowlist_count, 0)
        self.assertEqual(before.debts["rule5"], {"db::service::tests::case"})
        self.assertEqual(before.debts["rule1"], {"db::service::tests::case"})
        self.assertEqual(before.debts["rule2"], {"db::service::tests::case"})

        (self.root / "scripts/pg_test_lane_allowlist.txt").write_text(
            "test:db::service::tests::case  # false positive, tracked in #9999\n", "utf-8"
        )
        after = self.fx.analysis()
        self.assertEqual(after.allowlist_count, 1)
        # rule1-rule4 keep the allowance they were designed with. Changing this
        # would be a separate decision about a separate rule; the entry above is
        # accepted by `load_allowlist` on the strength of an unverified issue
        # number, which is precisely why rule5 must not be reachable this way.
        self.assertEqual(after.debts["rule1"], set())
        self.assertEqual(after.debts["rule2"], set())
        # rule5 does not. It is computed over the unfiltered inventory.
        self.assertEqual(after.debts["rule5"], {"db::service::tests::case"})

    def test_write_snapshots_refuses_to_regenerate_over_a_live_rule5(self) -> None:
        self.write_pg_test()
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP)
        manifest = self.root / membership.MANIFEST_REL
        baseline = self.root / membership.BASELINE_REL
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(io.StringIO()):
            rc = membership.main(["--repo-root", str(self.root), "--write-snapshots"])
        self.assertEqual(rc, 1)
        self.assertIn("refusing to write snapshots", stderr.getvalue())
        self.assertFalse(manifest.exists())
        self.assertFalse(baseline.exists())

        # Fixing the workflow, not the snapshot, is what unblocks regeneration.
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP, sweep_starts_service=True)
        with contextlib.redirect_stdout(io.StringIO()):
            rc = membership.main(["--repo-root", str(self.root), "--write-snapshots"])
        self.assertEqual(rc, 0)
        self.assertTrue(manifest.exists())
        self.assertTrue(baseline.exists())
        original_baseline = baseline.read_text("utf-8")
        self.fx.write_source(
            "src/db/service.rs",
            "#[cfg(test)] mod tests { #[test] fn case() { create_test_database(); } }\n"
            "#[cfg(test)] mod more_tests { #[test] fn new_case() { create_test_database(); } }\n",
        )
        self.assertEqual(membership.main(["--repo-root", str(self.root), "--write-snapshots", "--manifest-only"]), 0)
        self.assertEqual(baseline.read_text("utf-8"), original_baseline)
        self.assertIn("db::service::more_tests::new_case", manifest.read_text("utf-8"))
        self.assertEqual(membership.main(["--repo-root", str(self.root), "--write-snapshots"]), 0)
        self.assertIn("db::service::more_tests::new_case", baseline.read_text("utf-8"))

    def test_four_step_allowlist_bypass_of_rule5_is_refused(self) -> None:
        """The reviewed attack in order: mutate, allowlist, regenerate, recheck."""
        # 1. a tree whose PR sweep starts the service owes nothing.
        self.write_pg_test()
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP, sweep_starts_service=True)
        self.assertEqual(self.fx.analysis().debts["rule5"], set())

        # 2. delete the service step. rule5 names the test it now strands.
        self.fx.write_pr(("src/db/**",), sweep=self.SWEEP)
        named = self.fx.analysis().debts["rule5"]
        self.assertEqual(named, {"db::service::tests::case"})

        # 3. feed every id rule5 just printed back to the allowlist, in the form
        #    `load_allowlist` accepts: any non-empty text after the `#`.
        (self.root / "scripts/pg_test_lane_allowlist.txt").write_text(
            "".join(f"test:{name}  # false positive, tracked in #9999\n" for name in sorted(named)),
            "utf-8",
        )
        analysis = self.fx.analysis()
        self.assertEqual(analysis.allowlist_count, len(named))
        self.assertEqual(analysis.debts["rule5"], named)

        # 4. regenerate the snapshots -- the step that used to clear the leftover
        #    rule2/rule3 stale entries and take the whole run to rc=0.
        with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(membership.main(["--repo-root", str(self.root), "--write-snapshots"]), 1)

        # The allowlist has emptied `active_tests`, so rule1-rule4 are clean and
        # the manifest matches: rule5 is the only thing left holding this red.
        self.assertEqual(analysis.debts["rule1"], set())
        self.assertEqual(analysis.debts["rule2"], set())
        self.assertEqual(analysis.debts["rule3"], set())
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(io.StringIO()):
            verdict = membership.check_analysis(
                analysis,
                {section: set() for section in membership.SECTIONS},
                {section: set() for section in membership.SECTIONS},
                membership.render_manifest(analysis.inventory),
                reference_label="fixture base",
                allowlist_label="fixture allowlist",
            )
        self.assertEqual(verdict, 1)
        self.assertIn("FAIL: [rule5]", stderr.getvalue())
        self.assertIn("allowlisting these ids leaves the count where it is", stderr.getvalue())
        # The message must describe the mechanism, not an execution order:
        # `active_tests` is built at the top of `analyze` and rule5 is built
        # below it, so "computed before allowlist filtering" was backwards.
        self.assertIn("reads the pre-filter inventory", stderr.getvalue())
        self.assertNotIn("computed before allowlist filtering", stderr.getvalue())

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

    def test_jobs_parser_variants_surface_rule4_by_set_equality(self) -> None:
        cases = {
            "jobs_comment.yml": "comment_job",
            "jobs_4space.yml": "four_space_job",
            "jobs_6space.yml": "six_space_job",
            "jobs_quoted.yml": "quoted_job",
        }
        workflow = self.root / ".github/workflows/ci-main.yml"

        def legacy_job_names(text: str) -> list[str]:
            jobs_key = re.search(r"^(?P<indent>[^\S\n]*)jobs:[^\S\n]*$", text, re.MULTILINE)
            if jobs_key is None:
                return []
            jobs_indent = len(jobs_key.group("indent"))
            return [
                match.group("name")
                for match in re.finditer(
                    r"^(?P<indent>[^\S\n]+)(?P<name>[A-Za-z0-9_-]+):[^\S\n]*$",
                    text[jobs_key.end():],
                    re.MULTILINE,
                )
                if len(match.group("indent")) == jobs_indent + 2
            ]

        for fixture, job_name in cases.items():
            with self.subTest(fixture=fixture):
                fixture_text = (FIXTURES / fixture).read_text("utf-8")
                self.assertEqual(legacy_job_names(fixture_text), [])
                workflow.write_text(fixture_text, "utf-8")
                self.assertEqual(
                    self.fx.analysis().debts["rule4"],
                    {f".github/workflows/ci-main.yml:{job_name}"},
                )

    def test_top_level_jobs_wins_over_nested_input_key(self) -> None:
        workflow = self.root / ".github/workflows/ci-main.yml"
        workflow.write_text((FIXTURES / "jobs_nested_input.yml").read_text("utf-8"), "utf-8")
        self.assertEqual(
            self.fx.analysis().debts["rule4"],
            {".github/workflows/ci-main.yml:pg_lane"},
        )

    def test_low_indent_block_scalar_line_does_not_hide_later_job(self) -> None:
        workflow = self.root / ".github/workflows/ci-main.yml"
        fixture_text = (FIXTURES / "jobs_block_scalar.yml").read_text("utf-8")
        loaded = yaml.safe_load(fixture_text)
        run_script = loaded["jobs"]["prepare"]["steps"][0]["run"]
        self.assertIn("EOF", run_script.splitlines())
        workflow.write_text(fixture_text, "utf-8")
        jobs = membership.parse_jobs(workflow, self.root)
        self.assertEqual([job.name for job in jobs], ["prepare", "pg_lane"])
        self.assertEqual(
            self.fx.analysis().debts["rule4"],
            {".github/workflows/ci-main.yml:pg_lane"},
        )

    def test_top_level_section_after_jobs_does_not_create_ghost_job(self) -> None:
        workflow = self.root / ".github/workflows/ci-main.yml"
        fixture_text = (FIXTURES / "jobs_followed_by_section.yml").read_text("utf-8")
        self.assertIsInstance(yaml.safe_load(fixture_text), dict)
        workflow.write_text(fixture_text, "utf-8")
        jobs = membership.parse_jobs(workflow, self.root)
        self.assertEqual([job.name for job in jobs], ["pg_lane"])
        self.assertNotIn("defaults", jobs[0].text)
        self.assertEqual(
            self.fx.analysis().debts["rule4"],
            {".github/workflows/ci-main.yml:pg_lane"},
        )

        with mock.patch.object(membership, "_jobs_section_end", return_value=len(fixture_text)):
            with self.assertRaises(AssertionError):
                self.assertEqual(
                    [job.name for job in membership.parse_jobs(workflow, self.root)],
                    ["pg_lane"],
                )

    def test_empty_jobs_shapes_are_configuration_errors(self) -> None:
        cases = {
            "comment": "jobs: # parsed, but empty\n",
            "quoted": "\"jobs\": # quoted and empty\n",
            "four-space": "jobs:\n    # indented comment, but no job key\n",
            "tab": "jobs:\n\t# tab-indented comment, but no job key\n",
        }
        workflow = self.root / ".github/workflows/empty.yml"
        empty = empty_debts()
        for shape, text in cases.items():
            with self.subTest(shape=shape):
                workflow.write_text(text, "utf-8")
                analysis = self.fx.analysis()
                findings = [
                    finding for finding in analysis.findings if finding.kind == "jobs-empty"
                ]
                self.assertEqual(len(findings), 1)
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    rc = membership.check_analysis(
                        analysis,
                        empty,
                        empty,
                        membership.render_manifest(analysis.inventory),
                        reference_label="fixture base",
                        allowlist_label="fixture allowlist",
                    )
                self.assertEqual(rc, 2)
                self.assertIn("FAIL: [jobs-empty]", stderr.getvalue())

    def test_unsupported_top_level_jobs_shapes_are_configuration_errors(self) -> None:
        payload = (
            "    steps:\n"
            "      - run: ./scripts/ci/postgres-service.sh start\n"
            "      - run: cargo test postgres_\n"
        )
        cases = {
            "flow-empty": "jobs: {}\n",
            "bom-nonempty": "\ufeffjobs:\n  bypass:\n" + payload,
            "flow-nonempty": (
                "jobs: {bypass: {steps: "
                "[{run: './scripts/ci/postgres-service.sh start'}, "
                "{run: 'cargo test postgres_'}]}}\n"
            ),
            "space-before-colon": "jobs :\n  bypass:\n" + payload,
        }
        workflow = self.root / ".github/workflows/unsupported.yml"
        empty = empty_debts()
        for shape, text in cases.items():
            with self.subTest(shape=shape):
                workflow.write_text(text, "utf-8")
                analysis = self.fx.analysis()
                findings = [
                    finding.kind for finding in analysis.findings
                    if finding.source == ".github/workflows/unsupported.yml"
                ]
                self.assertEqual(findings, ["jobs-empty"])
                self.assertEqual(membership.parse_jobs(workflow, self.root), [])
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    rc = membership.check_analysis(
                        analysis,
                        empty,
                        empty,
                        membership.render_manifest(analysis.inventory),
                        reference_label="fixture base",
                        allowlist_label="fixture allowlist",
                    )
                self.assertEqual(rc, 2)
                self.assertIn("FAIL: [jobs-empty]", stderr.getvalue())

    def test_supported_extended_job_headers_are_enumerated(self) -> None:
        cases = {
            "mixed-anchor": (
                "jobs:\n"
                "  build: &base\n"
                "    runs-on: ubuntu-latest\n"
                "  test:\n"
                "    runs-on: ubuntu-latest\n",
                ["build", "test"],
            ),
            "mixed-space-before-colon": (
                "jobs:\n"
                "  bypass :\n    steps:\n      - run: echo bypass\n"
                "  ordinary:\n    steps:\n      - run: echo ordinary\n",
                ["bypass", "ordinary"],
            ),
        }
        workflow = self.root / ".github/workflows/extended-job-headers.yml"
        empty = empty_debts()
        for shape, (text, expected_jobs) in cases.items():
            with self.subTest(shape=shape):
                loaded = yaml.safe_load(text)
                self.assertEqual(list(loaded["jobs"]), expected_jobs)
                workflow.write_text(text, "utf-8")
                analysis = self.fx.analysis()
                self.assertEqual(
                    [job.name for job in membership.parse_jobs(workflow, self.root)],
                    expected_jobs,
                )
                self.assertFalse(any(
                    finding.source == ".github/workflows/extended-job-headers.yml"
                    for finding in analysis.findings
                ))
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    rc = membership.check_analysis(
                        analysis,
                        empty,
                        empty,
                        membership.render_manifest(analysis.inventory),
                        reference_label="fixture base",
                        allowlist_label="fixture allowlist",
                    )
                self.assertEqual(rc, 0)
                self.assertNotIn("jobs-empty", stderr.getvalue())

    def test_top_level_jobs_anchor_is_enumerated(self) -> None:
        workflow = self.root / ".github/workflows/anchored-jobs-map.yml"
        text = (
            "name: anchored-jobs-map\n"
            "on: push\n"
            "jobs: &workflow_jobs\n"
            "  build:\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - run: echo build\n"
            "  test:\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - run: echo test\n"
        )
        expected_jobs = list(yaml.safe_load(text)["jobs"])
        workflow.write_text(text, "utf-8")
        analysis = self.fx.analysis()
        self.assertEqual(
            [job.name for job in membership.parse_jobs(workflow, self.root)],
            expected_jobs,
        )
        self.assertFalse(any(
            finding.source == ".github/workflows/anchored-jobs-map.yml"
            for finding in analysis.findings
        ))
        empty = empty_debts()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            rc = membership.check_analysis(
                analysis,
                empty,
                empty,
                membership.render_manifest(analysis.inventory),
                reference_label="fixture base",
                allowlist_label="fixture allowlist",
            )
        self.assertEqual(rc, 0)
        self.assertNotIn("jobs-empty", stderr.getvalue())

    def test_workflow_without_jobs_key_is_not_configuration_error(self) -> None:
        workflow = self.root / ".github/workflows/no-jobs.yml"
        workflow.write_text("on:\n  push:\n", "utf-8")
        analysis = self.fx.analysis()
        self.assertFalse(any(finding.kind == "jobs-empty" for finding in analysis.findings))
        empty = empty_debts()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            rc = membership.check_analysis(
                analysis,
                empty,
                empty,
                membership.render_manifest(analysis.inventory),
                reference_label="fixture base",
                allowlist_label="fixture allowlist",
            )
        self.assertEqual(rc, 0)
        self.assertNotIn("jobs-empty", stderr.getvalue())

    def test_non_top_level_jobs_text_is_not_configuration_error(self) -> None:
        cases = {
            "indented": (
                "  jobs:\n    bypass:\n      steps:\n        - run: echo ok\n",
                [],
            ),
            "nested-run": (
                "jobs:\n  lane:\n    steps:\n      - run: |\n"
                "          printf '%s\\n' 'jobs:'\n",
                ["lane"],
            ),
            "jobs-summary": ("jobs_summary:\n  bypass:\n", []),
            "jobs-prefix": ("jobsfoo:\n  bypass:\n", []),
            "comment": ("# jobs:\n#   bypass:\n", []),
            "quoted-and-block-scalars": (
                "name: \"jobs:\"\ndescription: |\n  jobs:\n    bypass:\n",
                [],
            ),
        }
        workflow = self.root / ".github/workflows/non-top-level.yml"
        empty = empty_debts()
        for shape, (text, expected_jobs) in cases.items():
            with self.subTest(shape=shape):
                workflow.write_text(text, "utf-8")
                analysis = self.fx.analysis()
                self.assertFalse(any(
                    finding.kind == "jobs-empty"
                    and finding.source == ".github/workflows/non-top-level.yml"
                    for finding in analysis.findings
                ))
                self.assertEqual(
                    [job.name for job in membership.parse_jobs(workflow, self.root)],
                    expected_jobs,
                )
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    rc = membership.check_analysis(
                        analysis,
                        empty,
                        empty,
                        membership.render_manifest(analysis.inventory),
                        reference_label="fixture base",
                        allowlist_label="fixture allowlist",
                    )
                self.assertEqual(rc, 0)
                self.assertNotIn("jobs-empty", stderr.getvalue())

    def test_jobs_comment_rule4_debt_fails_on_new_violation(self) -> None:
        workflow = self.root / ".github/workflows/ci-main.yml"
        workflow.write_text((FIXTURES / "jobs_comment.yml").read_text("utf-8"), "utf-8")
        analysis = self.fx.analysis()
        self.assertEqual(
            analysis.debts["rule4"],
            {".github/workflows/ci-main.yml:comment_job"},
        )
        empty = empty_debts()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            rc = membership.check_analysis(
                analysis,
                empty,
                empty,
                membership.render_manifest(analysis.inventory),
                reference_label="fixture base",
                allowlist_label="fixture allowlist",
            )
        self.assertEqual(rc, 1)
        self.assertIn("FAIL: [rule4] baseline drift: 1 new", stderr.getvalue())

    def test_three_hop_rule_debt_fails_on_new_violations(self) -> None:
        self.fx.write_source(
            "src/xfile_a.rs",
            (FIXTURES / "xfile_a.rs").read_text("utf-8"),
            "mod xfile_a;\nmod xfile_b;\n",
        )
        self.fx.write_source(
            "src/xfile_b.rs",
            (FIXTURES / "xfile_b.rs").read_text("utf-8"),
        )
        analysis = self.fx.analysis()
        self.assertEqual(
            {section: len(analysis.debts[section]) for section in ("rule1", "rule2", "rule3")},
            {"rule1": 1, "rule2": 1, "rule3": 1},
        )
        empty = empty_debts()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            rc = membership.check_analysis(
                analysis,
                empty,
                empty,
                membership.render_manifest(analysis.inventory),
                reference_label="fixture base",
                allowlist_label="fixture allowlist",
            )
        self.assertEqual(rc, 1)
        for section in ("rule1", "rule2", "rule3"):
            self.assertIn(f"FAIL: [{section}] baseline drift: 1 new", stderr.getvalue())

    def test_brace_cache_has_hits_and_preserves_counter_contract(self) -> None:
        membership._matching_brace_cached.cache_clear()
        clean = "{ nested { body(); } }"
        membership._matching_brace(clean, 0)
        membership._matching_brace(clean, 0)
        info = membership._matching_brace_cached.cache_info()
        self.assertEqual(info.hits, 1)
        self.assertEqual(info.misses, 1)

    def test_operation_counter_is_warn_only(self) -> None:
        analysis = self.fx.analysis()
        counters = [finding for finding in analysis.findings if finding.kind == "operation-counter"]
        self.assertEqual(len(counters), 1)
        self.assertRegex(counters[0].detail, r"_matching_brace calls=\d+ cache_hits=\d+ cache_misses=\d+")
        empty = empty_debts()
        synthetic = membership.Analysis(
            membership.PgInventory({}), empty, 0, tuple(counters)
        )
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            rc = membership.check_analysis(
                synthetic,
                empty,
                empty,
                membership.render_manifest(synthetic.inventory),
                reference_label="fixture base",
                allowlist_label="fixture allowlist",
            )
        self.assertEqual(rc, 0)
        self.assertIn("WARN: [operation-counter]", stderr.getvalue())

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


def empty_debts() -> dict[str, set[str]]:
    """Every debt key `check_analysis` reads, baselined or not."""
    return {
        section: set()
        for section in membership.SECTIONS + membership.UNBASELINED_SECTIONS
    }


class BaselineAndEnforcement(FixtureCase):
    def analysis(self, debts: dict[str, set[str]] | None = None):
        return membership.Analysis(
            membership.PgInventory({"service::postgres_tests::case": "src/db/service.rs"}),
            debts or empty_debts(),
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
        old = empty_debts()
        old["rule3"] = {"src/db/service.rs"}
        rc, _, _ = self.run_check(self.analysis(), empty_debts(), old)
        self.assertEqual(rc, 0)
        rc, _, error = self.run_check(self.analysis(), old, old)
        self.assertEqual(rc, 1)
        self.assertIn("FAIL: [rule3] baseline drift", error)
        self.assertIn("Remove '-' entries", error)

    def test_matching_nonempty_live_debt_emits_no_drift_diagnostic(self) -> None:
        debts = empty_debts()
        debts["rule3"] = {"src/db/service.rs"}
        rc, output, error = self.run_check(self.analysis(debts), debts, debts)
        self.assertEqual(rc, 0)
        self.assertIn("PG test-lane membership check passed", output)
        self.assertEqual(error, "")

    def test_new_violation_fails_even_with_no_baseline(self) -> None:
        empty = empty_debts()
        current = empty_debts()
        current["rule1"] = {"service::tests::case"}
        rc, _, error = self.run_check(self.analysis(current), empty, empty)
        self.assertEqual(rc, 1)
        self.assertIn("FAIL: [rule1] baseline drift", error)
        self.assertIn("Fix '+' violations", error)
        rc, _, error = self.run_check(self.analysis(current), current, empty)
        self.assertEqual(rc, 1)
        self.assertIn("baseline growth forbidden", error)

    def test_manifest_drift_fails_with_complete_recovery_command(self) -> None:
        empty = empty_debts()
        rc, _, error = self.run_check(self.analysis(), empty, empty, "")
        self.assertEqual(rc, 1)
        self.assertIn("python3 scripts/check_pg_test_lane_membership.py --write-snapshots --manifest-only", error)
        self.assertIn("only the manifest", error)
        self.assertIn("baseline-growth", error)

    def test_inventory_change_passes_when_manifest_matches(self) -> None:
        empty = empty_debts()
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


class BypassFixtureCase(FixtureCase):
    """A PG-dependent test plus a PR sweep job that can be reshaped per case."""

    SWEEP = "cargo test --lib -- --skip _pg --skip pg_ --skip postgres"
    CASE = "db::service::tests::case"

    def setUp(self) -> None:
        super().setUp()
        self.fx.write_source(
            "src/db/service.rs",
            "#[cfg(test)] mod tests { #[test] fn case() { create_test_database(); } }\n",
            "mod db { pub mod service; }\n",
        )

    def write_sweep(self, block: str) -> None:
        self.fx.write_pr(("src/db/**",), sweep_block=block)


class BlockScalarMutations(BypassFixtureCase):
    """R1: a `run:` block scalar must be read under every header YAML allows.

    Measured on the pre-fix function, `run: |` yielded the block's command and
    `|-`, `|+`, `>`, `>-`, `>+` each yielded nothing: the header fell through
    to the plain-scalar branch, `block_indent` stayed None, and the body was
    never scanned. `|-` is the ordinary way to write a block, so this was one
    routine edit away rather than an adversarial shape.
    """

    HEADERS = ("|", "|-", "|+", ">", ">-", ">+")

    def test_every_block_scalar_header_exposes_the_body(self) -> None:
        for header in (*self.HEADERS, "|2", "|2-", "|-2", ">2", "| # trailing"):
            with self.subTest(header=header):
                text = f"steps:\n  - run: {header}\n      cargo test --lib -- --skip _pg\n"
                self.assertEqual(
                    membership._cargo_commands(text),
                    ["cargo test --lib -- --skip _pg"],
                )

    def test_a_quoted_pipe_is_a_command_not_a_block_header(self) -> None:
        # The counterexample to reading the header too loosely: `run: "|"` is
        # a one-character command, not the start of a block.
        self.assertEqual(membership._cargo_commands('steps:\n  - run: "|"\n'), [])
        self.assertEqual(
            membership._block_scalar_style('"|"'), None
        )
        self.assertEqual(membership._block_scalar_style("|-"), "|")
        self.assertEqual(membership._block_scalar_style(">-"), ">")

    def test_folded_scalar_joins_lines_that_a_literal_one_keeps_apart(self) -> None:
        body = "      cargo test --lib\n      -- --skip _pg\n"
        self.assertEqual(
            membership._cargo_commands(f"steps:\n  - run: >-\n{body}"),
            ["cargo test --lib -- --skip _pg"],
        )
        # The same two lines under `|` are two shell lines, and the second
        # carries no `cargo test`, so only the unfiltered first one is a
        # command. Folding is what makes the extracted selection match the
        # selection the job actually runs.
        self.assertEqual(
            membership._cargo_commands(f"steps:\n  - run: |\n{body}"),
            ["cargo test --lib"],
        )

    def test_folded_scalar_keeps_a_more_indented_line_on_its_own(self) -> None:
        # YAML preserves the line breaks around a more-indented line inside a
        # folded scalar, so it is its own shell line rather than a fold.
        # Folding all three would yield one `cargo test --lib --release --
        # --skip _pg`; the real shell sees three lines, and only the first
        # carries a cargo invocation.
        text = (
            "steps:\n  - run: >\n"
            "      cargo test --lib\n"
            "        --release\n"
            "      -- --skip _pg\n"
        )
        self.assertEqual(membership._cargo_commands(text), ["cargo test --lib"])

    def test_a_blank_line_ends_a_folded_command(self) -> None:
        text = (
            "steps:\n  - run: >\n"
            "      cargo test --lib\n"
            "\n"
            "      -- --skip _pg\n"
        )
        self.assertEqual(membership._cargo_commands(text), ["cargo test --lib"])

    def test_rule5_names_the_test_under_every_block_header(self) -> None:
        for header in self.HEADERS:
            with self.subTest(header=header):
                self.write_sweep(
                    f"  sweep:\n    steps:\n      - run: {header}\n"
                    f"          {self.SWEEP}\n"
                )
                self.assertEqual(self.fx.analysis().debts["rule5"], {self.CASE})


class CommentMarkerMutations(BypassFixtureCase):
    """R2: a comment that merely mentions the service is not a service start."""

    def test_a_commented_service_marker_does_not_start_the_service(self) -> None:
        self.write_sweep(
            "  sweep:\n    steps:\n"
            "      # ./scripts/ci/postgres-service.sh start\n"
            f"      - run: {self.SWEEP}\n"
        )
        analysis = self.fx.analysis()
        self.assertEqual(analysis.debts["rule5"], {self.CASE})
        # And the same comment must not buy the job rule1 coverage either.
        self.assertEqual(analysis.debts["rule1"], {self.CASE})

    def test_a_real_service_step_still_reads_as_one(self) -> None:
        self.write_sweep(
            "  sweep:\n    env:\n      AGENTDESK_REQUIRE_PG: \"1\"\n    steps:\n"
            "      - run: ./scripts/ci/postgres-service.sh start\n"
            f"      - run: {self.SWEEP}\n"
        )
        analysis = self.fx.analysis()
        self.assertEqual(analysis.debts["rule5"], set())
        self.assertEqual(analysis.debts["rule4"], set())

    def test_a_commented_require_pg_env_does_not_satisfy_rule4(self) -> None:
        self.write_sweep(
            "  sweep:\n    env:\n      # AGENTDESK_REQUIRE_PG: \"1\"\n    steps:\n"
            "      - run: ./scripts/ci/postgres-service.sh start\n"
            f"      - run: {self.SWEEP}\n"
        )
        self.assertEqual(
            self.fx.analysis().debts["rule4"],
            {".github/workflows/ci-pr.yml:sweep"},
        )

    def test_a_hash_inside_a_quoted_scalar_is_not_a_comment(self) -> None:
        for line in (
            'run: echo "a # b"',
            "run: echo 'a # b'",
            'run: echo "it\'s a # b"',
            'name: "step # one"',
        ):
            with self.subTest(line=line):
                self.assertEqual(membership._strip_comments(line), line)
        self.assertEqual(membership._strip_comments("  - run: x  # note"), "  - run: x")
        self.assertEqual(membership._strip_comments("      # note"), "")
        self.assertEqual(membership._strip_comments("run: echo a#b"), "run: echo a#b")

    def test_a_quoted_hash_on_the_service_line_keeps_the_service(self) -> None:
        # The counterexample to cutting at every `#`: the service start here
        # sits after a quoted one, and must survive comment removal.
        self.write_sweep(
            "  sweep:\n    env:\n      AGENTDESK_REQUIRE_PG: \"1\"\n    steps:\n"
            '      - run: echo "lane # 1" && ./scripts/ci/postgres-service.sh start\n'
            f"      - run: {self.SWEEP}\n"
        )
        self.assertEqual(self.fx.analysis().debts["rule5"], set())


class ReusableWorkflowMutations(BypassFixtureCase):
    """R3: a job this gate cannot read is failed by name, not passed."""

    CALL = "  sweep:\n    uses: ./.github/workflows/library-sweep.yml\n    with:\n      lane: lib\n"

    def test_a_reusable_workflow_job_is_named_rather_than_passed(self) -> None:
        self.write_sweep(self.CALL)
        analysis = self.fx.analysis()
        # Nothing else moves: the called file is where the cargo command went,
        # so every rule reads this job as running nothing at all.
        self.assertEqual(analysis.debts["rule5"], set())
        self.assertEqual(
            [(finding.kind, finding.source) for finding in analysis.findings
             if finding.kind in membership.UNANALYZABLE_FINDINGS],
            [("pr-job-delegates-to-reusable-workflow", ".github/workflows/ci-pr.yml:sweep")],
        )
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(io.StringIO()):
            verdict = membership.check_analysis(
                analysis,
                analysis.debts,
                analysis.debts,
                membership.render_manifest(analysis.inventory),
                reference_label="fixture base",
                allowlist_label="fixture allowlist",
            )
        self.assertEqual(verdict, 1)
        self.assertIn("pr-job-delegates-to-reusable-workflow", stderr.getvalue())
        self.assertIn("library-sweep.yml", stderr.getvalue())
        self.assertIn("Unanalysable is not clean", stderr.getvalue())
        self.assertIn("move the cargo-test steps back", stderr.getvalue())

    def test_step_level_uses_is_not_a_reusable_workflow_call(self) -> None:
        # The shape every real ci-pr.yml job has. `- uses:` is a step, and a
        # step-level `uses:` sits below the job's own key indent, so neither
        # may be read as delegation.
        self.write_sweep(
            "  sweep:\n    steps:\n      - uses: actions/checkout@v4\n"
            "      - name: build\n        uses: dtolnay/rust-toolchain@master\n"
            "      - run: ./scripts/ci/postgres-service.sh start\n"
            f"      - run: {self.SWEEP}\n"
        )
        analysis = self.fx.analysis()
        self.assertEqual(membership._unanalyzable(analysis.findings), ())
        self.assertEqual(analysis.debts["rule5"], set())

    def test_write_snapshots_refuses_under_an_unreadable_job(self) -> None:
        self.write_sweep(self.CALL)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(io.StringIO()):
            verdict = membership.main(["--repo-root", str(self.root), "--write-snapshots"])
        self.assertEqual(verdict, 1)
        self.assertIn("refusing to write snapshots", stderr.getvalue())

    def test_the_shipped_pr_workflow_has_no_unreadable_job(self) -> None:
        # The count this fail-closed was added against: zero today, so it
        # changes no existing verdict.
        jobs = [
            job
            for path in sorted((REPO_ROOT / ".github/workflows").glob("*.yml"))
            for job in membership.parse_jobs(path, REPO_ROOT, [])
        ]
        self.assertEqual(membership.pr_reusable_workflow_jobs(jobs), ())


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
