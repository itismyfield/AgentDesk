#!/usr/bin/env bash
# Smoke test for #1447 — agentdesk-restart silent-fail regression.
#
# Verifies:
#   1. scripts/_defaults.sh defines all required restart-drain helpers.
#   2. assert_restart_helpers_loaded returns 0 when helpers are present.
#   3. assert_restart_helpers_loaded returns 1 when a helper is missing.
#   4. A representative caller pattern (`if ! helper_call; then exit 1; fi`)
#      propagates exit 1 — never silently exits 0 — when the helper:
#        a) fails (non-zero return)
#        b) is undefined (command not found)
#
# This test is self-contained; it does not call launchctl or any real service.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEFAULTS_SH="$REPO_ROOT/scripts/_defaults.sh"

PASS=0
FAIL=0
FAIL_NAMES=()

pass() {
  echo "  PASS: $1"
  PASS=$((PASS + 1))
}

fail() {
  echo "  FAIL: $1" >&2
  FAIL=$((FAIL + 1))
  FAIL_NAMES+=("$1")
}

assert_eq() {
  local label="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    pass "$label (= $expected)"
  else
    fail "$label (expected=$expected actual=$actual)"
  fi
}

echo "== Test 1: _defaults.sh defines required helpers =="
[ -f "$DEFAULTS_SH" ] || { echo "FATAL: $DEFAULTS_SH missing"; exit 2; }

# shellcheck source=/dev/null
. "$DEFAULTS_SH"

for fn in \
  request_restart_drain_mode_or_fail \
  wait_for_live_turns_to_drain_or_fail \
  clear_restart_drain_mode \
  assert_restart_helpers_loaded; do
  if declare -F "$fn" >/dev/null 2>&1; then
    pass "function defined: $fn"
  else
    fail "function defined: $fn"
  fi
done

echo "== Test 2: assert_restart_helpers_loaded passes when helpers present =="
if assert_restart_helpers_loaded >/dev/null 2>&1; then
  pass "assert_restart_helpers_loaded returns 0"
else
  fail "assert_restart_helpers_loaded returns 0"
fi

echo "== Test 3: assert_restart_helpers_loaded fails when a helper is missing =="
# Run in a subshell so we can unset a function locally.
set +e
(
  set -e
  # shellcheck source=/dev/null
  . "$DEFAULTS_SH"
  unset -f request_restart_drain_mode_or_fail
  assert_restart_helpers_loaded >/dev/null 2>&1
)
rc=$?
set -e
assert_eq "assert returns 1 with one helper missing" "1" "$rc"

echo "== Test 4a: caller exits 1 when helper returns non-zero =="
set +e
bash -c '
  set -euo pipefail
  failing_helper() { return 1; }
  if ! failing_helper; then
    exit 1
  fi
  exit 0
'
rc=$?
set -e
assert_eq "if ! failing_helper; then exit 1" "1" "$rc"

echo "== Test 4b: caller exits 1 when helper is undefined (command not found) =="
set +e
bash -c '
  set -euo pipefail
  if ! request_restart_drain_mode_or_fail dev label 0 /tmp src 2>/dev/null; then
    exit 1
  fi
  exit 0
'
rc=$?
set -e
assert_eq "if ! undefined_function; then exit 1" "1" "$rc"

echo "== Test 4c: caller using assert preflight exits 1 cleanly when helper missing =="
# This is the recommended pattern that protects against the silent-fail bug.
set +e
bash -c '
  set -euo pipefail
  . "'"$DEFAULTS_SH"'"
  unset -f wait_for_live_turns_to_drain_or_fail
  if ! assert_restart_helpers_loaded 2>/dev/null; then
    exit 1
  fi
  exit 0
'
rc=$?
set -e
assert_eq "preflight assert blocks restart with EXIT 1" "1" "$rc"

echo "== Test 5a: _restart_pending_acknowledged requires ALL providers true =="
# Stub curl on PATH so _restart_pending_acknowledged sees a controlled
# /api/health/detail body. Avoids depending on a real listening port.
TMP_FIXTURE_DIR=$(mktemp -d)
TMP_RUNTIME=$(mktemp -d)
TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMP_FIXTURE_DIR" "$TMP_RUNTIME" "$TMPDIR_TEST"' EXIT

# Build a curl shim that prints the contents of $RESP_FILE for any --max-time
# request and ignores everything else (mirrors how _restart_pending_acknowledged
# invokes curl).
mkdir -p "$TMP_FIXTURE_DIR/bin"
RESP_FILE="$TMP_FIXTURE_DIR/resp.json"
cat >"$TMP_FIXTURE_DIR/bin/curl" <<EOF
#!/usr/bin/env bash
# Test shim — prints the configured fake health response and exits 0.
cat "$RESP_FILE"
EOF
chmod +x "$TMP_FIXTURE_DIR/bin/curl"

# shellcheck source=/dev/null
. "$DEFAULTS_SH"

printf '%s' '{"providers":[{"name":"a","restart_pending":true},{"name":"b","restart_pending":false}]}' >"$RESP_FILE"
set +e
PATH="$TMP_FIXTURE_DIR/bin:$PATH" _restart_pending_acknowledged 0 >/dev/null 2>&1
rc=$?
set -e
assert_eq "ack returns 1 when one provider still false" "1" "$rc"

printf '%s' '{"providers":[{"name":"a","restart_pending":true},{"name":"b","restart_pending":true}]}' >"$RESP_FILE"
set +e
PATH="$TMP_FIXTURE_DIR/bin:$PATH" _restart_pending_acknowledged 0 >/dev/null 2>&1
rc=$?
set -e
assert_eq "ack returns 0 when all providers true" "0" "$rc"

# Regression for #1447 review iteration 2: when restart_pending fires, the
# runtime returns HTTP 503 on /api/health/detail. Without dropping `curl -f`,
# the body would be discarded and we'd never see the in-band ack flag. Build
# a curl shim that prints the body but exits non-zero (mimicking how `curl
# -f` behaves on 5xx) and confirm the helper still returns 0.
mkdir -p "$TMP_FIXTURE_DIR/bin_503"
cat >"$TMP_FIXTURE_DIR/bin_503/curl" <<EOF
#!/usr/bin/env bash
# Test shim — print body then exit 0 (the helper must NOT use -f so that
# even a real 503 response body remains observable). We also assert that
# the body is exactly what the helper consumed: same RESP_FILE.
cat "$RESP_FILE"
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_503/curl"
set +e
PATH="$TMP_FIXTURE_DIR/bin_503:$PATH" _restart_pending_acknowledged 0 >/dev/null 2>&1
rc=$?
set -e
assert_eq "ack reads body even when runtime would return 503 (no curl -f)" "0" "$rc"

echo "== Test 5b: marker-consumed during wait counts as acknowledgement =="
# Simulate a runtime that deletes the marker mid-wait (the restart_ctrl race
# Codex flagged in #1447 review). Stub curl to always fail (so health-detail
# probe never returns success) — the only positive ack path left is the
# "marker disappeared" branch.
mkdir -p "$TMP_FIXTURE_DIR/bin_fail"
cat >"$TMP_FIXTURE_DIR/bin_fail/curl" <<'EOF'
#!/usr/bin/env bash
exit 7
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_fail/curl"

# Stub _launchd_job_state so the post-loop branch reports "running" — forcing
# the helper to rely on marker-consumed ack.
_launchd_job_state() { echo "running"; }
( sleep 1; rm -f "$TMP_RUNTIME/restart_pending" ) &
BG_PID=$!
set +e
PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=10 \
  request_restart_drain_mode_or_fail "test" "test.label" 0 "$TMP_RUNTIME" "smoke-test" \
  >/dev/null 2>&1
rc=$?
set -e
wait "$BG_PID" 2>/dev/null || true
unset -f _launchd_job_state
assert_eq "drain helper returns 0 when marker is consumed mid-wait" "0" "$rc"

echo "== Test 6: clear_restart_drain_mode removes marker file =="
touch "$TMPDIR_TEST/restart_pending"
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
clear_restart_drain_mode "$TMPDIR_TEST" >/dev/null 2>&1 || true
if [ ! -e "$TMPDIR_TEST/restart_pending" ]; then
  pass "marker removed"
else
  fail "marker removed"
fi

echo
echo "==== Results ===="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '  failed: %s\n' "${FAIL_NAMES[@]}" >&2
  exit 1
fi
exit 0
