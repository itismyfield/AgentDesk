"""Tests for the Rust test-lane coverage ratchet (#4846)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_test_lane_coverage.py"
_spec = importlib.util.spec_from_file_location("check_test_lane_coverage", SCRIPT)
assert _spec and _spec.loader
coverage = importlib.util.module_from_spec(_spec)
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


class LaneFilterTests(unittest.TestCase):
    def test_parses_only_positive_libtest_filters(self) -> None:
        command = (
            "cargo test --lib relay_recovery -- --skip postgres "
            "--test-threads=1"
        )
        self.assertEqual(coverage.cargo_test_filters(command), {"relay_recovery"})
        self.assertEqual(
            coverage.cargo_test_filters(
                "cargo test --bin agentdesk high_risk_recovery:: -- --test-threads=1"
            ),
            set(),
        )

    def test_exact_test_filter_marks_its_module_selected(self) -> None:
        modules = {"service::tests", "other::tests"}
        filters = {"service::tests::one_case"}
        self.assertEqual(coverage.uncovered_modules(modules, filters), {"other::tests"})


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

    def test_new_uncovered_module_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "new_tests")
            baseline = root / "scripts/test_lane_coverage_baseline.txt"
            baseline.write_text("", encoding="utf-8")
            stderr = io.StringIO()

            with contextlib.redirect_stderr(stderr):
                result = coverage.check(root, baseline)

            self.assertEqual(result, 1)
            self.assertIn("+ new_tests", stderr.getvalue())

    def test_baselined_debt_passes_and_stale_entry_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_repo(root, "covered_tests")
            baseline = root / "scripts/test_lane_coverage_baseline.txt"
            baseline.write_text("covered_tests\n", encoding="utf-8")
            stdout = io.StringIO()

            with contextlib.redirect_stdout(stdout):
                result = coverage.check(root, baseline)

            self.assertEqual(result, 0)
            self.assertIn("baseline module(s) are now covered", stdout.getvalue())

    def test_repository_baseline_contains_footer_regression_module(self) -> None:
        baseline = coverage.load_baseline(
            REPO_ROOT / "scripts/test_lane_coverage_baseline.txt"
        )
        self.assertIn(
            "services::discord::turn_bridge::single_message_footer::tests", baseline
        )

    def test_ci_script_checks_wires_guard_and_tests(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            '"$PYTHON" scripts/check_test_lane_coverage.py', script
        )
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_test_lane_coverage', script
        )


if __name__ == "__main__":
    unittest.main()
