#!/usr/bin/env bash
set -euo pipefail

SELF_NAME="$(basename "$0")"
TMP_DIR="$(mktemp -d)"
SUMMARY_ROWS="$TMP_DIR/summary-rows.md"
PG_JOB_NAME="PostgreSQL tests (ubuntu-postgres)"
WINDOWS_ADVISORY_JOB_NAME="Fast check + non-PG tests (windows-latest)"
trap 'rm -rf "$TMP_DIR"' EXIT

: >"$SUMMARY_ROWS"

usage() {
  cat <<EOF
Usage: $SELF_NAME [--self-test]

Environment:
  GITHUB_REPOSITORY  owner/repository (required for a workflow run)
  RUN_ID             CI PR workflow run id
  RUN_ATTEMPT        attempt to classify (must be less than 3)
  RERUN_DRY_RUN      set to 1 to classify historical attempts without rerunning
EOF
}

require_cmd() {
  local name="$1"
  if ! command -v "$name" >/dev/null 2>&1; then
    echo "missing required command: $name" >&2
    exit 1
  fi
}

is_positive_integer() {
  [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

# Keep the first alternation exactly aligned with the validated termination
# regex in scripts/main-ci-triage.sh. The final alternation is deliberately
# narrow: it matches GitHub's step-level action timeout, not arbitrary timeout
# text from tests, dependencies, or cache statistics.
log_has_infra_failure() {
  local log_path="$1"
  [[ -s "$log_path" ]] || return 1
  grep -a -E -i -q -- \
    "signal[: ]+(9|15)([^0-9]|$)|sig(term|kill)|terminated on line [0-9]+ by signal|(exit(ed)?|code|status)[^0-9]*143([^0-9]|$)|the operation was cancell?ed|runner has received a shutdown signal|the action '.+' has timed out after [0-9]+ minutes" \
    "$log_path"
}

# Regression markers are checked across every failed job except the explicitly
# advisory Windows lane. This remains a separate guard from the infra predicate
# so mixed logs from required-context upstream jobs fail closed.
log_has_regression() {
  local log_path="$1"
  [[ -s "$log_path" ]] || return 1
  grep -a -E -i -q -- \
    'test result: FAILED|error\[E|panicked at' \
    "$log_path"
}

job_log_has_blocking_regression() {
  local job_name="$1"
  local log_path="$2"

  [[ "$job_name" != "$WINDOWS_ADVISORY_JOB_NAME" ]] || return 1
  log_has_regression "$log_path"
}

decide_retry() {
  local pg_failed_count="$1"
  local pg_classified_count="$2"
  local regression_count="$3"
  local unknown_count="$4"

  if (( unknown_count > 0 )); then
    printf 'no-op:unknown'
  elif (( regression_count > 0 )); then
    printf 'no-op:regression'
  elif (( pg_failed_count == 0 )); then
    printf 'no-op:no-pg-failure'
  elif (( pg_failed_count != 1 )); then
    printf 'no-op:ambiguous-pg-jobs'
  elif (( pg_classified_count != pg_failed_count )); then
    printf 'no-op:unclassified-pg-failure'
  else
    printf 'would-rerun:infra'
  fi
}

append_summary_row() {
  local job_id="$1"
  local job_class="$2"
  printf '%s\n' "| \`$job_id\` | \`$job_class\` |" >>"$SUMMARY_ROWS"
}

write_summary() {
  local run_id="$1"
  local run_attempt="$2"
  local decision="$3"
  local destination="${GITHUB_STEP_SUMMARY-}"

  [[ -n "$destination" ]] || return 0
  {
    printf '### CI PR infrastructure retry\n\n'
    printf '%s\n' "- Run: \`$run_id\`, attempt: \`$run_attempt\`"
    printf '%s\n\n' "- Decision: \`$decision\`"
    printf '| Failed job id | Classification |\n'
    printf '| --- | --- |\n'
    cat "$SUMMARY_ROWS"
  } >>"$destination"
}

validate_attempt_payload() {
  local payload="$1"
  local expected_attempt="$2"
  jq -e \
    --argjson attempt "$expected_attempt" \
    '.name == "CI PR" and .event == "pull_request" and .status == "completed" and .conclusion == "failure" and .run_attempt == $attempt' \
    "$payload" >/dev/null
}

latest_attempt_is_still_failed() {
  local repo="$1"
  local run_id="$2"
  local expected_attempt="$3"
  local payload="$TMP_DIR/latest-run.json"

  gh api "repos/$repo/actions/runs/$run_id" >"$payload" 2>/dev/null || return 1
  jq -e \
    --argjson attempt "$expected_attempt" \
    '.status == "completed" and .conclusion == "failure" and .run_attempt == $attempt' \
    "$payload" >/dev/null
}

run_classifier() {
  require_cmd gh
  require_cmd jq

  local repo="${GITHUB_REPOSITORY-}"
  local run_id="${RUN_ID-}"
  local run_attempt="${RUN_ATTEMPT-}"
  local dry_run="${RERUN_DRY_RUN:-0}"
  local attempt_payload="$TMP_DIR/attempt.json"
  local jobs_payload="$TMP_DIR/jobs.json"
  local decision="no-op:invalid-input"

  if [[ -z "$repo" ]] || ! is_positive_integer "$run_id" || ! is_positive_integer "$run_attempt"; then
    echo "invalid GITHUB_REPOSITORY, RUN_ID, or RUN_ATTEMPT" >&2
    write_summary "${run_id:-unknown}" "${run_attempt:-unknown}" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if (( run_attempt >= 3 )); then
    decision="no-op:attempt-cap"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! gh api "repos/$repo/actions/runs/$run_id/attempts/$run_attempt" >"$attempt_payload" 2>/dev/null; then
    decision="no-op:attempt-api-failure"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! validate_attempt_payload "$attempt_payload" "$run_attempt"; then
    decision="no-op:invalid-attempt"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! gh api "repos/$repo/actions/runs/$run_id/attempts/$run_attempt/jobs?per_page=100" >"$jobs_payload" 2>/dev/null; then
    decision="no-op:jobs-api-failure"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  local total_count
  local returned_count
  total_count="$(jq -r '.total_count // -1' "$jobs_payload")"
  returned_count="$(jq -r '.jobs | length' "$jobs_payload")"
  if ! is_positive_integer "$total_count" || [[ "$total_count" != "$returned_count" ]]; then
    decision="no-op:incomplete-jobs"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  local pg_failed_count=0
  local pg_classified_count=0
  local regression_count=0
  local unknown_count=0
  local pg_job_id=""
  local job_id job_name log_path job_class

  while IFS=$'\t' read -r job_id job_name; do
    [[ -n "$job_id" ]] || continue
    log_path="$TMP_DIR/job-$job_id.log"
    job_class="unrelated-failure"

    if ! gh api "repos/$repo/actions/jobs/$job_id/logs" >"$log_path" 2>/dev/null || [[ ! -s "$log_path" ]]; then
      unknown_count=$((unknown_count + 1))
      job_class="unknown"
      append_summary_row "$job_id" "$job_class"
      continue
    fi

    if job_log_has_blocking_regression "$job_name" "$log_path"; then
      regression_count=$((regression_count + 1))
      job_class="regression"
    elif log_has_regression "$log_path"; then
      job_class="advisory-regression"
    elif log_has_infra_failure "$log_path"; then
      job_class="infra-unrelated"
    fi

    if [[ "$job_name" == "$PG_JOB_NAME" ]]; then
      pg_failed_count=$((pg_failed_count + 1))
      pg_job_id="$job_id"
      if log_has_infra_failure "$log_path"; then
        pg_classified_count=$((pg_classified_count + 1))
        if [[ "$job_class" != "regression" ]]; then
          if grep -a -E -i -q -- "the action '.+' has timed out after [0-9]+ minutes" "$log_path"; then
            job_class="infra-timeout"
          else
            job_class="infra-shutdown"
          fi
        fi
      elif [[ "$job_class" == "regression" ]]; then
        # A regression is a complete classification, but the separate global
        # regression-count guard must block it before the retry decision. This
        # conjunctive shape makes that guard independently load-bearing while
        # still requiring every non-regression PG log to match infra.
        pg_classified_count=$((pg_classified_count + 1))
      else
        job_class="unclassified-pg-failure"
      fi
    fi

    append_summary_row "$job_id" "$job_class"
  done < <(jq -r '.jobs[] | select(.conclusion == "failure") | [(.id | tostring), .name] | @tsv' "$jobs_payload")

  decision="$(decide_retry "$pg_failed_count" "$pg_classified_count" "$regression_count" "$unknown_count")"
  if [[ "$decision" != "would-rerun:infra" ]]; then
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if [[ "$dry_run" == "1" ]]; then
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision pg_job_id=$pg_job_id"
    return 0
  fi

  if ! latest_attempt_is_still_failed "$repo" "$run_id" "$run_attempt"; then
    decision="no-op:stale-attempt"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision"
    return 0
  fi

  if ! gh run rerun "$run_id" --repo "$repo" --job "$pg_job_id"; then
    decision="no-op:rerun-request-failed"
    write_summary "$run_id" "$run_attempt" "$decision"
    echo "decision=$decision pg_job_id=$pg_job_id" >&2
    return 1
  fi
  decision="rerun-requested:infra"
  write_summary "$run_id" "$run_attempt" "$decision"
  echo "decision=$decision pg_job_id=$pg_job_id"
}

assert_equal() {
  local expected="$1"
  local actual="$2"
  local label="$3"
  if [[ "$actual" != "$expected" ]]; then
    echo "assertion failed ($label): expected '$expected', got '$actual'" >&2
    exit 1
  fi
}

run_self_test() {
  local infra_log="$TMP_DIR/selftest-infra.log"
  local timeout_log="$TMP_DIR/selftest-timeout.log"
  local regression_log="$TMP_DIR/selftest-regression.log"
  local mixed_log="$TMP_DIR/selftest-mixed.log"
  local run_29067936620_windows_log="$TMP_DIR/selftest-run-29067936620-windows.log"
  local run_29067936620_pg_log="$TMP_DIR/selftest-run-29067936620-postgres.log"

  printf '%s\n' 'The runner has received a shutdown signal.' 'Error: Process completed with exit code 143.' >"$infra_log"
  printf '%s\n' "The action 'just test-postgres' has timed out after 15 minutes." >"$timeout_log"
  printf '%s\n' 'thread panicked at src/example.rs:42' 'test result: FAILED. 1 passed; 1 failed' >"$regression_log"
  # #4392 mutation fixture: a real regression can also carry shutdown cleanup
  # noise. Both predicates intentionally match; the explicit regression-count
  # guard in decide_retry must win.
  cp "$regression_log" "$mixed_log"
  printf '%s\n' 'The runner has received a shutdown signal.' >>"$mixed_log"
  # Run 29067936620 attempt 1: the advisory Windows job panicked while the PG
  # job exited 143 during runner shutdown. The PG failure remains rerunnable.
  printf '%s\n' \
    'thread panicked at src\services\routines\store.rs:4244:9' \
    'test result: FAILED. 163 passed; 1 failed' \
    >"$run_29067936620_windows_log"
  printf '%s\n' \
    'Error: Process completed with exit code 143.' \
    'The runner has received a shutdown signal.' \
    >"$run_29067936620_pg_log"

  log_has_infra_failure "$infra_log" || { echo "assertion failed: shutdown must be infrastructure" >&2; exit 1; }
  log_has_infra_failure "$timeout_log" || { echo "assertion failed: action timeout must be infrastructure" >&2; exit 1; }
  log_has_regression "$regression_log" || { echo "assertion failed: regression markers must be detected" >&2; exit 1; }
  log_has_infra_failure "$mixed_log" || { echo "assertion failed: mixed log must contain infrastructure" >&2; exit 1; }
  log_has_regression "$mixed_log" || { echo "assertion failed: mixed log must contain regression" >&2; exit 1; }
  log_has_regression "$run_29067936620_windows_log" || { echo "assertion failed: run 29067936620 Windows fixture must contain regression" >&2; exit 1; }
  log_has_infra_failure "$run_29067936620_pg_log" || { echo "assertion failed: run 29067936620 PG fixture must contain infrastructure" >&2; exit 1; }

  assert_equal "would-rerun:infra" "$(decide_retry 1 1 0 0)" "infra-only rerun"
  assert_equal "no-op:regression" "$(decide_retry 1 1 1 0)" "mixed regression guard"
  assert_equal "no-op:unknown" "$(decide_retry 1 1 0 1)" "unknown log guard"
  assert_equal "no-op:no-pg-failure" "$(decide_retry 0 0 0 0)" "vacuous truth guard"
  assert_equal "no-op:ambiguous-pg-jobs" "$(decide_retry 2 2 0 0)" "ambiguous target guard"
  assert_equal "no-op:unclassified-pg-failure" "$(decide_retry 1 0 0 0)" "unclassified guard"

  local historical_regression_count=0
  if job_log_has_blocking_regression "$WINDOWS_ADVISORY_JOB_NAME" "$run_29067936620_windows_log"; then
    historical_regression_count=$((historical_regression_count + 1))
  fi
  assert_equal \
    "would-rerun:infra" \
    "$(decide_retry 1 1 "$historical_regression_count" 0)" \
    "run 29067936620 advisory regression reclassification"

  local pg_regression_count=0
  if job_log_has_blocking_regression "$PG_JOB_NAME" "$regression_log"; then
    pg_regression_count=$((pg_regression_count + 1))
  fi
  assert_equal \
    "no-op:regression" \
    "$(decide_retry 1 1 "$pg_regression_count" 0)" \
    "PG regression remains fail-closed"

  echo "self-test passed"
}

main() {
  case "${1-}" in
    --self-test)
      run_self_test
      ;;
    "")
      run_classifier
      ;;
    -h|--help)
      usage
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
}

main "${1-}"
