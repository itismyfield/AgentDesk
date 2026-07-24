"""Tests for the Rust test-lane coverage ratchet (#4846)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_test_lane_coverage.py"
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

    def test_single_test_filter_does_not_cover_parent_module(self) -> None:
        modules = {"service::tests", "other::tests"}
        lanes = (coverage.LaneFilter(("service::tests::one_case",), ()),)
        self.assertEqual(coverage.uncovered_modules(modules, lanes), modules)

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


class RatchetTests(unittest.TestCase):
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
        (root / ".github/workflows/ci-pr.yml").write_text(
            "run: cargo test --lib targeted_tests\n", encoding="utf-8"
        )

    def run_check(
        self, root: Path, baseline_entries: str, expected_count: int
    ) -> tuple[int, str]:
        baseline = root / "scripts/test_lane_coverage_baseline.txt"
        baseline.write_text(baseline_entries, encoding="utf-8")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = coverage.check(
                root, baseline, expected_count, emit_success=False
            )
        return result, stderr.getvalue()

    def test_new_uncovered_module_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "new_tests")
            result, stderr = self.run_check(root, "", 0)
            self.assertEqual(result, 1)
            self.assertIn("+ new_tests", stderr)

    def test_baseline_growth_fails_even_if_it_contains_new_module(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "new_tests")
            result, stderr = self.run_check(root, "new_tests\n", 0)
            self.assertEqual(result, 1)
            self.assertIn("baseline growth", stderr)

    def test_stale_baseline_entry_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "covered_tests")
            result, stderr = self.run_check(root, "covered_tests\n", 1)
            self.assertEqual(result, 1)
            self.assertIn("1 stale/covered", stderr)
            self.assertIn("- covered_tests", stderr)

    def test_baselined_uncovered_debt_passes(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "legacy_tests")
            result, stderr = self.run_check(root, "legacy_tests\n", 1)
            self.assertEqual(result, 0)
            self.assertEqual(stderr, "")

    def test_repository_baseline_contains_logical_footer_path(self) -> None:
        baseline = coverage.load_baseline(REPO_ROOT / coverage.BASELINE_REL)
        self.assertIn(
            "services::discord::tmux::tmux_watcher::single_message_footer::tests",
            baseline,
        )
        self.assertNotIn(
            "services::discord::tmux_watcher::single_message_footer::tests",
            baseline,
        )

    def test_repository_baseline_count_matches_locked_constant(self) -> None:
        baseline = coverage.load_baseline(REPO_ROOT / coverage.BASELINE_REL)
        self.assertEqual(len(baseline), coverage.BASELINE_ENTRY_COUNT)

    def test_ci_script_checks_wires_guard_and_tests(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn('"$PYTHON" scripts/check_test_lane_coverage.py', script)
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_test_lane_coverage', script
        )


if __name__ == "__main__":
    unittest.main()
