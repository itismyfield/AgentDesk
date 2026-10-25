#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULTS_SH="$REPO_ROOT/scripts/_defaults.sh"
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
export AGENTDESK_RESTART_LEGACY_INDEX_COMPAT=1
TMP_TEST=$(mktemp -d)
trap 'rm -r "$TMP_TEST"' EXIT
PASS=0

assert_eq() {
  if [ "$2" != "$3" ]; then
    printf 'FAIL: %s\nexpected: %s\nactual: %s\n' "$1" "$2" "$3" >&2
    exit 1
  fi
}

assert_line() {
  if ! grep -Fqx -- "$1" "$TMP_TEST/stderr"; then
    printf 'FAIL: missing diagnostic: %s\n' "$1" >&2
    cat "$TMP_TEST/stderr" >&2
    exit 1
  fi
}

new_request() {
  ROOT="$TMP_TEST/$1 marker root"
  mkdir -p "$ROOT"
  _restart_stage_and_link_marker "$ROOT" expected test scope label
  : >"$TMP_TEST/sleeps"
}

sleep() { printf '%s\n' "$1" >>"$TMP_TEST/sleeps"; }

capture_gate() {
  RC=0
  wait_for_restart_persistence_or_fail test "$ROOT" expected "${1:-1}" \
    >"$TMP_TEST/stdout" 2>"$TMP_TEST/stderr" || RC=$?
}

assert_refused() {
  local header
  assert_eq 'refusal exit code' 1 "$RC"
  assert_eq 'refusal stdout' '' "$(cat "$TMP_TEST/stdout")"
  assert_line "✗ [gate] test restart persistence was not acknowledged within ${1:-1}s"
  assert_line '  Cleared restart_pending and refused bootout: the in-flight delivery frontier is not durable.'
  printf -v header '  restart persistence marker_root=%q nonce=%s' "$ROOT" expected
  assert_line "$header"
  [ ! -e "$ROOT/restart_pending" ] && [ ! -e "$ROOT/restart_pending.expected" ] \
    || { echo 'FAIL: request marker was not cleared' >&2; exit 1; }
  _restart_artifact_nonce_matches "$ROOT/restart_cancelled.expected" expected \
    || { echo 'FAIL: cancellation proof missing' >&2; exit 1; }
  assert_eq 'wait count and duration' "${2-1}" "$(cat "$TMP_TEST/sleeps")"
}

passed() { PASS=$((PASS + 1)); printf 'PASS: %s\n' "$1"; }

new_request missing
capture_gate
assert_refused
assert_line '  last proof observation: file=restart_persisted.expected state=missing'
assert_line '  last proof observation: file=restart_persisted state=missing'
passed 'missing identity and fixed-index markers'

new_request mismatch
printf 'nonce=other-request\n' >"$ROOT/restart_persisted.expected"
printf 'nonce=older-request\n' >"$ROOT/restart_persisted"
capture_gate
assert_refused
assert_line '  last proof observation: file=restart_persisted.expected state=nonce-mismatch observed_nonce=other-request'
assert_line '  last proof observation: file=restart_persisted state=nonce-mismatch observed_nonce=older-request'
passed 'identity and fixed-index nonce mismatches'

new_request malformed
printf 'provider=codex\nTOKEN=do-not-print\n' >"$ROOT/restart_persisted.expected"
printf 'nonce=postgres://secret:password@db/private\n' >"$ROOT/restart_persisted"
capture_gate
assert_refused
assert_line '  last proof observation: file=restart_persisted.expected state=malformed reason=missing-nonce'
assert_line '  last proof observation: file=restart_persisted state=malformed reason=invalid-nonce'
if grep -Eq 'TOKEN|do-not-print|postgres|secret|password|private' "$TMP_TEST/stderr"; then
  echo 'FAIL: marker contents leaked' >&2
  exit 1
fi
passed 'malformed nonce and missing nonce without secret contents'

new_request nonregular
mkdir "$ROOT/restart_persisted.expected"
capture_gate
assert_refused
assert_line '  last proof observation: file=restart_persisted.expected state=not-regular-file'
passed 'nonregular artifact'

new_request unreadable
printf 'nonce=other-request\n' >"$ROOT/restart_persisted.expected"
(
  grep() {
    local arg
    for arg in "$@"; do
      case "$arg" in "$ROOT"/restart_persisted*) return 2 ;; esac
    done
    command grep "$@"
  }
  capture_gate
  unset -f grep
  assert_refused
  assert_line '  last proof observation: file=restart_persisted.expected state=unreadable'
)
passed 'read failure is reported without changing refusal'

new_request publication-race
(
  _restart_artifact_nonce_matches() {
    if [ "$1" = "$ROOT/restart_persisted.expected" ]; then
      printf 'nonce=expected\n' >"$1"
      return 1
    fi
    [ -f "$1" ] && command grep -Fqx -- "nonce=$2" "$1" 2>/dev/null
  }
  capture_gate
  assert_refused
  assert_line '  last proof observation: file=restart_persisted.expected state=nonce-observed-after-failed-check'
)
passed 'publication during diagnostics is observational and cannot pass the gate'

new_request legacy-disabled
printf 'nonce=expected\n' >"$ROOT/restart_persisted"
AGENTDESK_RESTART_LEGACY_INDEX_COMPAT=0 capture_gate
assert_refused
assert_line '  last proof observation: file=restart_persisted state=legacy-index-disabled'
passed 'disabled compatibility index stays refused'

new_request last-observation
sleep() {
  printf '%s\n' "$1" >>"$TMP_TEST/sleeps"
  printf 'nonce=expected\n' >"$ROOT/restart_persisted.expected"
}
capture_gate
assert_refused
assert_line '  last proof observation: file=restart_persisted.expected state=missing'
passed 'final sleep does not replace the last failed observation or add a proof check'
sleep() { printf '%s\n' "$1" >>"$TMP_TEST/sleeps"; }

for proof in identity legacy-index; do
  new_request "$proof"
  artifact=restart_persisted
  [ "$proof" != identity ] || artifact=restart_persisted.expected
  printf 'nonce=expected\n' >"$ROOT/$artifact"
  capture_gate
  assert_eq 'success code' 0 "$RC"
  assert_eq 'success does not sleep' '' "$(cat "$TMP_TEST/sleeps")"
  if [ "$proof" = identity ]; then
    assert_eq 'identity stdout unchanged' \
      "✓ [gate] test restart persistence acknowledged by runtime at $ROOT proof=identity" \
      "$(cat "$TMP_TEST/stdout")"
    assert_eq 'identity stderr unchanged' '' "$(cat "$TMP_TEST/stderr")"
  else
    assert_eq 'legacy stdout unchanged' '' "$(cat "$TMP_TEST/stdout")"
    assert_eq 'legacy stderr unchanged' \
      "⚠ [gate] test restart persistence acknowledged by rollback-compatible fixed index at $ROOT proof=legacy-index" \
      "$(cat "$TMP_TEST/stderr")"
  fi
  [ -e "$ROOT/restart_pending" ] && [ ! -e "$ROOT/restart_cancelled.expected" ] \
    || { echo 'FAIL: success changed request markers' >&2; exit 1; }
  passed "$proof success output, timing and markers unchanged"
done

new_request zero-wait
capture_gate 0
assert_refused 0 ''
assert_line '  last proof observation: unavailable (no completed check or diagnostic helper failed)'
passed 'zero wait remains refused with no sleep and no unbound variable'

mkdir -p "$TMP_TEST/copy/lib"
cp "$DEFAULTS_SH" "$TMP_TEST/copy/_defaults.sh"
for fault in 'exit 0' 'return 71' 'printf "%s" "$UNSET_DIAGNOSTIC_VARIABLE"' missing; do
  new_request "helper-fault-$PASS"
  printf '_restart_persistence_failure_snapshot() { %s; }\n' "$fault" \
    >"$TMP_TEST/copy/lib/restart_persistence_diag.sh"
  [ "$fault" != missing ] || rm "$TMP_TEST/copy/lib/restart_persistence_diag.sh"
  RC=0
  bash -euo pipefail -c '
    . "$1"
    SLEEPS="$3"
    sleep() { printf "%s\n" "$1" >>"$SLEEPS"; }
    if wait_for_restart_persistence_or_fail test "$2" expected 1; then
      exit 0
    else
      exit "$?"
    fi
  ' bash "$TMP_TEST/copy/_defaults.sh" "$ROOT" "$TMP_TEST/sleeps" \
    >"$TMP_TEST/stdout" 2>"$TMP_TEST/stderr" || RC=$?
  assert_refused
  passed "diagnostic helper failure cannot bypass refusal: $fault"
done

printf 'RESULT: %s passed, 0 failed, 0 skipped\n' "$PASS"
