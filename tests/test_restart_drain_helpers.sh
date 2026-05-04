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
# a curl shim that *models* `-f` behavior — refuses to print the body when
# called with -f / --fail (returning 22 like curl) but prints the body and
# exits 0 otherwise. Helper must not pass -f, so this test passes only when
# the helper accepts the body delivered without -f.
mkdir -p "$TMP_FIXTURE_DIR/bin_503"
cat >"$TMP_FIXTURE_DIR/bin_503/curl" <<EOF
#!/usr/bin/env bash
# Test shim — refuse to deliver body if caller passed -f or --fail.
for arg in "\$@"; do
  case "\$arg" in
    -f|--fail|*-*f*)
      # Match real curl behaviour on 5xx with -f: no body, exit 22.
      case "\$arg" in
        -f|--fail) exit 22 ;;
      esac
      # Bundled short flags like -sf.
      if [ "\${arg#-}" != "\$arg" ] && [ "\${arg#--}" = "\$arg" ]; then
        case "\$arg" in *f*) exit 22 ;; esac
      fi
      ;;
  esac
done
cat "$RESP_FILE"
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_503/curl"
set +e
PATH="$TMP_FIXTURE_DIR/bin_503:$PATH" _restart_pending_acknowledged 0 >/dev/null 2>&1
rc=$?
set -e
assert_eq "ack reads body even when runtime would return 503 (no curl -f)" "0" "$rc"

# Sanity: confirm the same shim DOES fail if invoked with -f, so a future
# regression that re-introduces `curl -sf` would actually be caught.
set +e
PATH="$TMP_FIXTURE_DIR/bin_503:$PATH" curl -sf --max-time 1 "http://x" >/dev/null 2>&1
shim_with_f_rc=$?
PATH="$TMP_FIXTURE_DIR/bin_503:$PATH" curl -s --max-time 1 "http://x" >/dev/null 2>&1
shim_without_f_rc=$?
set -e
assert_eq "503 shim exits 22 when called with -sf (catches regression)" "22" "$shim_with_f_rc"
assert_eq "503 shim exits 0 when called without -f" "0" "$shim_without_f_rc"

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

echo "== Test 5c: health_turn_snapshot fails closed when counters absent =="
# Regression for #1447 review iteration 4 P2: previously a redacted body
# (no global_active / global_finalizing) silently defaulted to "0 active",
# which let strict-drain callers (AGENTDESK_SKIP_TURN_DRAIN=0) bypass the
# wait. Now health_turn_snapshot must return non-zero so the caller fails
# closed and refuses to restart.
mkdir -p "$TMP_FIXTURE_DIR/bin_redacted"
cat >"$TMP_FIXTURE_DIR/bin_redacted/curl" <<'EOF'
#!/usr/bin/env bash
# Mimic the public_health_json shape: status/version present, no counters.
printf '%s' '{"status":"unhealthy","version":"x","db":true,"dashboard":false}'
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_redacted/curl"
set +e
PATH="$TMP_FIXTURE_DIR/bin_redacted:$PATH" health_turn_snapshot 0 >/dev/null 2>&1
rc=$?
set -e
assert_eq "snapshot returns 1 when global_active is absent" "1" "$rc"

echo "== Test 5d: snapshot returns counters when present (auth-aware) =="
mkdir -p "$TMP_FIXTURE_DIR/bin_full"
cat >"$TMP_FIXTURE_DIR/bin_full/curl" <<'EOF'
#!/usr/bin/env bash
# Verify the Origin header the helper sends — auth_middleware accepts
# same-origin requests on auth-enabled deployments. Fail if missing.
saw_origin=0
for arg in "$@"; do
  case "$arg" in
    Origin:*) saw_origin=1 ;;
  esac
done
if [ "$saw_origin" != "1" ]; then
  echo "MISSING_ORIGIN_HEADER" >&2
  exit 33
fi
printf '%s' '{"global_active":2,"global_finalizing":1,"queue_depth":3}'
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_full/curl"
set +e
out=$(PATH="$TMP_FIXTURE_DIR/bin_full:$PATH" health_turn_snapshot 0 2>/dev/null)
rc=$?
set -e
assert_eq "snapshot returns 0 with counters present + Origin sent" "0" "$rc"
assert_eq "snapshot prints 'active finalizing queue_depth'" "2 1 3" "$out"

echo "== Test 5e: request helper clears marker if launchd job is stopped =="
# Regression for #1447 review iteration 4 P2: previously the not-running
# branch returned success but left restart_pending on disk, causing the
# next cold boot to drain-and-self-exit (KeepAlive flap).
TMP_RUNTIME2=$(mktemp -d)
trap 'rm -rf "$TMP_FIXTURE_DIR" "$TMP_RUNTIME" "$TMPDIR_TEST" "$TMP_RUNTIME2"' EXIT
mkdir -p "$TMP_FIXTURE_DIR/bin_unreach"
cat >"$TMP_FIXTURE_DIR/bin_unreach/curl" <<'EOF'
#!/usr/bin/env bash
exit 7
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_unreach/curl"
_launchd_job_state() { echo "not running"; }
set +e
PATH="$TMP_FIXTURE_DIR/bin_unreach:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=2 \
  request_restart_drain_mode_or_fail "test" "stopped.label" 0 "$TMP_RUNTIME2" "smoke-test" \
  >/dev/null 2>&1
rc=$?
set -e
unset -f _launchd_job_state
assert_eq "request returns 0 when job not running" "0" "$rc"
if [ ! -e "$TMP_RUNTIME2/restart_pending" ]; then
  pass "marker removed when job is not running (no flap on next boot)"
else
  fail "marker removed when job is not running (no flap on next boot)"
fi

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

echo "== Test 7: #1686 — wait_for_live_turns_to_drain_or_fail self-hosted/skip semantics =="
# 7a: skip=1 + live_turns>0 → returns 0 immediately (no max_wait stall).
# Stub health_turn_snapshot to report a stable live count; if the helper
# still entered the wait loop the test would take >max_wait seconds.
mkdir -p "$TMP_FIXTURE_DIR/bin_skip1"
cat >"$TMP_FIXTURE_DIR/bin_skip1/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s' '{"global_active":1,"global_finalizing":0,"queue_depth":0}'
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_skip1/curl"
_launchd_job_state() { echo "running"; }
start_ts=$(date +%s)
set +e
PATH="$TMP_FIXTURE_DIR/bin_skip1:$PATH" \
  AGENTDESK_SKIP_TURN_DRAIN=1 \
  wait_for_live_turns_to_drain_or_fail "release" "test.label" 0 30 2 \
  >/dev/null 2>&1
rc=$?
set -e
elapsed=$(( $(date +%s) - start_ts ))
unset -f _launchd_job_state
assert_eq "skip=1 returns 0 with live turn" "0" "$rc"
if [ "$elapsed" -lt 5 ]; then
  pass "skip=1 short-circuits without entering wait loop (elapsed=${elapsed}s < 5)"
else
  fail "skip=1 short-circuits without entering wait loop (elapsed=${elapsed}s)"
fi

# 7b: self-hosted detached child with exactly 1 live turn (the operator's
# own deploy turn) → effective_live=0, returns 0 even under skip=0 strict.
mkdir -p "$TMP_FIXTURE_DIR/bin_self1"
cat >"$TMP_FIXTURE_DIR/bin_self1/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s' '{"global_active":1,"global_finalizing":0,"queue_depth":0}'
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_self1/curl"
_launchd_job_state() { echo "running"; }
set +e
PATH="$TMP_FIXTURE_DIR/bin_self1:$PATH" \
  AGENTDESK_SKIP_TURN_DRAIN=0 \
  AGENTDESK_DEPLOY_DETACHED_CHILD=1 \
  AGENTDESK_REPORT_CHANNEL_ID=99999999999999 \
  wait_for_live_turns_to_drain_or_fail "release" "test.label" 0 5 1 \
  >/dev/null 2>&1
rc=$?
set -e
unset -f _launchd_job_state
assert_eq "skip=0 + self-hosted self-turn = treated as drained" "0" "$rc"

# 7c: skip=0 + 2 live turns + self-hosted (1 attributable to self) →
# effective_live=1 → enters wait loop and times out → returns 1.
mkdir -p "$TMP_FIXTURE_DIR/bin_self2"
cat >"$TMP_FIXTURE_DIR/bin_self2/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s' '{"global_active":2,"global_finalizing":0,"queue_depth":0}'
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_self2/curl"
_launchd_job_state() { echo "running"; }
set +e
PATH="$TMP_FIXTURE_DIR/bin_self2:$PATH" \
  AGENTDESK_SKIP_TURN_DRAIN=0 \
  AGENTDESK_DEPLOY_DETACHED_CHILD=1 \
  AGENTDESK_REPORT_CHANNEL_ID=99999999999999 \
  wait_for_live_turns_to_drain_or_fail "release" "test.label" 0 4 1 \
  >/dev/null 2>&1
rc=$?
set -e
unset -f _launchd_job_state
assert_eq "skip=0 + extra non-self live turn → strict timeout returns 1" "1" "$rc"

# 7d: skip=1 + no live turns → returns 0 with normal "no active/finalizing" path.
mkdir -p "$TMP_FIXTURE_DIR/bin_zero"
cat >"$TMP_FIXTURE_DIR/bin_zero/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s' '{"global_active":0,"global_finalizing":0,"queue_depth":0}'
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_zero/curl"
_launchd_job_state() { echo "running"; }
set +e
PATH="$TMP_FIXTURE_DIR/bin_zero:$PATH" \
  AGENTDESK_SKIP_TURN_DRAIN=1 \
  wait_for_live_turns_to_drain_or_fail "release" "test.label" 0 5 1 \
  >/dev/null 2>&1
rc=$?
set -e
unset -f _launchd_job_state
assert_eq "skip=1 + zero live turns returns 0" "0" "$rc"

echo
echo "==== Results ===="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '  failed: %s\n' "${FAIL_NAMES[@]}" >&2
  exit 1
fi
exit 0
