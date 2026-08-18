#!/usr/bin/env bash
# #5071 condition-3: seven fixed, hand-written relay-authority mutations.
#
# The declared floor stays four (the condition-3 minimum). #5071 relay-tail S4
# added the two destructive-fence rows S4-m5 and S4-m6 on top of it, and its r2
# repair added S4-m7 for the fence's judge/commit atomicity.
#
# Deferred workflow wiring (apply only after the relay-authority lane lands):
# in jobs.relay-authority-contract.steps, immediately after
# "Run named relay-authority contract targets" and before "sccache stats", add:
#
#      - name: Require relay-authority mutations to be killed
#        env:
#          BASH_ENV: /dev/null
#          CARGO_PROFILE_DEV_DEBUG: "0"
#          CARGO_PROFILE_TEST_DEBUG: "0"
#        shell: bash
#        timeout-minutes: 30
#        run: bash scripts/run_relay_authority_mutations.sh
#
# In that same follow-up commit, flip condition3_mutations_present to true in
# scripts/relay_authority_contract_targets.json and re-pin the
# relay-authority-contract job_sha256 in scripts/check-ci-runner-hardening.sh.
#
# Exit codes. Every non-zero code below is a gate failure; there is no
# "tolerated" non-zero exit.
#     0  every mutation was killed by the test named for it
#     1  a mutation SURVIVED the test that is supposed to kill it
#     2  invalid invocation: bad test mode, missing fixture runner, bad source
#    75  another relay-authority mutation run holds the lock
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
readonly WATCHER_REGISTRY="src/services/discord/tmux_watcher_registry.rs"
readonly DESTRUCTIVE_CANCEL_GATE="src/services/discord/destructive_cancel_gate.rs"
readonly -a MUTATION_FILES=(
  "$TERMINAL_HANDOFF"
  "$SESSION_RELAY_SINK"
  "$WATCHER_REGISTRY"
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

run_target() {
  local mutation=$1 target=$2 log=$3 rc compile_count test_result rest passed failed
  if [[ "$MODE" == "fixture" ]]; then
    set +e
    "$FIXTURE_RUNNER" "$mutation" "$target" >"$log" 2>&1
    rc=$?
    set -e
  else
    set +e
    (
      cd "$REPO_ROOT"
      env -u RUSTC_WRAPPER -u AGENTDESK_ROOT_DIR \
        CARGO_TERM_COLOR=never CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo test --offline --lib "$target" -- --exact --test-threads=1
    ) >"$log" 2>&1
    rc=$?
    set -e

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

  # The named test must actually have executed. `cargo test --lib <name> --exact`
  # answers rc=0 with "0 passed; 0 failed" when the filter matches nothing, which
  # the old script reported as "mutation survived" — red, but for the wrong
  # reason. --exact names exactly one test, so exactly one must have run.
  test_result="$( { grep -E '^test result: (ok|FAILED)\. [0-9]+ passed; [0-9]+ failed;' "$log" || true; } | head -n 1)"
  case "$test_result" in
    'test result: ok. 1 passed; 0 failed;'* | 'test result: FAILED. 0 passed; 1 failed;'*) ;;
    *)
      printf 'MUTATION_ORACLE mutation=%s compile_ok=yes tests_passed=0 tests_failed=0\n' "$mutation"
      printf 'ERROR mutation=%s status=NO-TEST-RAN rc=%d target=%s (named test did not execute)\n' "$mutation" "$rc" "$target" >&2
      cat "$log" >&2
      return 94
      ;;
  esac

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

run_mutation() {
  local mutation=$1 relative=$2 expected=$3 replacement=$4 target=$5 log rc command
  CURRENT_MUTATION="$mutation"
  restore_after_row
  apply_exact_mutation "$relative" "$expected" "$replacement"
  log="$(mktemp "${TMPDIR:-$REPO_ROOT/target}/relay-authority-${mutation}.XXXXXX")"
  command="cargo test --offline --lib $target -- --exact --test-threads=1"

  if run_target "$mutation" "$target" "$log"; then
    rc=0
  else
    rc=$?
  fi

  if ((rc == 0)); then
    printf 'MUTATION_RESULT mutation=%s status=SURVIVED rc=0 target=%s\n' "$mutation" "$target" >&2
    printf 'ERROR mutation survived: %s\nCOMMAND: %s\n' "$mutation" "$command" >&2
    cat "$log" >&2
    rm -f "$log"
    exit 1
  fi
  # 94/95/96 already streamed the full log to stderr inside run_target.
  if ((rc == 94 || rc == 95 || rc == 96)); then
    rm -f "$log"
    exit "$rc"
  fi

  printf 'MUTATION_RESULT mutation=%s status=KILLED rc=%d target=%s\n' "$mutation" "$rc" "$target"
  rm -f "$log"
  restore_after_row
}

mkdir -p "${TMPDIR:-$REPO_ROOT/target}" "$(dirname "$LOCK_DIR")"
trap on_exit EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
acquire_lock
prepare_backups
printf 'MUTATION_COUNT count=%d minimum=4\n' "$MUTATION_COUNT"

run_mutation \
  M10 "$TERMINAL_HANDOFF" \
  'delivery_frontier::SinkDeliveryProofResult::Persisted => Self::Delivered,' \
  'delivery_frontier::SinkDeliveryProofResult::Persisted => Self::NotDelivered,' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::relay_deliver_preserves_tail_anchor_and_observes_persisted_proof'

run_mutation \
  M6 "$TERMINAL_HANDOFF" \
  'terminal_not_delivered || fenced_terminal_without_delivery' \
  'terminal_not_delivered' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::fenced_terminal_without_parser_delivery_is_terminal_not_delivered'

run_mutation \
  M8 "$TERMINAL_HANDOFF" \
  'Err(error) => return Err(error),' \
  'Err(_error) => { terminal_not_delivered = true; }' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::relay_deliver_propagates_injected_transport_error'

run_mutation \
  anchor-drop "$SESSION_RELAY_SINK" \
  $'formatting::watcher_completion_footer_anchor(\n                        last_chunk_anchor.as_ref(),\n                        msg_id,\n                        &relay_text,\n                    )' \
  $'formatting::watcher_completion_footer_anchor(\n                        None,\n                        msg_id,\n                        &relay_text,\n                    )' \
  'services::discord::session_relay_sink::delivery_orchestration_tests::relay_deliver_preserves_tail_anchor_and_observes_persisted_proof'

# #5071 relay-tail S4 (I-1): neutralize the delivery-lease conjunct that both
# fenced registry CAS cores gate their commit through. The bound fence is still
# matched (just unused), and `commit` is still consumed exactly once, so the
# mutant compiles and the only thing that changes is the verdict.
run_mutation \
  S4-m5 "$WATCHER_REGISTRY" \
  '        Some(fence) => fence.commit_if_permitted(commit),' \
  '        Some(_fence) => Some(commit()),' \
  'services::discord::relay_recovery::tests::post_gate_identity_matched_live_delivery_lease_blocks_dead_frontier_watcher_cancel'

# #5071 relay-tail S4 (I-2a): restore the terminal-envelope early return ahead of
# the no-progress ladder, i.e. undo the demotion. The envelope is still present
# in the target's fixture, so the mutant short-circuits to Allowed before the
# reprobe ever observes the advancing relay frontier.
run_mutation \
  S4-m6 "$DESTRUCTIVE_CANCEL_GATE" \
  $'    let Some(expected_output_path) = snapshot.output_path.as_deref() else {' \
  $'    if terminal_envelope_present(provider, snapshot) {\n        return DestructiveCancelGate::Allowed("terminal_envelope_present");\n    }\n    let Some(expected_output_path) = snapshot.output_path.as_deref() else {' \
  'services::discord::destructive_cancel_gate::tests::terminal_envelope_does_not_outrank_relay_frontier_progress_on_reprobe'

# #5071 relay-tail S4 r2 (P1-1): reopen the r1 read/act split. The judgment
# still happens under the cell's payload mutex, but the mutex is now DROPPED on
# the way out of `with_state_locked` and the destruction runs after it, exactly
# as the r1 `permits_destruction` -> bool shape did. Every sequential verdict is
# unchanged, so only the atomicity target can see this: a racing acquirer wins
# the judged key inside the reopened window and still observes the registry row
# the judgment authorized destroying.
run_mutation \
  S4-m7 "$WATCHER_REGISTRY" \
  $'            #[cfg(test)]\n            run_delivery_fence_permitted_hook_for_tests(self.site);\n            Some(commit())\n        })\n    }' \
  $'            Some(())\n        })?;\n        #[cfg(test)]\n        run_delivery_fence_permitted_hook_for_tests(self.site);\n        Some(commit())\n    }' \
  'services::discord::tmux_watcher_registry_restore_tests::delivery_fence_judgment_and_destruction_are_atomic_against_a_racing_acquire'

printf 'MUTATION_SUMMARY killed=%d survived=0 minimum=4 status=PASS\n' "$MUTATION_COUNT"
