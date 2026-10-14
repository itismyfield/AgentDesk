"""Assert native and policy guard mutants fail; original worktree bytes stay untouched."""
import hashlib
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = [ROOT / "scripts/n1_turn_canary_oracle.py", ROOT / "scripts/ci-script-checks.sh",
          *sorted((ROOT / "scripts/n1_canary").glob("*.*"))]
NATIVE = "tests.test_n1_canary_native.NativeContracts."
SPLIT = "tests.test_n1_canary_native.SplitContracts."
CLI = "tests.test_n1_canary_native_cli.NativeCLI."
MUTANTS = [
    ("codex-output-validation-removed", "native.py", 'codex_output(payload.get("output"))', 'pass', CLI + "test_codex_excluded_output_shapes_fail_closed_in_actual_cli"),
    ("codex-arguments-validation-removed", "native.py", 'require(isinstance(payload.get(field), str))', 'pass', CLI + "test_codex_missing_arguments_fail_closed_in_actual_cli"),
    ("claude-uuid-owner-ignored", "native.py", 'require(assistant_owners.get(value["key"], owner) == owner, "native_uuid_owner_conflict")', 'pass', CLI + "test_claude_tail_reobservation_cannot_create_a_new_native_turn"),
    ("logical-repeat-counted", "turns.py", 'if key in logical:\n            repeated += 1\n            continue', 'if key in logical:\n            repeated += 1', NATIVE + "test_hand_defined_turn_and_unit_sets_for_both_providers"),
    ("physical-repeat-kept", "turns.py", 'physical_repeated += 1\n            continue', 'physical_repeated += 1', NATIVE + "test_physical_and_logical_keys_are_separate"),
    ("logical-conflict-ignored", "turns.py", 'if key in logical and logical[key] != signature:', 'if False:', NATIVE + "test_same_id_changed_body_hidden_tool_outcome_or_record_order_is_unknown"),
    ("prefix-identity-forgotten", "turns.py", 'if turn["start_offset"] < scope["baseline_offset"]:\n            continue', 'if turn["start_offset"] < scope["baseline_offset"]:\n            logical.pop(key)\n            continue', NATIVE + "test_prefix_ids_and_straddling_baseline_do_not_become_new_turns"),
    ("aborted-counted", "turns.py", 'if turn["outcome"] == "aborted":', 'if False:', NATIVE + "test_hand_defined_turn_and_unit_sets_for_both_providers"),
    ("unit-conflict-ignored", "turns.py", 'if sealed[unit_key]["seal_signature"] != planned["seal_signature"]:', 'if False:', NATIVE + "test_unit_key_conflict_across_different_turns_is_unknown"),
    ("unknown-record-skipped", "native.py", 'require(isinstance(row, dict))\n', 'require(isinstance(row, dict))\n            if row.get("type") == "future_record":\n                continue\n', NATIVE + "test_unknown_records_partial_json_and_unknown_blocks_do_not_get_skipped"),
    ("partial-record-accepted", "native.py", r'raw.endswith(b"\n") and raw[:end].endswith(b"\n")', 'True', NATIVE + "test_unknown_records_partial_json_and_unknown_blocks_do_not_get_skipped"),
    ("unsealed-announcement-ignored", "native.py", 'require(not current.pop("announced"), "native_unit_unsealed")', 'current.pop("announced")', NATIVE + "test_anonymous_mismatched_incomplete_and_announced_unsealed_turns_are_unknown"),
    ("sidechain-as-parent", "native.py", 'require(row.get("isSidechain", False) is False, "native_sidechain_unsupported")', 'pass', NATIVE + "test_claude_sidechain_rows_are_outside_the_supported_sample"),
    ("malformed-excluded-content-ignored", "native.py", 'require(isinstance(item["content"], (str, list)))', 'pass', NATIVE + "test_malformed_excluded_content_is_not_silently_accepted"),
    ("fiftieth-boundary-ignored", "turns.py", 'elif candidates[49]["physical_observation_key"]["end_offset"] != scope["fiftieth_offset"]:', 'elif False:', NATIVE + "test_fiftieth_completion_boundary_is_checked"),
    ("utf16-count-as-scalars", "policy.py", 'return len(text.encode("utf-16-le")) // 2', 'return len(text)', SPLIT + "test_hand_defined_split_boundaries_utf16_and_fences"),
    ("newline-boundary-ignored", "policy.py", 'return newline or len(window)', 'return len(window)', SPLIT + "test_hand_defined_split_boundaries_utf16_and_fences"),
    ("sentence-boundary-ignored", "policy.py", 'return last or len(window)', 'return len(window)', SPLIT + "test_hand_defined_split_boundaries_utf16_and_fences"),
    ("code-fence-reopen-missing", "policy.py", r'prefix = "```" + language + "\n" if in_fence else ""', 'prefix = ""', SPLIT + "test_hand_defined_split_boundaries_utf16_and_fences"),
    ("python-trim-instead-of-rust", "policy.py", '.strip(WHITE_SPACE) if unit["payload"]', '.strip() if unit["payload"]', SPLIT + "test_payload_trim_is_rust_white_space_and_hash_is_utf8"),
    ("native-hook-disconnected", "n1_turn_canary_oracle.py", 'native = reconstruct(raw["transcript"], manifest["scope"])', 'raise Unknown("native_disconnected")', CLI + "test_actual_cli_derives_native_counts_without_pass"),
    ("cli-pass", "n1_turn_canary_oracle.py", 'code = finish(result)', 'code = 0', CLI + "test_fifty_completions_still_leave_origin_and_audit_unknown"),
    ("remove-ci-wiring", "ci-script-checks.sh", '"$PYTHON" -m unittest tests.test_n1_canary_native tests.test_n1_canary_native_cli', 'true', "tests.test_n1_canary_native_wiring.NativeWiring.test_required_guards_recipe_executes_both_native_suites_and_mutants"),
]


def main():
    original = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in SOURCE}
    failures = []
    for label, filename, before, after, test in MUTANTS:
        with tempfile.TemporaryDirectory(prefix="n1-native-mutant-") as tmp:
            dest = Path(tmp)
            shutil.copytree(ROOT / "scripts/n1_canary", dest / "scripts/n1_canary", ignore=shutil.ignore_patterns("__pycache__"))
            for name in ("n1_turn_canary_oracle.py", "ci-script-checks.sh"):
                shutil.copy2(ROOT / "scripts" / name, dest / "scripts" / name)
            (dest / "tests").mkdir()
            (dest / "tests/__init__.py").write_text("")
            for path in (ROOT / "tests").glob("test_n1_canary_*.py"):
                shutil.copy2(path, dest / "tests" / path.name)
            for fixture in ("n1_canary_o1", "n1_canary_o2"):
                shutil.copytree(ROOT / "tests/fixtures" / fixture, dest / "tests/fixtures" / fixture)
            path = dest / "scripts" / filename if filename in ("n1_turn_canary_oracle.py", "ci-script-checks.sh") else dest / "scripts/n1_canary" / filename
            source = path.read_text()
            if before not in source:
                raise AssertionError(f"mutant location missing: {label}")
            path.write_text(source.replace(before, after, 1))
            run = subprocess.run([sys.executable, "-m", "unittest", "-v", test], cwd=dest, capture_output=True, text=True, timeout=30)
            red = (run.returncode == 1 and "FAIL:" in run.stderr and "AssertionError" in run.stderr
                   and re.search(r"^ERROR: test_", run.stderr, re.M) is None)
            print(f"{label}: {'RED (assertion)' if red else 'MUTANT SURVIVED/INVALID'}", flush=True)
            print(run.stderr, flush=True)
            if not red:
                failures.append(label)
    if original != {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in SOURCE}:
        raise AssertionError("original worktree bytes changed")
    print(f"native mutation summary: {len(MUTANTS)} executed, {len(failures)} survived; worktree bytes identical")
    return bool(failures)


if __name__ == "__main__":
    sys.exit(main())
