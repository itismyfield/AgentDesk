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
# The committed required rosters; a deliberate roster change edits these in the same review.
REQUIRED_CASES = {
    "H6-1", "H6-2a", "H6-2b", "H6-3a", "H6-3b", "H6-4", "H6-5", "H6-5-live", "H6-6a", "H6-6b",
    "H7-P-posted", "H7-P-notfound", "H7-P-unresolved", "H7-U", "H7-B", "H7-R", "H7-G",
    "H7-G-unreadable", "H8-G", "H9-D", "H9-W",
}
REQUIRED_MUTANTS = {
    "prepared_as_zero", "unsealed_as_zero", "binding_pending_as_zero", "drain_ignores_running_posts",
    "accepts_ignores_home", "orphan_read_opens_gate", "skip_post_abort_join", "posts_unreadable_as_zero",
    "release_ignores_reset_refusal", "release_skips_reset", "release_refused_changes_detail",
    "release_before_reset", "boot_skips_releasing_drain", "reconcile_as_channel_restored",
    "restored_always_true", "standby_skips_persisted_restore", "standby_marks_restored_before_restore",
}
S3 = "services::cluster::intake_worker::home_route_tests::s3_real::"
# The release-reset rows piece 6 made `now`, the mutants that must kill them and the rows still blocked.
RELEASE_ROWS = {
    "H6-2b": S3 + "holder_pg::th6_releasing_holder_restart_retries_reset_pg",
    "H7-R": S3 + "drain_pg::th7_releasing_tail_posts_before_release_pg",
}
RELEASE_MUTANTS = {
    "release_ignores_reset_refusal": ["H6-2b", "H7-R"],
    "release_skips_reset": ["H6-2b", "H7-R"],
    "release_refused_changes_detail": ["H6-2b", "H7-R"],
    "release_before_reset": ["H6-2b", "H7-R"],
    "boot_skips_releasing_drain": ["H6-2b"],
}
BLOCKED = {"H6-3a", "H6-3b", "H6-6b"}


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
                "--inventory", str(self.dir / "inventory.txt")]
        if results is None:
            args.append("--structure-only")
        else:
            (self.dir / "results.json").write_text(json.dumps(results))
            args += ["--require", require, "--results", str(self.dir / "results.json")]
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

    def test_empty_or_repeated_roster_fails(self) -> None:
        empty = complete()
        empty["required_cases"] = []
        rc, out = self.run_gate(empty, None)
        self.assertEqual(rc, 1)
        self.assertIn("required_cases is empty", out)
        repeated = complete()
        repeated["required_mutants"].append("m_now")
        rc, out = self.run_gate(repeated, None)
        self.assertEqual(rc, 1)
        self.assertIn("required_mutants lists m_now twice", out)

    def test_require_without_results_accepts_nothing(self) -> None:
        (self.dir / "manifest.json").write_text(json.dumps(complete()))
        base = [sys.executable, str(SCRIPT), "--manifest", str(self.dir / "manifest.json"),
                "--inventory", str(self.dir / "inventory.txt")]
        for require in ("now", "full"):
            done = subprocess.run(base + ["--require", require], capture_output=True, check=False)
            self.assertNotEqual(done.returncode, 0, require)
        done = subprocess.run(base + ["--structure-only"], capture_output=True, check=False)
        self.assertEqual(done.returncode, 0)

    def test_structure_only_refuses_results(self) -> None:
        (self.dir / "manifest.json").write_text(json.dumps(complete()))
        (self.dir / "results.json").write_text(json.dumps(passing()))
        args = [sys.executable, str(SCRIPT), "--manifest", str(self.dir / "manifest.json"),
                "--inventory", str(self.dir / "inventory.txt"), "--structure-only",
                "--results", str(self.dir / "results.json")]
        done = subprocess.run(args, capture_output=True, text=True, check=False)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("--structure-only takes no --results", done.stderr)

    def test_mutant_on_a_required_now_case_counts_for_now(self) -> None:
        manifest = complete()
        manifest["mutants"][0]["cases"] = ["N", "S"]
        results = passing()
        del results["mutants"]["m_now"]
        rc, out = self.run_gate(manifest, results, "now")
        self.assertEqual(rc, 1)
        self.assertIn("mutant m_now: missing", out)

    def test_committed_required_rosters_are_pinned(self) -> None:
        manifest = json.loads(MANIFEST.read_text())
        self.assertEqual(sorted(manifest["required_cases"]), sorted(REQUIRED_CASES))
        self.assertEqual(sorted(manifest["required_mutants"]), sorted(REQUIRED_MUTANTS))

    def test_unreadable_post_guard_counts_for_now(self) -> None:
        manifest = json.loads(MANIFEST.read_text())
        cases = {cid: "PASS" for cid in manifest["required_cases"] if cid != "H7-G-unreadable"}
        mutants = {name: "KILLED" for name in manifest["required_mutants"]}
        mutants["posts_unreadable_as_zero"] = "SURVIVED"
        (self.dir / "r.json").write_text(json.dumps({"cases": cases, "mutants": mutants}))
        args = [sys.executable, str(SCRIPT), "--require", "now", "--results", str(self.dir / "r.json")]
        done = subprocess.run(args, capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 1)
        self.assertIn("H7-G-unreadable: result missing", done.stdout)
        self.assertIn("mutant posts_unreadable_as_zero: SURVIVED", done.stdout)

    def test_release_rows_are_now_with_their_tests_and_mutants(self) -> None:
        manifest = json.loads(MANIFEST.read_text())
        rows = {case["id"]: case for case in manifest["cases"]}
        for cid, test in RELEASE_ROWS.items():
            row = rows[cid]
            got = (row["profile"], row["piece"], row["status"], row["tests"])
            self.assertEqual(got, ("now", 6, "implemented", [test]), cid)
        mutants = {mutant["name"]: mutant["cases"] for mutant in manifest["mutants"]}
        for name, cases in RELEASE_MUTANTS.items():
            self.assertEqual(mutants.get(name), cases, name)
        blocked = {cid for cid, row in rows.items() if row["status"].startswith("blocked:")}
        self.assertEqual(blocked, BLOCKED)

    def run_committed(self, cases: dict, mutants: dict, require: str) -> tuple[int, str]:
        (self.dir / "r.json").write_text(json.dumps({"cases": cases, "mutants": mutants}))
        args = [sys.executable, str(SCRIPT), "--require", require, "--results", str(self.dir / "r.json")]
        done = subprocess.run(args, capture_output=True, text=True, check=False)
        return done.returncode, done.stdout

    def test_release_rows_and_mutants_gate_now(self) -> None:
        manifest = json.loads(MANIFEST.read_text())
        cases = {cid: "PASS" for cid in manifest["required_cases"]}
        mutants = {name: "KILLED" for name in manifest["required_mutants"]}
        self.assertEqual(self.run_committed(cases, mutants, "now")[0], 0)
        rc, out = self.run_committed(cases, mutants, "full")
        self.assertEqual(rc, 1)
        failed = {line.split(":")[0].strip() for line in out.splitlines() if line.startswith("  ")}
        self.assertEqual(failed, BLOCKED, out)
        for cid in RELEASE_ROWS:
            for got in (None, "FAIL"):
                broken = {k: v for k, v in cases.items() if k != cid}
                if got:
                    broken[cid] = got
                rc, out = self.run_committed(broken, mutants, "now")
                self.assertEqual(rc, 1, f"{cid} {got}")
                self.assertIn(f"{cid}: result {got or 'missing'}", out)
        for name in RELEASE_MUTANTS:
            for got in (None, "SURVIVED", "TIMEOUT"):
                broken = {k: v for k, v in mutants.items() if k != name}
                if got:
                    broken[name] = got
                rc, out = self.run_committed(cases, broken, "now")
                self.assertEqual(rc, 1, f"{name} {got}")
                self.assertIn(f"mutant {name}: {got or 'missing'}", out)

    def test_committed_manifest_is_well_formed_and_not_yet_complete(self) -> None:
        manifest = json.loads(MANIFEST.read_text())
        args = [sys.executable, str(SCRIPT)]
        done = subprocess.run(args + ["--structure-only"], capture_output=True, check=False)
        self.assertEqual(done.returncode, 0)
        cases = {cid: "PASS" for cid in manifest["required_cases"]}
        mutants = {name: "KILLED" for name in manifest["required_mutants"]}
        (self.dir / "all.json").write_text(json.dumps({"cases": cases, "mutants": mutants}))
        args += ["--require", "full", "--results", str(self.dir / "all.json")]
        done = subprocess.run(args, capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 1, "pending or blocked rows keep full from passing")
        self.assertIn("blocked:", done.stdout)


if __name__ == "__main__":
    unittest.main()
