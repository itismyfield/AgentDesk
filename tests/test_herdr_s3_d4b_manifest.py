"""CLI-boundary tests for the D4-B acceptance manifest gate.

Each negative takes a complete synthetic manifest and results set and applies one regression the
gate must see: a deleted required row, a missing slow or case result, a TIMEOUT reported as a kill,
or a blocked row with passing results.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_herdr_s3_d4b_manifest.py"
MANIFEST = ROOT / "scripts/herdr_s3_d4b_manifest.json"
TEST = "a::b::t_pg"


def complete() -> dict:
    case = lambda cid, profile, piece: {
        "id": cid, "profile": profile, "piece": piece, "status": "implemented", "tests": [TEST]
    }
    return {
        "required_cases": ["N", "S", "A"],
        "required_mutants": ["m_now", "m_slow"],
        "cases": [case("N", "now", 1), case("S", "slow", 5), case("A", "after", 6)],
        "mutants": [{"name": "m_now", "cases": ["N"]}, {"name": "m_slow", "cases": ["S"]}],
    }


def passing() -> dict:
    return {
        "cases": {"N": "PASS", "S": "PASS", "A": "PASS"},
        "mutants": {"m_now": "KILLED", "m_slow": "KILLED"},
    }


class ManifestGateTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.dir = Path(temp.name)
        (self.dir / "inventory.txt").write_text(f"# generated\n{TEST}\n")

    def run_gate(self, manifest: dict, results: dict | None, require: str = "full") -> tuple[int, str]:
        (self.dir / "manifest.json").write_text(json.dumps(manifest))
        args = [sys.executable, str(SCRIPT), "--manifest", str(self.dir / "manifest.json"),
                "--inventory", str(self.dir / "inventory.txt"), "--require", require]
        if results is not None:
            (self.dir / "results.json").write_text(json.dumps(results))
            args += ["--results", str(self.dir / "results.json")]
        done = subprocess.run(args, capture_output=True, text=True, check=False)
        return done.returncode, done.stdout + done.stderr

    def test_complete_evidence_passes_now_and_full(self) -> None:
        for require in ("now", "full"):
            self.assertEqual(self.run_gate(complete(), passing(), require)[0], 0, require)

    def test_deleted_required_row_fails(self) -> None:
        manifest = complete()
        manifest["cases"] = [case for case in manifest["cases"] if case["id"] != "S"]
        rc, out = self.run_gate(manifest, None)
        self.assertEqual(rc, 1)
        self.assertIn("required case S has no row", out)

    def test_missing_slow_result_fails_full_only(self) -> None:
        results = passing()
        del results["cases"]["S"]
        rc, out = self.run_gate(complete(), results)
        self.assertEqual(rc, 1)
        self.assertIn("S: result missing", out)
        self.assertEqual(self.run_gate(complete(), results, "now")[0], 0)

    def test_missing_case_result_fails_now_and_full(self) -> None:
        results = passing()
        del results["cases"]["N"]
        for require in ("now", "full"):
            self.assertEqual(self.run_gate(complete(), results, require)[0], 1, require)

    def test_timeout_is_not_a_kill(self) -> None:
        results = passing()
        results["mutants"]["m_now"] = "TIMEOUT"
        rc, out = self.run_gate(complete(), results)
        self.assertEqual(rc, 1)
        self.assertIn("mutant m_now: TIMEOUT", out)

    def test_blocked_row_fails_full_even_with_passing_results(self) -> None:
        manifest = complete()
        manifest["cases"][2]["status"] = "blocked:feature"
        rc, out = self.run_gate(manifest, passing())
        self.assertEqual(rc, 1)
        self.assertIn("A: blocked:feature", out)
        self.assertEqual(self.run_gate(manifest, passing(), "now")[0], 0)

    def test_implemented_test_must_be_registered(self) -> None:
        manifest = complete()
        manifest["cases"][0]["tests"] = ["a::b::unregistered_pg"]
        self.assertEqual(self.run_gate(manifest, None)[0], 1)

    def test_committed_manifest_is_well_formed_and_not_yet_complete(self) -> None:
        manifest = json.loads(MANIFEST.read_text())
        args = [sys.executable, str(SCRIPT)]
        self.assertEqual(subprocess.run(args, capture_output=True, check=False).returncode, 0)
        cases = {cid: "PASS" for cid in manifest["required_cases"]}
        mutants = {name: "KILLED" for name in manifest["required_mutants"]}
        (self.dir / "all.json").write_text(json.dumps({"cases": cases, "mutants": mutants}))
        done = subprocess.run(args + ["--results", str(self.dir / "all.json")],
                              capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 1, "pending or blocked rows keep full from passing")
        self.assertIn("blocked:", done.stdout)


if __name__ == "__main__":
    unittest.main()
