#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/required-check-mirror.sh"
OUT_FILE="$(mktemp "${TMPDIR:-/tmp}/required-check-mirror.XXXXXX")"
trap 'rm -f "$OUT_FILE"' EXIT

run_mirror() {
  local changes_result="$1"
  local filter_output="$2"
  local upstream_result="$3"

  env \
    CHANGED_PATHS_RESULT="$changes_result" \
    FILTER_NAME="rust_or_policy" \
    FILTER_OUTPUT="$filter_output" \
    UPSTREAM_JOB_NAME="check_fast" \
    UPSTREAM_RESULT="$upstream_result" \
    "$SCRIPT" >"$OUT_FILE" 2>&1
}

expect_pass() {
  local name="$1"
  shift
  if run_mirror "$@"; then
    echo "ok - $name"
  else
    echo "not ok - $name" >&2
    cat "$OUT_FILE" >&2
    exit 1
  fi
}

expect_fail() {
  local name="$1"
  shift
  if run_mirror "$@"; then
    echo "not ok - $name" >&2
    cat "$OUT_FILE" >&2
    exit 1
  fi
  echo "ok - $name"
}

expect_pass "passes when the filter requires the job and it succeeds" success true success
expect_pass "passes when the filter explicitly skips the job" success false skipped
expect_pass "passes when a filtered-out job still succeeded" success false success
expect_fail "fails closed when Changed paths fails even if upstream was skipped" failure false skipped
expect_fail "fails closed when the filter required the job but it was skipped" success true skipped
expect_fail "fails closed when the filter output is missing" success "" skipped
expect_fail "fails closed when a filtered-out job failed" success false failure
