#!/usr/bin/env bash
# Run a deterministic shard of the fixed relay-authority mutations.
# Every selected row must be killed by its named test.
#
# Exit codes. Every non-zero code below is a gate failure; there is no
# "tolerated" non-zero exit.
#     0  every mutation was killed by the test named for it
#     1  a mutation SURVIVED the test that is supposed to kill it
#     2  invalid invocation: bad shard, bad test mode, missing fixture runner, bad source
#    75  another relay-authority mutation run holds the lock
#    93  NO-VERDICT: incomplete run or exit status contradicts the test summary
#    94  NO-TEST-RAN: the named test never executed, so nothing was proven (#5243)
#    95  BUILD-BROKEN: the mutant did not compile, so it is not a valid mutant
#         and cargo's rc=101 does not mean "the test caught it" (#5243)
#    96  cache proof invalid: the mutant's build was reused from cache
#    97  source restoration or lock release failed
#    98  per-row source restoration hash mismatch
#   129  HUP / 130 INT / 143 TERM
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
readonly REPO_ROOT
readonly TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target/relay-authority-mutations}"
readonly LOCK_DIR="$REPO_ROOT/target/relay-authority-mutations.lock"
readonly MUTATION_COUNT=7
readonly MODE="${RELAY_AUTHORITY_MUTATION_TEST_MODE:-cargo}"
readonly FIXTURE_RUNNER="${RELAY_AUTHORITY_MUTATION_FIXTURE_RUNNER:-}"
readonly SHARD_INDEX="${RELAY_AUTHORITY_MUTATION_SHARD_INDEX-0}"
readonly SHARD_TOTAL="${RELAY_AUTHORITY_MUTATION_SHARD_TOTAL-1}"

if [[ ! "$SHARD_TOTAL" =~ ^[1-9]$ || ! "$SHARD_INDEX" =~ ^[0-9]$ ]] ||
  ((SHARD_TOTAL > MUTATION_COUNT || SHARD_INDEX >= SHARD_TOTAL)); then
  printf 'ERROR invalid mutation shard index=%q total=%q (require 0 <= index < total <= %d)\n' \
    "$SHARD_INDEX" "$SHARD_TOTAL" "$MUTATION_COUNT" >&2
  exit 2
fi

if [[ "$MODE" != "cargo" && "$MODE" != "fixture" ]]; then
  printf 'ERROR invalid RELAY_AUTHORITY_MUTATION_TEST_MODE=%q\n' "$MODE" >&2
  exit 2
fi
if [[ "$MODE" == "fixture" && ! -x "$FIXTURE_RUNNER" ]]; then
  printf 'ERROR fixture mode requires an executable RELAY_AUTHORITY_MUTATION_FIXTURE_RUNNER\n' >&2
  exit 2
fi

readonly TERMINAL_HANDOFF="src/services/discord/session_relay_sink/terminal_handoff.rs"
readonly SESSION_RELAY_SINK="src/services/discord/session_relay_sink.rs"
# #5457 moved the S4 fence layer out of `tmux_watcher_registry.rs` into this
# child module. Both S4 fence rows below anchor on text that went with it, so
# they mutate the child; the registry root no longer carries a mutated anchor.
readonly WATCHER_FENCES="src/services/discord/tmux_watcher_registry/fences.rs"
readonly DESTRUCTIVE_CANCEL_GATE="src/services/discord/destructive_cancel_gate.rs"
# The path filter selects these sources plus their judges and fixture owners.
# Other authority paths are covered by the unconditional named contract targets.
readonly -a MUTATION_FILES=(
  "$TERMINAL_HANDOFF"
  "$SESSION_RELAY_SINK"
  "$WATCHER_FENCES"
  "$DESTRUCTIVE_CANCEL_GATE"
)
declare -a ORIGINAL_COPIES=()
declare -a ORIGINAL_HASHES=()
RESTORE_FAILED=0
LOCK_HELD=0
CURRENT_MUTATION=""

sha256_file() {
  shasum -a 256 "$1" | cut -d ' ' -f 1
}

acquire_lock() {
  if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    printf 'ERROR another relay-authority mutation run holds lock: %s\n' "$LOCK_DIR" >&2
    exit 75
  fi
  LOCK_HELD=1
  printf '%s\n' "$$" >"$LOCK_DIR/pid"
}

release_lock() {
  if ((LOCK_HELD == 0)); then
    return 0
  fi
  if ! rm -f "$LOCK_DIR/pid" || ! rmdir "$LOCK_DIR"; then
    printf 'ERROR mutation lock release failed: %s\n' "$LOCK_DIR" >&2
    return 1
  fi
  LOCK_HELD=0
}

prepare_backups() {
  local index relative source backup
  for index in "${!MUTATION_FILES[@]}"; do
    relative="${MUTATION_FILES[$index]}"
    source="$REPO_ROOT/$relative"
    if [[ ! -f "$source" || -L "$source" ]]; then
      printf 'ERROR mutation source must be a non-symlink regular file: %s\n' "$relative" >&2
      exit 2
    fi
    backup="$(mktemp "${TMPDIR:-$REPO_ROOT/target}/relay-authority-mutation.XXXXXX")"
    cp -p "$source" "$backup"
    ORIGINAL_COPIES[$index]="$backup"
    ORIGINAL_HASHES[$index]="$(sha256_file "$source")"
  done
}

restore_sources() {
  local index relative source backup restored_hash
  set +e
  for index in "${!MUTATION_FILES[@]}"; do
    relative="${MUTATION_FILES[$index]}"
    source="$REPO_ROOT/$relative"
    backup="${ORIGINAL_COPIES[$index]:-}"
    if [[ -n "$backup" && -f "$backup" ]]; then
      cp -p "$backup" "$source" || RESTORE_FAILED=1
      restored_hash="$(sha256_file "$source" 2>/dev/null)" || RESTORE_FAILED=1
      if [[ "$restored_hash" != "${ORIGINAL_HASHES[$index]:-missing}" ]]; then
        printf 'ERROR restoration hash mismatch: %s\n' "$relative" >&2
        RESTORE_FAILED=1
      fi
      rm -f "$backup" || RESTORE_FAILED=1
    fi
  done
  if ((RESTORE_FAILED != 0)); then
    printf 'ERROR source restoration failed after mutation=%s\n' "${CURRENT_MUTATION:-none}" >&2
  fi
  return "$RESTORE_FAILED"
}

on_exit() {
  local incoming_rc=$?
  trap - EXIT HUP INT TERM
  if ! restore_sources; then
    incoming_rc=97
  fi
  if ! release_lock; then
    incoming_rc=97
  fi
  exit "$incoming_rc"
}

apply_exact_mutation() {
  local relative=$1 expected=$2 replacement=$3
  python3 - "$REPO_ROOT/$relative" "$expected" "$replacement" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
expected = sys.argv[2]
replacement = sys.argv[3]
source = path.read_text(encoding="utf-8")
count = source.count(expected)
if count != 1:
    raise SystemExit(
        f"ERROR mutation anchor must match exactly once: path={path} matches={count} anchor={expected!r}"
    )
path.write_text(source.replace(expected, replacement, 1), encoding="utf-8")
PY
}

restore_after_row() {
  local index source backup restored_hash
  for index in "${!MUTATION_FILES[@]}"; do
    source="$REPO_ROOT/${MUTATION_FILES[$index]}"
    backup="${ORIGINAL_COPIES[$index]}"
    cp -p "$backup" "$source"
    restored_hash="$(sha256_file "$source")"
    if [[ "$restored_hash" != "${ORIGINAL_HASHES[$index]}" ]]; then
      printf 'ERROR row restoration hash mismatch: %s\n' "${MUTATION_FILES[$index]}" >&2
      exit 98
    fi
  done
}

no_verdict() {
  printf 'ERROR mutation=%s status=NO-VERDICT rc=%d target=%s (%s)\n' "$1" "$2" "$3" "$5" >&2
  cat "$4" >&2
}

run_target() {
  local mutation=$1 target=$2 log=$3 rc compile_count test_result rest passed failed
  local summaries running summary_pattern expected_result row_status parent_rows parent_line summary_line running_line
  # Keep child/panic diagnostics off the parent oracle; bind its result to the exact test.
  local stdout_log="$log.stdout" result_log="$log.results"
  : >"$stdout_log"
  : >"$result_log"
  if [[ "$MODE" == "fixture" ]]; then
    set +e
    "$FIXTURE_RUNNER" "$mutation" "$target" >"$stdout_log" 2>"$log"
    rc=$?
    set -e
  else
    set +e
    (
      cd "$REPO_ROOT"
      # Incremental is on here although the repo default is off: every row is a
      # two-anchor delta from the previous build, and the sccache constraint
      # behind CARGO_INCREMENTAL=0 does not apply once RUSTC_WRAPPER is unset.
      # It cannot fake a kill -- a binary missing the mutation passes the named
      # test, which this script grades as SURVIVED.
      env -u RUSTC_WRAPPER -u AGENTDESK_ROOT_DIR \
        CARGO_TERM_COLOR=never CARGO_INCREMENTAL=1 CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo test --offline --lib "$target" -- --exact --test-threads=1 \
          --no-capture --logfile "$result_log"
    ) >"$stdout_log" 2>"$log"
    rc=$?
    set -e
  fi
  cat "$stdout_log" >>"$log"
  if [[ "$MODE" == "cargo" ]]; then
    compile_count="$(grep -Fc 'Compiling agentdesk v' "$log" || true)"
    if [[ "$compile_count" != "1" ]] || grep -Fq 'Fresh agentdesk v' "$log"; then
      printf 'ERROR mutation=%s cache-proof=invalid compile_count=%s expected=1 and no Fresh agentdesk\n' "$mutation" "$compile_count" >&2
      cat "$log" >&2
      return 96
    fi

    # CARGO_TERM_COLOR=never above makes both cache-proof markers stable for grep.
    printf 'CACHE_PROOF mutation=%s compiling_agentdesk=%s fresh_agentdesk=0\n' "$mutation" "$compile_count"
  fi

  # #5243: rc alone cannot separate "the test caught the mutant" from "the mutant
  # did not compile" — cargo returns 101 for both. A failed build also writes no
  # fingerprint, so it recompiles on every retry and satisfies the cache proof
  # above with the same compiling=1 fresh=0 values a real kill produces. Judge on
  # the log of this same single invocation. Do not add a second cargo call: a
  # preceding `cargo check` would make this run Fresh and trip the cache proof.
  if grep -Fq 'could not compile `agentdesk`' "$log"; then
    printf 'MUTATION_ORACLE mutation=%s compile_ok=no tests_passed=0 tests_failed=0\n' "$mutation"
    printf 'ERROR mutation=%s status=BUILD-BROKEN rc=%d target=%s (mutant did not compile)\n' "$mutation" "$rc" "$target" >&2
    cat "$log" >&2
    return 95
  fi

  summaries="$(grep -c '^test result:' "$stdout_log" || true)"
  running="$(grep -E '^running [0-9]+ tests?$' "$stdout_log" || true)"
  test_result="$(grep '^test result:' "$stdout_log" || true)"
  summary_pattern='^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored; ([0-9]+) measured; [0-9]+ filtered out;?($| finished in .+s$)'
  if [[ "$summaries" != 1 || ! "$test_result" =~ $summary_pattern ]]; then
    no_verdict "$mutation" "$rc" "$target" "$log" "missing or ambiguous summary"
    return 93
  fi
  if [[ "$running" == 'running 0 tests' && "${BASH_REMATCH[2]}" == 0 && "${BASH_REMATCH[3]}" == 0 ]]; then
    printf 'MUTATION_ORACLE mutation=%s compile_ok=yes tests_passed=0 tests_failed=0\n' "$mutation"
    printf 'ERROR mutation=%s status=NO-TEST-RAN rc=%d target=%s (named test did not execute)\n' "$mutation" "$rc" "$target" >&2
    cat "$log" >&2
    return 94
  fi
  if [[ "$running" != 'running 1 test' || "${BASH_REMATCH[4]}" != 0 || "${BASH_REMATCH[5]}" != 0 ]]; then
    no_verdict "$mutation" "$rc" "$target" "$log" "incomplete named-test run"
    return 93
  fi
  case "$test_result" in
    'test result: ok. 1 passed; 0 failed;'* | 'test result: FAILED. 0 passed; 1 failed;'*) ;;
    *)
      no_verdict "$mutation" "$rc" "$target" "$log" "inconsistent named-test summary"
      return 93 ;;
  esac
  if [[ ( "$test_result" == 'test result: ok.'* && "$rc" != 0 ) ||
        ( "$test_result" == 'test result: FAILED.'* && "$rc" == 0 ) ]]; then
    no_verdict "$mutation" "$rc" "$target" "$log" "exit status contradicts summary"
    return 93
  fi

  expected_result="failed $target"
  row_status=FAILED
  if ((rc == 0)); then
    expected_result="ok $target"
    row_status=ok
  fi
  if [[ "$MODE" == "cargo" && "$(cat "$result_log")" != "$expected_result" ]]; then
    no_verdict "$mutation" "$rc" "$target" "$log" "missing or inconsistent parent test result"
    return 93
  fi

  if [[ "$MODE" == "cargo" ]]; then
    parent_rows="$(grep -nFx "test $target ... $row_status" "$stdout_log" || true)"
    parent_line="${parent_rows%%:*}"
    summary_line="$(grep -n '^test result:' "$stdout_log")"
    summary_line="${summary_line%%:*}"
    running_line="$(grep -nE '^running [0-9]+ tests?$' "$stdout_log")"
    running_line="${running_line%%:*}"
    if [[ -z "$parent_rows" || "$parent_rows" == *$'\n'* ]] ||
      ((running_line >= parent_line || parent_line >= summary_line)); then
      no_verdict "$mutation" "$rc" "$target" "$log" "missing or ambiguous parent completion row"
      return 93
    fi
  fi

  rest="${test_result#*. }"
  passed="${rest%% passed;*}"
  rest="${rest#* passed; }"
  failed="${rest%% failed;*}"
  # #5243: the KILLED path deletes its own log, so a green run used to leave no
  # trace of what killed the mutant. Emit the verdict's evidence on stdout, where
  # the existing MUTATION_* markers already go, rather than inventing a new
  # artifact path.
  printf 'MUTATION_ORACLE mutation=%s compile_ok=yes tests_passed=%s tests_failed=%s\n' "$mutation" "$passed" "$failed"
  return "$rc"
}

remove_run_logs() {
  rm -f "$1" "$1.stdout" "$1.results"
}

run_mutation() {
  local mutation=$1 relative=$2 expected=$3 replacement=$4 target=$5 log rc command
  CURRENT_MUTATION="$mutation"
  restore_after_row
  apply_exact_mutation "$relative" "$expected" "$replacement"
  log="$(mktemp "${TMPDIR:-$REPO_ROOT/target}/relay-authority-${mutation}.XXXXXX")"
  command="cargo test --offline --lib $target -- --exact --test-threads=1 --no-capture --logfile $log.results"

  if run_target "$mutation" "$target" "$log"; then
    rc=0
  else
    rc=$?
  fi

  if ((rc == 0)); then
    printf 'MUTATION_RESULT mutation=%s status=SURVIVED rc=0 target=%s\n' "$mutation" "$target" >&2
    printf 'ERROR mutation survived: %s\nCOMMAND: %s\n' "$mutation" "$command" >&2
    cat "$log" >&2
    remove_run_logs "$log"
    exit 1
  fi
  # Oracle failures already streamed the full log to stderr inside run_target.
  if ((rc == 93 || rc == 94 || rc == 95 || rc == 96)); then
    remove_run_logs "$log"
    exit "$rc"
  fi

  printf 'MUTATION_RESULT mutation=%s status=KILLED rc=%d target=%s\n' "$mutation" "$rc" "$target"
  remove_run_logs "$log"
  restore_after_row
}

declare -a MUTATION_IDS=() MUTATION_SOURCES=() MUTATION_EXPECTED=()
declare -a MUTATION_REPLACEMENTS=() MUTATION_TARGETS=()
declare -a SHARD_ROWS=() SHARD_IDS=() ASSIGNMENT_COUNTS=()

register_mutation() {
  MUTATION_IDS+=("$1")
  MUTATION_SOURCES+=("$2")
  MUTATION_EXPECTED+=("$3")
  MUTATION_REPLACEMENTS+=("$4")
  MUTATION_TARGETS+=("$5")
}

validate_shards() {
  local index previous shard
  if ((${#MUTATION_IDS[@]} != MUTATION_COUNT)); then
    printf 'ERROR mutation row count=%d expected=%d\n' "${#MUTATION_IDS[@]}" "$MUTATION_COUNT" >&2
    exit 2
  fi
  for index in "${!MUTATION_IDS[@]}"; do
    for ((previous = 0; previous < index; previous++)); do
      if [[ "${MUTATION_IDS[$previous]}" == "${MUTATION_IDS[$index]}" ]]; then
        printf 'ERROR duplicate mutation id=%s\n' "${MUTATION_IDS[$index]}" >&2
        exit 2
      fi
    done
    ASSIGNMENT_COUNTS[$index]=0
  done
  for ((shard = 0; shard < SHARD_TOTAL; shard++)); do
    for index in "${!MUTATION_IDS[@]}"; do
      if ((index % SHARD_TOTAL == shard)); then
        ASSIGNMENT_COUNTS[$index]=$((ASSIGNMENT_COUNTS[$index] + 1))
        if ((shard == SHARD_INDEX)); then
          SHARD_ROWS+=("$index")
          SHARD_IDS+=("${MUTATION_IDS[$index]}")
        fi
      fi
    done
  done
  for index in "${!MUTATION_IDS[@]}"; do
    if ((ASSIGNMENT_COUNTS[$index] != 1)); then
      printf 'ERROR mutation id=%s shard assignments=%d expected=1\n' \
        "${MUTATION_IDS[$index]}" "${ASSIGNMENT_COUNTS[$index]}" >&2
      exit 2
    fi
  done
}

register_mutation \
  M10 "$TERMINAL_HANDOFF" \
  'delivery_frontier::SinkDeliveryProofResult::Persisted => Self::Delivered,' \
  'delivery_frontier::SinkDeliveryProofResult::Persisted => Self::NotDelivered,' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::relay_deliver_preserves_tail_anchor_and_observes_persisted_proof'

register_mutation \
  M6 "$TERMINAL_HANDOFF" \
  'terminal_not_delivered || fenced_terminal_without_delivery' \
  'terminal_not_delivered' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::fenced_terminal_without_parser_delivery_is_terminal_not_delivered'

register_mutation \
  M8 "$TERMINAL_HANDOFF" \
  'Err(error) => return Err(error),' \
  'Err(_error) => { terminal_not_delivered = true; }' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::relay_deliver_propagates_injected_transport_error'

register_mutation \
  anchor-drop "$SESSION_RELAY_SINK" \
  $'formatting::watcher_completion_footer_anchor(\n                        last_chunk_anchor.as_ref(),\n                        msg_id,\n                        &relay_text,\n                    )' \
  $'formatting::watcher_completion_footer_anchor(\n                        None,\n                        msg_id,\n                        &relay_text,\n                    )' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::relay_deliver_preserves_tail_anchor_and_observes_persisted_proof'

# Bypass the delivery lease while retaining a compilable commit path.
register_mutation \
  S4-m5 "$WATCHER_FENCES" \
  '        Some(fence) => fence.commit_if_permitted(commit),' \
  '        Some(_fence) => Some(commit()),' \
  'services::discord::relay_recovery::tests::post_gate_identity_matched_live_delivery_lease_blocks_dead_frontier_watcher_cancel'

# Let the terminal envelope bypass the relay frontier progress check.
register_mutation \
  S4-m6 "$DESTRUCTIVE_CANCEL_GATE" \
  $'    let Some(expected_output_path) = snapshot.output_path.as_deref() else {' \
  $'    if terminal_envelope_present(provider, snapshot) {\n        return DestructiveCancelGate::Allowed("terminal_envelope_present");\n    }\n    let Some(expected_output_path) = snapshot.output_path.as_deref() else {' \
  'services::discord::destructive_cancel_gate::tests::terminal_envelope_does_not_outrank_relay_frontier_progress_on_reprobe'

# Release the judgment lock before destruction to expose a racing acquire.
register_mutation \
  S4-m7 "$WATCHER_FENCES" \
  $'            #[cfg(test)]\n            run_delivery_fence_permitted_hook_for_tests(self.site);\n            Some(commit())\n        })\n    }' \
  $'            Some(())\n        })?;\n        #[cfg(test)]\n        run_delivery_fence_permitted_hook_for_tests(self.site);\n        Some(commit())\n    }' \
  'services::discord::tmux_watcher_registry_restore_tests::delivery_fence_judgment_and_destruction_are_atomic_against_a_racing_acquire'

validate_shards
printf 'MUTATION_SHARD index=%d total=%d count=%d ids=%s\n' \
  "$SHARD_INDEX" "$SHARD_TOTAL" "${#SHARD_ROWS[@]}" "$(IFS=,; printf '%s' "${SHARD_IDS[*]}")"

mkdir -p "${TMPDIR:-$REPO_ROOT/target}" "$(dirname "$LOCK_DIR")"
trap on_exit EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
acquire_lock
prepare_backups
printf 'MUTATION_COUNT count=%d minimum=4\n' "$MUTATION_COUNT"
# Rows within a shard stay sequential to reuse incremental builds.
printf 'MUTATION_RUNNER cores=%s target_dir_avail_kb=%s\n' \
  "$( (nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown) )" \
  "$( (df -Pk "$REPO_ROOT" 2>/dev/null | awk 'NR==2{print $4}') || echo unknown )"

killed_count=0
for index in "${SHARD_ROWS[@]}"; do
  run_mutation "${MUTATION_IDS[$index]}" "${MUTATION_SOURCES[$index]}" \
    "${MUTATION_EXPECTED[$index]}" "${MUTATION_REPLACEMENTS[$index]}" \
    "${MUTATION_TARGETS[$index]}"
  killed_count=$((killed_count + 1))
done
printf 'MUTATION_SUMMARY killed=%d survived=0 minimum=4 status=PASS\n' "$killed_count"
