"""Tests for the Rust test-lane coverage ratchet (#4846)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_test_lane_coverage.py"
AUTO_QUEUE_POSTGRES_TESTS = {
    "dispatch::dispatch_status::auto_queue_phase_gate_finalize_wrapper_tests::postgres_tests::finalize_and_patch_accept_ascii_control_whitespace_legacy_provenance",
    "dispatch::dispatch_status::auto_queue_phase_gate_finalize_wrapper_tests::postgres_tests::finalize_and_patch_reject_mixed_null_and_nonblank_legacy_provenance",
    "dispatch::dispatch_status::auto_queue_phase_gate_finalize_wrapper_tests::postgres_tests::finalize_and_patch_reject_nonblank_legacy_provenance",
    "dispatch::dispatch_status::auto_queue_phase_gate_finalize_wrapper_tests::postgres_tests::normal_finalize_infers_legacy_default_only_from_persisted_null_kind",
    "dispatch::dispatch_status::auto_queue_phase_gate_finalize_wrapper_tests::postgres_tests::patch_completion_reconstructs_legacy_default_and_clears_gate",
    "services::auto_queue::route::route_generate::deploy_gate_request_rejection_tests::postgres_tests::unavailable_deploy_gate_creates_no_database_rows",
}
_spec = importlib.util.spec_from_file_location("check_test_lane_coverage", SCRIPT)
assert _spec and _spec.loader
coverage = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = coverage
_spec.loader.exec_module(coverage)


class TestModuleScannerTests(unittest.TestCase):
    def test_discovers_inline_and_external_cfg_test_modules(self) -> None:
        source = r'''
            // #[cfg(test)] mod comment_fake;
            const FAKE: &str = "#[cfg(test)] mod string_fake;";
            mod outer {
                #[cfg(all(test, feature = "fixture"))]
                mod nested_tests { }
            }
            #[cfg(test)]
            pub(crate) mod tests;
        '''

        self.assertEqual(
            coverage.test_modules_in_source(source, ("services", "relay")),
            {
                "services::relay::outer::nested_tests",
                "services::relay::tests",
            },
        )

    def test_discovers_conventional_file_module_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src/services/foo").mkdir(parents=True)
            (root / "src/lib.rs").write_text(
                "#[cfg(test)] mod root_tests;\n", encoding="utf-8"
            )
            (root / "src/services/foo/mod.rs").write_text(
                "#[cfg(test)] mod tests;\n", encoding="utf-8"
            )
            (root / "src/services/foo/helper.rs").write_text(
                "#[cfg(test)] mod helper_tests {}\n", encoding="utf-8"
            )
            (root / "src/main.rs").write_text(
                "#[cfg(test)] mod ignored_binary_tests {}\n", encoding="utf-8"
            )

            self.assertEqual(
                coverage.discover_test_modules(root),
                {
                    "root_tests",
                    "services::foo::tests",
                    "services::foo::helper::helper_tests",
                },
            )

    def test_path_alias_uses_logical_module_path_for_nested_tests(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src/services/discord").mkdir(parents=True)
            (root / "src/lib.rs").write_text("", encoding="utf-8")
            (root / "src/services/discord/tmux.rs").write_text(
                '#[path = "tmux_watcher.rs"]\nmod watcher_alias;\n',
                encoding="utf-8",
            )
            (root / "src/services/discord/tmux_watcher.rs").write_text(
                "mod footer { #[cfg(test)] mod tests {} }\n", encoding="utf-8"
            )

            modules = coverage.discover_test_modules(root)

            self.assertEqual(
                modules,
                {"services::discord::tmux::watcher_alias::footer::tests"},
            )

    def test_nested_path_alias_chain_normalizes_to_logical_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src/physical/child").mkdir(parents=True)
            (root / "src/lib.rs").write_text(
                '#[path = "physical/parent.rs"] mod logical;\n', encoding="utf-8"
            )
            (root / "src/physical/parent.rs").write_text(
                '#[path = "child/leaf.rs"] mod nested;\n', encoding="utf-8"
            )
            (root / "src/physical/child/leaf.rs").write_text(
                "#[cfg(test)] mod tests {}\n", encoding="utf-8"
            )

            self.assertEqual(
                coverage.discover_test_modules(root), {"logical::nested::tests"}
            )

    def test_inline_parent_path_alias_matches_rustc_logical_path(self) -> None:
        """Regression fixture from the round-2 GPT review."""
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src/outer").mkdir(parents=True)
            (root / "src/lib.rs").write_text(
                'mod outer { #[path = "leaf.rs"] mod renamed; }\n',
                encoding="utf-8",
            )
            (root / "src/outer/leaf.rs").write_text(
                "#[cfg(test)] mod tests { #[test] fn visible() {} }\n",
                encoding="utf-8",
            )

            inventory = coverage.discover_test_inventory(root)

            self.assertEqual(
                inventory,
                {"outer::renamed::tests": {"outer::renamed::tests::visible"}},
            )
            self.assertNotIn("outer::leaf::tests", inventory)


class LaneFilterTests(unittest.TestCase):
    def test_parses_positive_skip_and_exact_filters(self) -> None:
        lane = coverage.cargo_test_filter(
            "cargo test --lib relay_recovery -- --skip postgres --exact"
        )
        self.assertEqual(
            lane,
            coverage.LaneFilter(("relay_recovery",), ("postgres",), True),
        )
        self.assertIsNone(
            coverage.cargo_test_filter(
                "cargo test --bin agentdesk high_risk_recovery:: -- --test-threads=1"
            )
        )

    def test_exact_skip_matching_agrees_with_libtest(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "exact_skip.rs"
            binary = root / "exact_skip"
            source.write_text("#[test] fn deploy_gate_case() {}\n", encoding="utf-8")
            subprocess.run(
                ["rustc", "--test", source, "-o", binary],
                check=True,
                capture_output=True,
                text=True,
            )

            cases = (
                ("deploy_gate", True, "1 passed"),
                ("deploy_gate_case", False, "0 passed"),
            )
            for skip, expected_selected, libtest_summary in cases:
                with self.subTest(skip=skip):
                    lane = coverage.LaneFilter(
                        ("deploy_gate_case",), (skip,), exact=True
                    )
                    result = subprocess.run(
                        [binary, "deploy_gate_case", "--exact", "--skip", skip],
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(
                        lane.selects_test("deploy_gate_case"), expected_selected
                    )
                    self.assertIn(libtest_summary, result.stdout)

    def test_single_test_filter_does_not_cover_parent_module(self) -> None:
        modules = {"service::tests", "other::tests"}
        lanes = (coverage.LaneFilter(("service::tests::one_case",), ()),)
        self.assertEqual(coverage.uncovered_modules(modules, lanes), modules)

    def test_discovers_main_push_recipe_after_pr_test_moves(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / ".github/workflows").mkdir(parents=True)
            (root / "justfile").write_text(
                "test-non-pg:\n    cargo test --lib retained_tests\n",
                encoding="utf-8",
            )
            (root / ".github/workflows/ci-main.yml").write_text(
                "run: just test-non-pg\n", encoding="utf-8"
            )
            (root / ".github/workflows/ci-pr.yml").write_text(
                "run: cargo check --workspace\n", encoding="utf-8"
            )

            self.assertEqual(
                coverage.discover_lane_filters(root),
                (coverage.LaneFilter(("retained_tests",), ()),),
            )

    def test_discovers_shared_non_pg_filter_without_treating_variable_as_positive(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / ".github/workflows").mkdir(parents=True)
            (root / "scripts/ci").mkdir(parents=True)
            (root / coverage.NON_PG_FILTER_REL).write_text(
                (REPO_ROOT / coverage.NON_PG_FILTER_REL).read_text(encoding="utf-8"),
                encoding="utf-8",
            )
            (root / "justfile").write_text(
                "test-non-pg:\n    cargo test --lib retained_tests\n",
                encoding="utf-8",
            )
            (root / ".github/workflows/ci-pr.yml").write_text(
                'run: cargo test --lib -- "${NON_PG_SKIP_ARGS[@]}"\n',
                encoding="utf-8",
            )

            args = coverage.load_non_pg_skip_args(root)
            lanes = coverage.discover_lane_filters(root)

            self.assertIn(
                coverage.LaneFilter((), tuple(args[1::2])),
                lanes,
            )

    def test_module_filter_covers_nested_module(self) -> None:
        modules = {"service::tests", "other::tests"}
        lanes = (coverage.LaneFilter(("service",), ()),)
        self.assertEqual(coverage.uncovered_modules(modules, lanes), {"other::tests"})

    def test_skip_matching_module_overrides_positive_filter(self) -> None:
        modules = {"alpha::tests", "alpha::_pg_tests"}
        lanes = (coverage.LaneFilter(("alpha",), ("_pg",)),)
        self.assertEqual(
            coverage.uncovered_modules(modules, lanes), {"alpha::_pg_tests"}
        )

    def test_skip_matching_any_test_name_makes_module_partially_covered(self) -> None:
        inventory = {
            "services::auto_queue::tests": {
                "services::auto_queue::tests::status_is_visible",
                "services::auto_queue::tests::auto_queue_status_query_uses_latest_review_clock_pg",
            }
        }
        lanes = (coverage.LaneFilter(("auto_queue",), ("_pg", "pg_", "postgres")),)

        self.assertEqual(
            coverage.uncovered_modules(inventory, lanes),
            {"services::auto_queue::tests"},
        )

    def test_repository_auto_queue_pg_test_keeps_module_uncovered(self) -> None:
        inventory = coverage.discover_test_inventory(REPO_ROOT)
        test_name = (
            "services::auto_queue::tests::"
            "auto_queue_status_query_uses_latest_review_clock_pg"
        )
        self.assertIn(test_name, inventory["services::auto_queue::tests"])
        lane = coverage.LaneFilter(
            ("auto_queue",), ("_pg", "pg_", "postgres")
        )
        self.assertFalse(
            lane.fully_selects(
                "services::auto_queue::tests",
                inventory["services::auto_queue::tests"],
            )
        )

    def test_auto_queue_postgres_authority_lane_owns_all_six_regressions(self) -> None:
        inventory = coverage.discover_test_inventory(REPO_ROOT)
        discovered = set().union(*inventory.values())
        self.assertEqual(AUTO_QUEUE_POSTGRES_TESTS & discovered, AUTO_QUEUE_POSTGRES_TESTS)

        non_pg = coverage.LaneFilter(
            ("auto_queue",), ("_pg", "pg_", "postgres")
        )
        postgres = coverage.LaneFilter(("_pg", "pg_", "postgres"), ())
        for test_name in AUTO_QUEUE_POSTGRES_TESTS:
            with self.subTest(test=test_name):
                self.assertFalse(non_pg.selects_test(test_name))
                self.assertTrue(postgres.selects_test(test_name))

    def test_auto_queue_postgres_authority_lane_inventory_match_is_fail_closed(self) -> None:
        def assert_expected_tests_exist(
            expected: set[str], discovered: set[str]
        ) -> None:
            missing = expected - discovered
            if missing:
                raise AssertionError(f"missing expected tests: {sorted(missing)}")

        inventory = coverage.discover_test_inventory(REPO_ROOT)
        discovered = set().union(*inventory.values())
        assert_expected_tests_exist(AUTO_QUEUE_POSTGRES_TESTS, discovered)
        with self.assertRaisesRegex(AssertionError, "missing_regression"):
            assert_expected_tests_exist(
                AUTO_QUEUE_POSTGRES_TESTS
                | {
                    "dispatch::dispatch_status::auto_queue_phase_gate_finalize_wrapper_tests::postgres_tests::missing_regression"
                },
                discovered,
            )


class RatchetTests(unittest.TestCase):
    def init_git_repo(self, root: Path) -> None:
        subprocess.run(["git", "init", "-q", "-b", "main", root], check=True)
        for key, value in (
            ("maintenance.auto", "false"),
            ("gc.auto", "0"),
            ("user.email", "tests@example.com"),
            ("user.name", "Tests"),
        ):
            subprocess.run(
                ["git", "-C", root, "config", "--local", key, value], check=True
            )

    def make_repo(self, root: Path, module_name: str) -> None:
        (root / "src").mkdir()
        (root / ".github/workflows").mkdir(parents=True)
        (root / "scripts").mkdir()
        (root / "src/lib.rs").write_text(
            f"#[cfg(test)] mod {module_name} {{}}\n", encoding="utf-8"
        )
        (root / "justfile").write_text(
            "test-non-pg:\n    cargo test --lib covered_tests\n", encoding="utf-8"
        )
        (root / ".github/workflows/ci-main.yml").write_text(
            "run: just test-non-pg\n", encoding="utf-8"
        )
        (root / ".github/workflows/ci-pr.yml").write_text(
            "run: cargo test --lib targeted_tests\n", encoding="utf-8"
        )

    def run_check(
        self,
        root: Path,
        baseline_entries: str,
        reference_entries: set[str],
    ) -> tuple[int, str]:
        baseline = root / "scripts/test_lane_coverage_baseline.txt"
        baseline.write_text(baseline_entries, encoding="utf-8")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = coverage.check(
                root,
                baseline,
                reference_entries,
                reference_label="fixture base",
                emit_success=False,
            )
        return result, stderr.getvalue()

    def test_new_uncovered_module_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "new_tests")
            result, stderr = self.run_check(root, "", set())
            self.assertEqual(result, 1)
            self.assertIn("+ new_tests", stderr)

    def test_baseline_growth_fails_even_if_it_contains_new_module(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "new_tests")
            result, stderr = self.run_check(root, "new_tests\n", set())
            self.assertEqual(result, 1)
            self.assertIn("baseline growth forbidden", stderr)
            self.assertIn("+ new_tests", stderr)

    def test_parallel_disjoint_removals_compose_without_conflict(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "legacy_c")
            result, stderr = self.run_check(
                root,
                "legacy_c\n",
                {"legacy_a", "legacy_c"},
            )
            self.assertEqual(result, 0)
            self.assertEqual(stderr, "")

    def test_stale_baseline_entry_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "covered_tests")
            result, stderr = self.run_check(
                root, "covered_tests\n", {"covered_tests"}
            )
            self.assertEqual(result, 1)
            self.assertIn("1 stale/covered", stderr)
            self.assertIn("- covered_tests", stderr)

    def test_baselined_uncovered_debt_passes(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "legacy_tests")
            result, stderr = self.run_check(
                root, "legacy_tests\n", {"legacy_tests"}
            )
            self.assertEqual(result, 0)
            self.assertEqual(stderr, "")

    def test_repository_inventory_uses_logical_footer_path(self) -> None:
        # This asserted the same property against the debt baseline until
        # #5185's library sweep covered the footer module and removed its
        # entry. Anchoring on the inventory instead states the real contract --
        # `#[path]` aliases resolve to logical module paths -- and keeps
        # holding as the baseline shrinks toward empty, which is the direction
        # the ratchet exists to force.
        inventory = coverage.discover_test_inventory(REPO_ROOT)
        self.assertIn(
            "services::discord::tmux::tmux_watcher::single_message_footer::tests",
            inventory,
        )
        self.assertNotIn(
            "services::discord::tmux_watcher::single_message_footer::tests",
            inventory,
        )

    def test_candidate_merge_first_parent_composes_parallel_removals(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "legacy_01")
            baseline = root / coverage.BASELINE_REL
            (root / "src/lib.rs").write_text(
                "#[cfg(test)] mod legacy_09 {}\n",
                encoding="utf-8",
            )
            self.init_git_repo(root)

            baseline.write_text(
                "".join(f"legacy_{index:02d}\n" for index in range(1, 10)),
                encoding="utf-8",
            )
            subprocess.run(["git", "-C", root, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", root, "commit", "-q", "-m", "common base"],
                check=True,
            )
            common_sha = subprocess.check_output(
                ["git", "-C", root, "rev-parse", "HEAD"], text=True
            ).strip()

            subprocess.run(["git", "-C", root, "switch", "-q", "-c", "pr"], check=True)
            baseline.write_text(
                "".join(
                    f"legacy_{index:02d}\n"
                    for index in range(1, 10)
                    if index != 8
                ),
                encoding="utf-8",
            )
            subprocess.run(["git", "-C", root, "commit", "-qam", "remove b"], check=True)
            pr_sha = subprocess.check_output(
                ["git", "-C", root, "rev-parse", "HEAD"], text=True
            ).strip()

            subprocess.run(
                ["git", "-C", root, "switch", "-q", "--detach", common_sha], check=True
            )
            baseline.write_text(
                "".join(
                    f"legacy_{index:02d}\n"
                    for index in range(1, 10)
                    if index != 2
                ),
                encoding="utf-8",
            )
            subprocess.run(["git", "-C", root, "commit", "-qam", "remove a"], check=True)
            main_sha = subprocess.check_output(
                ["git", "-C", root, "rev-parse", "HEAD"], text=True
            ).strip()
            subprocess.run(
                [
                    "git",
                    "-C",
                    root,
                    "merge",
                    "-q",
                    "--no-ff",
                    pr_sha,
                    "-m",
                    "candidate",
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            candidate_sha = subprocess.check_output(
                ["git", "-C", root, "rev-parse", "HEAD"], text=True
            ).strip()

            resolved, reference = coverage.load_baseline_from_git(root, "HEAD^1")
            self.assertEqual(resolved, main_sha)
            self.assertEqual(
                reference,
                {f"legacy_{index:02d}" for index in range(1, 10) if index != 2},
            )
            self.assertEqual(
                coverage.load_baseline(baseline),
                {
                    f"legacy_{index:02d}"
                    for index in range(1, 10)
                    if index not in {2, 8}
                },
            )
            candidate = coverage.load_baseline(baseline)
            self.assertEqual(coverage.baseline_growth(candidate, reference), [])
            self.assertEqual(reference - candidate, {"legacy_08"})

            subprocess.run(
                [
                    "git",
                    "-C",
                    root,
                    "update-ref",
                    "refs/remotes/origin/main",
                    pr_sha,
                ],
                check=True,
            )
            self.assertEqual(
                coverage.load_baseline_from_git(root, "HEAD^1"),
                (
                    main_sha,
                    {
                        f"legacy_{index:02d}"
                        for index in range(1, 10)
                        if index != 2
                    },
                ),
            )
            self.assertEqual(
                subprocess.check_output(
                    ["git", "-C", root, "rev-parse", "HEAD"], text=True
                ).strip(),
                candidate_sha,
            )

    def test_main_push_before_sha_reads_pre_push_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            baseline = root / coverage.BASELINE_REL
            baseline.parent.mkdir(parents=True)
            self.init_git_repo(root)
            baseline.write_text("legacy_a\n", encoding="utf-8")
            subprocess.run(["git", "-C", root, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", root, "commit", "-q", "-m", "before"], check=True
            )
            before = subprocess.check_output(
                ["git", "-C", root, "rev-parse", "HEAD"], text=True
            ).strip()

            baseline.write_text("legacy_a\nlegacy_b\n", encoding="utf-8")
            subprocess.run(
                ["git", "-C", root, "commit", "-qam", "intermediate growth"],
                check=True,
            )
            baseline.write_text("legacy_b\n", encoding="utf-8")
            subprocess.run(
                ["git", "-C", root, "commit", "-qam", "tip changes growth"],
                check=True,
            )

            _, reference = coverage.load_baseline_from_git(root, before)
            _, previous_commit = coverage.load_baseline_from_git(root, "HEAD^1")
            self.assertEqual(reference, {"legacy_a"})
            self.assertEqual(previous_commit, {"legacy_a", "legacy_b"})
            self.assertEqual(
                coverage.baseline_growth(coverage.load_baseline(baseline), reference),
                ["legacy_b"],
            )
            self.assertEqual(
                coverage.baseline_growth(
                    coverage.load_baseline(baseline), previous_commit
                ),
                [],
            )

    def test_main_forwards_explicit_baseline_ref_to_git_loader(self) -> None:
        reference = {"legacy_a"}
        with mock.patch.object(
            coverage,
            "load_baseline_from_git",
            return_value=("a" * 40, reference),
        ) as load_reference, mock.patch.object(
            coverage, "check", return_value=0
        ) as check:
            result = coverage.main(
                [
                    "--repo-root",
                    str(REPO_ROOT),
                    "--baseline-ref",
                    "immutable-before",
                ]
            )

        self.assertEqual(result, 0)
        load_reference.assert_called_once_with(REPO_ROOT.resolve(), "immutable-before")
        check.assert_called_once()
        self.assertEqual(check.call_args.args[2], reference)
        self.assertEqual(check.call_args.kwargs["reference_label"], f"commit {'a' * 40}")

    def test_cli_explicit_reference_catches_growth_that_self_compare_misses(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "legacy_b")
            baseline = root / coverage.BASELINE_REL
            self.init_git_repo(root)
            baseline.write_text("legacy_a\n", encoding="utf-8")
            subprocess.run(["git", "-C", root, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", root, "commit", "-q", "-m", "trusted"], check=True
            )
            trusted = subprocess.check_output(
                ["git", "-C", root, "rev-parse", "HEAD"], text=True
            ).strip()
            baseline.write_text("legacy_b\n", encoding="utf-8")
            subprocess.run(
                ["git", "-C", root, "commit", "-qam", "candidate growth"],
                check=True,
            )

            explicit = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--repo-root",
                    str(root),
                    "--baseline-ref",
                    trusted,
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            self_compare = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--repo-root",
                    str(root),
                    "--baseline-ref",
                    "HEAD",
                ],
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(explicit.returncode, 1)
        self.assertIn("baseline growth forbidden", explicit.stderr)
        self.assertEqual(self_compare.returncode, 0)
        self.assertNotIn("baseline growth forbidden", self_compare.stderr)

    def test_cli_requires_explicit_baseline_ref(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--repo-root",
                str(REPO_ROOT),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("--baseline-ref", result.stderr)

    def test_cli_missing_reference_exits_two(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--repo-root",
                str(REPO_ROOT),
                "--baseline-ref",
                "0" * 40,
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid baseline reference", result.stderr)

    def test_missing_zero_or_shallow_reference_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            subprocess.run(
                ["git", "init", "-q", "-b", "main", root], check=True
            )
            for ref in ("", "0" * 40, "missing-parent"):
                with self.subTest(ref=ref), self.assertRaises(ValueError):
                    coverage.load_baseline_from_git(root, ref)

    def test_reference_without_baseline_blob_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.init_git_repo(root)
            (root / "placeholder").write_text("x", encoding="utf-8")
            subprocess.run(["git", "-C", root, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", root, "commit", "-q", "-m", "no baseline"],
                check=True,
            )
            with self.assertRaises(ValueError):
                coverage.load_baseline_from_git(root, "HEAD")

    def test_repository_uses_semantic_baseline_without_scalar(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        baseline_text = (REPO_ROOT / coverage.BASELINE_REL).read_text(encoding="utf-8")
        self.assertNotIn("BASELINE_ENTRY_COUNT", source)
        self.assertNotIn("BASELINE_ENTRY_COUNT", baseline_text)
        self.assertEqual(
            coverage.load_baseline(REPO_ROOT / coverage.BASELINE_REL),
            coverage.parse_baseline(baseline_text, "repository baseline"),
        )

    def test_ci_script_checks_wires_guard_and_tests(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            'scripts/check_test_lane_coverage.py --baseline-ref "$TEST_LANE_BASELINE_REF"',
            script,
        )
        self.assertNotIn("TEST_LANE_BASELINE_REF:-HEAD", script)
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_test_lane_coverage', script
        )


if __name__ == "__main__":
    unittest.main()
