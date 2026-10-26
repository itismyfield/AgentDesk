"""Contracts for scripts/run_test_lane.py in the plan mode the PostgreSQL 15 owner proof uses."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
RUN_TEST_LANE = REPO_ROOT / "scripts/run_test_lane.py"


def transcript(passed=(), ignored=(), summary=None) -> str:
    """A libtest run reporting `passed` and `ignored`; `summary` overrides its counts."""
    lines = [f"test {test_id} ... ok" for test_id in passed]
    lines += [f"test {test_id} ... ignored" for test_id in ignored]
    ok, skipped = summary or (len(passed), len(ignored))
    return (f"running {len(lines)} tests\n" + "".join(line + "\n" for line in lines)
            + f"\ntest result: ok. {ok} passed; 0 failed; {skipped} ignored; "
            "0 measured; 9 filtered out; finished in 0.00s\n")


def ledger_rows(lane: str, ids) -> str:
    return "".join(f"ignored | {test_id} | lane={lane} | #1 | a dormant fixture that this lane "
                   "keeps ignored until its backing service exists in CI\n" for test_id in ids)



class ExpectedIdsTests(unittest.TestCase):
    """run_test_lane --expected-ids: the run must report exactly the planned ids."""

    def run_lane(self, expected: list[str], reported: list[str], ledger: str = "",
                 lane: str = "demo", ignored=(), summary=None) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as directory:
            ids = Path(directory) / "ids"
            ids.write_text("\n".join(expected) + "\n")
            ledger_path = Path(directory) / "ledger"
            ledger_path.write_text(ledger)
            text = transcript(reported, ignored, summary)
            result = subprocess.run(
                [sys.executable, str(RUN_TEST_LANE), "--lane", lane, "--expected-ids",
                 str(ids), "--ledger", str(ledger_path), "--", sys.executable, "-c",
                 f"import sys; sys.stdout.write({text!r})"], capture_output=True, text=True)
            return result.returncode, result.stderr

    def test_an_empty_plan_or_a_missing_id_is_red_and_the_exact_set_is_green(self) -> None:
        self.assertEqual(self.run_lane(["a::b::c"], ["a::b::c"])[0], 0)
        rc, stderr = self.run_lane([], [])
        self.assertEqual(rc, 1)
        self.assertIn("derived expectation is empty", stderr)
        rc, stderr = self.run_lane(["a::b::c", "a::b::d"], ["a::b::c"])
        self.assertEqual(rc, 1)
        self.assertIn("a::b::d", stderr)

    def test_a_pg_run_of_zero_tests_or_of_undeclared_ignores_is_red(self) -> None:
        planned, run, held = ["a::b::c_pg", "a::b::d_pg"], ["a::b::c_pg"], ["a::b::d_pg"]
        declared = ledger_rows("pg", held)
        self.assertEqual(self.run_lane(planned[:1], run, lane="pg")[0], 0)
        self.assertEqual(self.run_lane(planned[:1], [], lane="pg")[0], 1)
        self.assertEqual(self.run_lane(planned, run, lane="pg", ignored=held)[0], 1)
        self.assertEqual(self.run_lane(planned, run, declared, lane="pg", ignored=held)[0], 0)
        self.assertEqual(self.run_lane(planned, run, declared, lane="pg", ignored=held,
                                       summary=(2, 0))[0], 1)
        self.assertEqual(self.run_lane(planned, [], declared, lane="pg", ignored=held)[0], 1)
        # Ignores the ledger declares still leave a plan that executed nothing.
        rc, stderr = self.run_lane(held, [], declared, lane="pg", ignored=held)
        self.assertEqual(rc, 1)
        self.assertIn("no planned test executed", stderr)

    def test_declared_ignored_ids_outside_the_plan_are_not_stale(self) -> None:
        ledger = ("ignored | x::not::planned | lane=demo | #1 | a dormant fixture that "
                  "this lane keeps ignored until its backing service exists in CI\n")
        rc, stderr = self.run_lane(["a::b::c"], ["a::b::c"], ledger)
        self.assertEqual(rc, 0, stderr)


LIBTEST_CRATE = {
    "Cargo.toml": '[package]\nname = "lanecheck"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/lib.rs": "#[cfg(test)]\nmod tests {\n    #[test]\n    fn runs_pg() {}\n\n"
                  "    #[test]\n    #[ignore]\n    fn held_pg() {}\n}\n",
}


@unittest.skipUnless(shutil.which("cargo"), "cargo is not installed")
class LibtestIntegrationTests(unittest.TestCase):
    """Real libtest output, one passing and one #[ignore] test, through run_test_lane."""

    def test_a_real_ignored_id_is_red_until_the_pg_ledger_declares_it(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory) / "crate"
            for name, text in LIBTEST_CRATE.items():
                (crate / name).parent.mkdir(parents=True, exist_ok=True)
                (crate / name).write_text(text)
            ids, ledger = Path(directory) / "ids", Path(directory) / "ledger"
            planned = ["tests::runs_pg", "tests::held_pg"]
            ids.write_text("\n".join(planned) + "\n")
            # No compiler wrapper (empty disables one a cargo config names) and a private target.
            env = {**os.environ, "RUSTC_WRAPPER": "", "CARGO_BUILD_RUSTC_WRAPPER": "",
                   "CARGO_TARGET_DIR": str(Path(directory) / "target")}

            def lane(rows: str) -> subprocess.CompletedProcess:
                ledger.write_text(rows)
                return subprocess.run(
                    [sys.executable, str(RUN_TEST_LANE), "--lane", "pg", "--expected-ids", str(ids),
                     "--ledger", str(ledger), "--", "cargo", "test", "--lib", "--",
                     "--exact", *planned, "--test-threads=1"],
                    cwd=crate, env=env, capture_output=True, text=True)

            undeclared = lane("")
            self.assertEqual(undeclared.returncode, 1, undeclared.stdout + undeclared.stderr)
            self.assertIn("undeclared-ignored: tests::held_pg", undeclared.stdout + undeclared.stderr)
            self.assertIn("1 passed; 0 failed; 1 ignored", undeclared.stdout)
            declared = lane(ledger_rows("pg", planned[1:]))
            self.assertEqual(declared.returncode, 0, declared.stdout + declared.stderr)
            self.assertIn("1 passed; 0 failed; 1 ignored", declared.stdout)
            self.assertIn("executed=1", declared.stdout)


if __name__ == "__main__":
    unittest.main()
