"""Producer mutants must reach assertion failures; original bytes are pinned."""
import hashlib
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TEST = "tests.test_n1_canary_producer.ProducerCLI."
MUTANTS = [
    ("capability-always-approved", "manifest.py", 'return any(c["build_commit"]', 'return True or any(c["build_commit"]',
     "test_t9_unapproved_pair_or_capability_never_becomes_zero"),
    ("old-digest-approved", "manifest.py", 'c["executable_sha256"] == runtime["executable_sha256"]', 'True',
     "test_t9_unapproved_pair_or_capability_never_becomes_zero"),
    ("actual-confirmation-capability-ignored", "manifest.py", 'and c["actual_confirmation_produced"]', '',
     "test_t9_unapproved_pair_or_capability_never_becomes_zero"),
    ("unknown-log-counter-comparison-removed", "outcomes.py", 'if end < before or end - before != raw:', 'if False:',
     "test_t10_counter_only_missing_log_and_regression_are_unknown"),
    ("unknown-witness-reread-counted-twice", "outcomes.py", 'unknown_witnesses_distinct=len(witnesses)', 'unknown_witnesses_distinct=len(rows)',
     "test_t10_unknown_reread_is_one_witness_two_raw_events"),
    ("resume-forgets-unknown", "outcomes.py", 'witnesses.add(witness)',
     'if row["seq"] > baseline["seq_high_water"]:\n            witnesses.add(witness)', "test_t10_resume_retains_unresolved_witness_before_confirmation"),
    ("completed-erases-unknown", "outcomes.py", 'reasons.append("unknown_outcome_conflict")', 'pass',
     "test_t10_named_unknown_conflicts_with_completed_not_resolved_by_it"),
    ("logical-reread-is-new-turn", "turns.py", 'if key in logical:\n            repeated += 1\n            continue',
     'if key in logical:\n            repeated += 1', "test_t8_49_native_plus_logical_reread_never_counts_fifty"),
    ("production-native-comparison-removed", "audit.py", 'if not expected <= matched:', 'if False:',
     "test_t11_missing_native_production_even_with_fifty_raw_events"),
    ("shipped-unproduced-guard-removed", "evidence.py", 'reasons = [] if producer_supported else ["unknown_outcome_unproduced"]', 'reasons = []',
     "test_t9_approved_pair_and_shipped_table"),
    ("partial-observation-as-zero", "evidence.py", 'if healthy and complete and valid and not any(', 'if valid and not any(',
     "test_supported_unavailable_drop_and_open_incomplete_keep_unknown_null"),
    ("confirmation-receipt-check-removed", "evidence.py", 'reasons.append("confirmation_boundary_unknown")', 'pass',
     "test_t12_denied_confirmation_capture_stays_unknown"),
    ("producer-ci-disconnected", "ci-script-checks.sh", '"$PYTHON" -m unittest tests.test_n1_canary_producer', 'true',
     "test_required_guards_producer_recipe_executes_tests_and_mutants"),
]


def main():
    files = [ROOT / "scripts/n1_turn_canary_oracle.py", ROOT / "scripts/ci-script-checks.sh",
             *sorted((ROOT / "scripts/n1_canary").glob("*.*"))]
    original = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
    survived = []
    for label, filename, before, after, test in MUTANTS:
        with tempfile.TemporaryDirectory(prefix="n1-producer-mutant-") as tmp:
            dest = Path(tmp)
            shutil.copytree(ROOT / "scripts/n1_canary", dest / "scripts/n1_canary", ignore=shutil.ignore_patterns("__pycache__"))
            for name in ("n1_turn_canary_oracle.py", "ci-script-checks.sh"):
                shutil.copy2(ROOT / "scripts" / name, dest / "scripts" / name)
            (dest / "tests").mkdir()
            (dest / "tests/__init__.py").write_text("")
            for p in (ROOT / "tests").glob("test_n1_canary_*.py"):
                shutil.copy2(p, dest / "tests" / p.name)
            for fixture in ("n1_canary_o1", "n1_canary_o2"):
                shutil.copytree(ROOT / "tests/fixtures" / fixture, dest / "tests/fixtures" / fixture)
            path = dest / "scripts" / filename if filename in {"ci-script-checks.sh", "n1_turn_canary_oracle.py"} else dest / "scripts/n1_canary" / filename
            source = path.read_text()
            assert before in source, f"missing mutant: {label}"
            path.write_text(source.replace(before, after, 1))
            run = subprocess.run([sys.executable, "-m", "unittest", "-v", TEST + test], cwd=dest, capture_output=True, text=True, timeout=40)
            red = run.returncode == 1 and "FAIL:" in run.stderr and "AssertionError" in run.stderr and not re.search(r"^ERROR: test_", run.stderr, re.M)
            print(f"{label}: {'RED (assertion)' if red else 'SURVIVED/INVALID'}", flush=True)
            print(run.stderr, flush=True)
            if not red:
                survived.append(label)
    assert original == {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in files}, "original bytes changed"
    print(f"producer mutation summary: {len(MUTANTS)} executed, {len(survived)} survived; worktree bytes identical")
    return bool(survived)


if __name__ == "__main__":
    sys.exit(main())
