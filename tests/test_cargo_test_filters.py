"""A `cargo test` name filter in CI Main, Nightly or the Windows warm can select nothing and stay green."""
from __future__ import annotations

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts/ci"))
import cargo_test_filters as F


class CargoTestFilters(unittest.TestCase):
    def findings(self, workflow: str, old: str, new: str) -> list[str]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for relative in F.WORKFLOWS:
                (root / relative).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / relative, root / relative)
            path = root / workflow
            text = path.read_text(encoding="utf-8")
            self.assertIn(old, text)
            path.write_text(text.replace(old, new, 1), encoding="utf-8")
            return F.findings(root)

    def test_checked_in_workflows_run_whole_targets(self) -> None:
        self.assertEqual(F.findings(ROOT), [])

    def test_a_name_filter_or_expansion_in_any_of_the_three_workflows_fails(self) -> None:
        main_cmd = "cargo test --bins --test '*' -- --skip _pg --skip pg_ --skip postgres\n"
        nightly_cmd = "cargo test --bins --examples --benches --test '*' -- --skip _pg --skip pg_ --skip postgres\n"
        cases = (
            ("main filter before --", ".github/workflows/ci-main.yml", main_cmd,
             main_cmd.replace("--test '*' --", "--test '*' transition --"), "transition"),
            ("main filter in front", ".github/workflows/ci-main.yml", main_cmd,
             main_cmd.replace("cargo test --bins", "cargo test invariant --bins"), "invariant"),
            ("main filter after --", ".github/workflows/ci-main.yml", main_cmd,
             main_cmd.replace("--skip postgres", "--skip postgres routines"), "routines"),
            ("doctest filter changed", ".github/workflows/ci-main.yml", "cargo test --doc ClaudeBinary\n",
             "cargo test --doc ClaudeBinaryX\n", "ClaudeBinaryX"),
            ("nightly skip array", ".github/workflows/ci-nightly.yml", "\n          " + nightly_cmd,
             "\n          " + nightly_cmd.replace("--skip _pg --skip pg_ --skip postgres", '"${SKIP_ARGS[@]}"'),
             "SKIP_ARGS"),
        )
        for label, workflow, old, new, word in cases:
            with self.subTest(label=label):
                errors = self.findings(workflow, old, new)
                self.assertEqual(len(errors), 1, errors)
                self.assertIn(workflow, errors[0])
                self.assertIn(word, errors[0])

    def test_values_libtest_options_and_the_allowed_doctest_are_not_filters(self) -> None:
        for script in ("cargo test --test e2e -- --test-threads=1 2>&1 | tee log",
                       "env -u AGENTDESK_ROOT_DIR cargo test --lib --no-run  # build only",
                       "cargo test --bins --test '*' --features x -- --skip a --test-threads 1 --exact",
                       "cargo test --doc ClaudeBinary"):
            with self.subTest(script=script):
                words = F.commands(script)
                self.assertEqual(len(words), 1)
                self.assertTrue(" ".join(words[0]) in F.ALLOWED or not F.filters(words[0]), words)
        self.assertEqual(F.commands("echo 'cargo test x'"), [])
        self.assertEqual([F.filters(w) for w in F.commands("cargo test --bins \\\n  -- extra\ncargo test x")],
                         [["extra"], ["x"]])


if __name__ == "__main__":
    unittest.main()
