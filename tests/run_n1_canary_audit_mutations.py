"""Execute audit guard mutants in isolated copies; require assertion failures."""
import hashlib
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TEST = "tests.test_n1_canary_audit.AuditCLI."
MUTANTS = [
    ("unfinished-stage-as-pass", "audit.py", 'complete = bool(result.get("audit")) and all(result["audit"]["stages"].values()) and not result["reasons"]',
     'complete = bool(result.get("audit"))', "test_incomplete_stage_never_composes_pass"),
    ("reverse-comparison-removed", "audit.py", 'if m["author"]["id"] in {str(i) for i in manifest["discord"]["bot_author_ids"]} and m["id"] not in explained',
     'if False', "test_reverse_unexplained_and_grounded_extra"),
    ("past-debt-as-current-failure", "delivery.py", 'if old_missing or old_duplicates:\n        reasons.append("historical_debt_unresolved")',
     'if old_missing or old_duplicates:\n        failures.append("delivery_missing")', "test_real_prewindow_missing_is_unknown_with_count_not_current_failure"),
    ("past-debt-discarded", "delivery.py", 'old_missing = sum(not old_seen[pair] for pair in historical)', 'old_missing = 0',
     "test_real_prewindow_missing_is_unknown_with_count_not_current_failure"),
    ("past-duplicates-discarded", "delivery.py", 'old_duplicates = sum(max(0, len(ids) - 1) for ids in old_seen.values())', 'old_duplicates = 0',
     "test_real_prewindow_duplicates_are_retained_without_current_failure"),
    ("search-call-id-unchecked", "native.py", 'require(payload.get("call_id") is None or isinstance(payload["call_id"], str))', 'pass',
     "test_search_call_id_is_typed_before_exclusion"),
    ("shipped-unproduced-guard-removed", "evidence.py", 'reasons = ["unknown_outcome_unproduced"]', 'reasons = []',
     "test_supported_fifty_pass_and_shipped_profile_stays_unknown"),
    ("forward-absence-ignored", "delivery.py", 'failures.append("delivery_missing")', 'pass', "test_forward_loss_duplicate_and_order_are_failures"),
    ("audit-ci-disconnected", "ci-script-checks.sh", '"$PYTHON" -m unittest tests.test_n1_canary_audit', 'true',
     "test_required_guards_audit_recipe_is_executable"),
    ("partial-effect-as-exact-zero", "n1_turn_canary_oracle.py", 'if result["reasons"] and "effects" in result:', 'if False:',
     "test_default_profile_partial_history_never_reports_exact_zero"),
    ("synthetic-commit-ignored", "audit.py", 'failures.append("synthetic_committed")', 'pass',
     "test_synthetic_commit_has_fail_precedence_over_partial_residue"),
    ("successful-placeholder-ignored", "audit.py", 'failures.append("placeholder_created")', 'pass',
     "test_successful_direct_placeholder_fails_even_when_deleted_or_patched"),
    ("past-posted-lookup-skipped", "delivery.py", 'historical_unverified.add(msg_id)', 'continue',
     "test_past_posted_lookup_missing_is_unresolved_not_zero"),
    ("past-uncertainty-discarded", "delivery.py", 'if uncertain:', 'if uncertain and not old:',
     "test_past_ambiguous_then_posted_remains_uncertain"),
    ("past-unresolved-not-old-restored", "delivery.py", 'if uncertain:', 'if uncertain and not old:',
     "test_past_unresolved_then_posted_retains_reason"),
    ("past-ambiguous-candidates-discarded", "delivery.py", 'opened["candidates"] = entry["candidates"]', 'pass',
     "test_past_ambiguous_then_posted_remains_uncertain"),
    ("past-unresolved-reason-discarded", "delivery.py", 'opened["reason"] = entry["reason"]', 'pass',
     "test_past_unresolved_then_posted_retains_reason"),
    ("individual-human-origin-excluded", "audit.py", 'values["discord"].get("individual", {}).items()', '{}.items()',
     "test_past_discord_input_lookup_supports_current_tty_fifty"),
    ("prefix-input-as-current-inventory", "audit.py", 'and turn["physical_observation_key"]["end_offset"] > scope["baseline_offset"]', '',
     "test_prefix_input_cannot_satisfy_current_human_inventory"),
]


def main():
    originals = [ROOT / "scripts/n1_turn_canary_oracle.py", ROOT / "scripts/ci-script-checks.sh",
                 *sorted((ROOT / "scripts/n1_canary").glob("*.*"))]
    hashes = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in originals}
    survived = []
    for label, filename, before, after, test in MUTANTS:
        with tempfile.TemporaryDirectory(prefix="n1-audit-mutant-") as tmp:
            dest = Path(tmp)
            shutil.copytree(ROOT / "scripts/n1_canary", dest / "scripts/n1_canary", ignore=shutil.ignore_patterns("__pycache__"))
            for name in ("n1_turn_canary_oracle.py", "ci-script-checks.sh"):
                shutil.copy2(ROOT / "scripts" / name, dest / "scripts" / name)
            (dest / "tests").mkdir()
            (dest / "tests/__init__.py").write_text("")
            for p in (ROOT / "tests").glob("test_n1_canary_*.py"):
                shutil.copy2(p, dest / "tests" / p.name)
            for name in ("n1_canary_o1", "n1_canary_o2"):
                shutil.copytree(ROOT / "tests/fixtures" / name, dest / "tests/fixtures" / name)
            path = dest / "scripts" / filename if filename in {"ci-script-checks.sh", "n1_turn_canary_oracle.py"} else dest / "scripts/n1_canary" / filename
            source = path.read_text()
            assert before in source, f"missing mutant: {label}"
            path.write_text(source.replace(before, after))
            run = subprocess.run([sys.executable, "-m", "unittest", "-v", TEST + test], cwd=dest, capture_output=True, text=True, timeout=40)
            red = run.returncode == 1 and "FAIL:" in run.stderr and "AssertionError" in run.stderr and not re.search(r"^ERROR: test_", run.stderr, re.M)
            print(f"{label}: {'RED (assertion)' if red else 'SURVIVED/INVALID'}", flush=True)
            print(run.stderr, flush=True)
            if not red:
                survived.append(label)
    assert hashes == {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in originals}, "original bytes changed"
    print(f"audit mutation summary: {len(MUTANTS)} executed, {len(survived)} survived; worktree bytes identical")
    return bool(survived)


if __name__ == "__main__":
    sys.exit(main())
