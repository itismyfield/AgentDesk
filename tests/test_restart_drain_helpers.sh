#!/usr/bin/env bash
# shellcheck disable=SC2218
# SC2218 is a structural false positive here: seam tests define an `rm()` shim
# (declare -f save -> define -> exercise -> `unset -f rm` -> restore), so `rm`
# calls outside those bounded regions target the real binary, not the shim.
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

if [ "${RESTART_S3B_ONLY:-0}" != "1" ]; then
for fn in \
  request_restart_drain_mode_or_fail \
  wait_for_restart_persistence_or_fail \
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
( for _ in $(seq 1 50); do
    [ -e "$TMP_RUNTIME/restart_pending" ] && break
    sleep 0.1
  done
  rm -f "$TMP_RUNTIME/restart_pending" ) &
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
assert_eq "snapshot prints 'active finalizing queue_depth runtime_active'" "2 1 3 0" "$out"

echo "== Test 5e: provider-active evidence blocks queued-only restart classification =="
mkdir -p "$TMP_FIXTURE_DIR/bin_provider_active"
cat >"$TMP_FIXTURE_DIR/bin_provider_active/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s' '{"global_active":0,"global_finalizing":0,"queue_depth":1,"providers":[{"name":"claude","active_turns":1,"queue_depth":1}],"mailboxes":[{"relay_stall_state":"active_foreground_stream","relay_health":{"bridge_inflight_present":true},"watcher_attached":true}]}'
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_provider_active/curl"
set +e
out=$(PATH="$TMP_FIXTURE_DIR/bin_provider_active:$PATH" health_turn_snapshot 0 2>/dev/null)
rc=$?
set -e
assert_eq "snapshot detects runtime active evidence even when global_active is zero" "0" "$rc"
assert_eq "snapshot prints runtime_active=1 for provider/mailbox evidence" "0 0 1 1" "$out"
_launchd_job_state() { echo "running"; }
set +e
PATH="$TMP_FIXTURE_DIR/bin_provider_active:$PATH" \
  AGENTDESK_SKIP_TURN_DRAIN=0 \
  wait_for_live_turns_to_drain_or_fail "release" "test.label" 0 0 1 \
  >/dev/null 2>&1
rc=$?
set -e
unset -f _launchd_job_state
assert_eq "strict drain refuses provider-active foreground evidence with queued work" "1" "$rc"

echo "== Test 5f: request helper clears marker if launchd job is stopped =="
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

echo "== Test 5g: restart persistence waits for runtime marker consumption =="
TMP_RUNTIME3=$(mktemp -d)
trap 'rm -rf "$TMP_FIXTURE_DIR" "$TMP_RUNTIME" "$TMPDIR_TEST" "$TMP_RUNTIME2" "$TMP_RUNTIME3"' EXIT
touch "$TMP_RUNTIME3/restart_pending"
( sleep 1; printf 'nonce=test-nonce\n' >"$TMP_RUNTIME3/restart_persisted"; rm -f "$TMP_RUNTIME3/restart_pending" ) &
BG_PID=$!
set +e
wait_for_restart_persistence_or_fail \
  "release" "$TMP_RUNTIME3" "test-nonce" 5 >/dev/null 2>&1
rc=$?
set -e
wait "$BG_PID" 2>/dev/null || true
assert_eq "persistence helper succeeds only after matching positive acknowledgement" "0" "$rc"

mkdir -p "$TMP_RUNTIME3/wrong-nonce"
touch "$TMP_RUNTIME3/wrong-nonce/restart_pending"
printf 'nonce=stale-nonce\n' >"$TMP_RUNTIME3/wrong-nonce/restart_persisted"
set +e
wait_for_restart_persistence_or_fail \
  "release" "$TMP_RUNTIME3/wrong-nonce" "current-nonce" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "persistence helper rejects acknowledgement from another request" "1" "$rc"
if [ -e "$TMP_RUNTIME3/wrong-nonce/restart_cancelled" ]; then
  pass "nonce mismatch publishes restart cancellation"
else
  fail "nonce mismatch publishes restart cancellation"
fi

mkdir -p "$TMP_RUNTIME3/consumed-without-ack"
touch "$TMP_RUNTIME3/consumed-without-ack/restart_pending"
# The rm() spy defined later (Test 6) is not yet in effect here; this is a
# real removal. shellcheck 0.9.x still reports SC2218 for the later stub even
# with a `command` prefix, so silence it explicitly.
# shellcheck disable=SC2218
command rm -f "$TMP_RUNTIME3/consumed-without-ack/restart_pending"
set +e
wait_for_restart_persistence_or_fail \
  "release" "$TMP_RUNTIME3/consumed-without-ack" "test-nonce" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "marker deletion without acknowledgement is not durability proof" "1" "$rc"

mkdir -p "$TMP_RUNTIME3/still-pending"
_restart_stage_and_link_marker \
  "$TMP_RUNTIME3/still-pending" test-nonce test release test.label
set +e
wait_for_restart_persistence_or_fail \
  "release" "$TMP_RUNTIME3/still-pending" "test-nonce" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "persistence helper refuses bootout while marker remains" "1" "$rc"
if [ ! -e "$TMP_RUNTIME3/still-pending/restart_pending" ]; then
  pass "persistence timeout clears restart marker"
else
  fail "persistence timeout clears restart marker"
fi

echo "== Test 6: clear_restart_drain_mode publishes cancellation before marker removal =="
printf 'nonce=handoff-order\n' >"$TMPDIR_TEST/restart_pending"
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
marker_remove_saw_cancel=0
mv() {
  if [ "$1" = "$TMPDIR_TEST/restart_pending" ] \
    && [ -f "$TMPDIR_TEST/restart_cancelled.handoff-order" ]; then
    marker_remove_saw_cancel=1
  fi
  command mv "$@"
}
clear_restart_drain_mode "$TMPDIR_TEST" handoff-order >/dev/null 2>&1 || true
unset -f mv
if [ ! -e "$TMPDIR_TEST/restart_pending" ]; then
  pass "marker removed"
else
  fail "marker removed"
fi
if [ "$marker_remove_saw_cancel" = "1" ] \
  && grep -q '^nonce=handoff-order$' "$TMPDIR_TEST/restart_cancelled"; then
  pass "cancellation nonce published before marker removal"
else
  fail "cancellation nonce published before marker removal"
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

echo "== Test 8: #5245 — shell writes and reads BOTH marker directories =="
# The deploy shell passes "$ADK_REL/runtime" while the running runtime watches
# "$ADK_REL" (crate::agentdesk_runtime_root returns $AGENTDESK_ROOT_DIR
# verbatim; no Rust code touches "$ROOT/runtime/restart_*"). Phase 1 mirrors
# request/cancellation to both and accepts an acknowledgement from either.
#
# These cases are the discrimination: 8b/8c prove the widening works, 8d/8e/8f
# prove it did NOT turn the gate into something that approves anything, and
# 8a/8h/8k prove the second path is really written and really comes from the
# explicit variable rather than from dirname.
TMP_D=$(mktemp -d)
trap 'rm -rf "$TMP_FIXTURE_DIR" "$TMP_RUNTIME" "$TMPDIR_TEST" "$TMP_RUNTIME2" "$TMP_RUNTIME3" "$TMP_D"' EXIT
DUAL_PRIMARY="$TMP_D/release/runtime"
DUAL_MIRROR="$TMP_D/release"
mkdir -p "$DUAL_PRIMARY"

dual_nonce_of() {
  # Never fails: a missing file yields the empty string so an assertion below
  # reports the mismatch instead of `set -e` aborting the suite mid-run.
  grep '^nonce=' "$1" 2>/dev/null | head -1 | cut -d= -f2- || true
}

# --- 8a (F): a single request lands in BOTH roots under ONE nonce ------------
_launchd_job_state() { echo "running"; }
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
  request_restart_drain_mode_or_fail "test" "test.label" 0 "$DUAL_PRIMARY" "smoke-test" \
  >/dev/null 2>&1
rc=$?
set -e
unset -f _launchd_job_state
assert_eq "dual-root request returns 0 (drain timeout path)" "0" "$rc"
if [ -f "$DUAL_PRIMARY/restart_pending" ] || [ -f "$DUAL_MIRROR/restart_pending" ]; then
  fail "timeout path clears the request marker in both roots"
else
  pass "timeout path clears the request marker in both roots"
fi
if [ -f "$DUAL_PRIMARY/restart_cancelled" ] && [ -f "$DUAL_MIRROR/restart_cancelled" ]; then
  pass "8e: cancellation reaches both roots, not just the one the shell owns"
else
  fail "8e: cancellation reaches both roots, not just the one the shell owns"
fi
cancel_primary_nonce=$(dual_nonce_of "$DUAL_PRIMARY/restart_cancelled" || true)
cancel_mirror_nonce=$(dual_nonce_of "$DUAL_MIRROR/restart_cancelled" || true)
if [ -n "$cancel_primary_nonce" ] && [ "$cancel_primary_nonce" = "$cancel_mirror_nonce" ]; then
  pass "both roots were cancelled under the same nonce ($cancel_primary_nonce)"
else
  fail "both roots were cancelled under the same nonce (primary=$cancel_primary_nonce mirror=$cancel_mirror_nonce)"
fi

# Same request again, this time observed while it is still armed: the marker
# must exist in both roots with an identical nonce. A single-root write (the
# pre-#5245 behaviour, or a regression that drops one of the two writes) fails
# here even though the deploy itself would still report success.
rm -f "$DUAL_PRIMARY/restart_cancelled" "$DUAL_MIRROR/restart_cancelled"
mkdir -p "$TMP_FIXTURE_DIR/bin_hold"
cat >"$TMP_FIXTURE_DIR/bin_hold/curl" <<'EOF'
#!/usr/bin/env bash
exit 7
EOF
chmod +x "$TMP_FIXTURE_DIR/bin_hold/curl"
_launchd_job_state() { echo "running"; }
armed_primary=0
armed_mirror=0
armed_same_nonce=0
( for _ in $(seq 1 60); do
    if [ -f "$DUAL_PRIMARY/restart_pending" ] && [ -f "$DUAL_MIRROR/restart_pending" ]; then
      break
    fi
    sleep 0.1
  done
  a=$(grep '^nonce=' "$DUAL_PRIMARY/restart_pending" 2>/dev/null | head -1 | cut -d= -f2- || true)
  b=$(grep '^nonce=' "$DUAL_MIRROR/restart_pending" 2>/dev/null | head -1 | cut -d= -f2- || true)
  printf 'primary=%s\nmirror=%s\n' \
    "$([ -f "$DUAL_PRIMARY/restart_pending" ] && echo 1 || echo 0)" \
    "$([ -f "$DUAL_MIRROR/restart_pending" ] && echo 1 || echo 0)" >"$TMP_D/armed"
  printf 'same=%s\n' "$([ -n "$a" ] && [ "$a" = "$b" ] && echo 1 || echo 0)" >>"$TMP_D/armed"
) &
OBS_PID=$!
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  PATH="$TMP_FIXTURE_DIR/bin_hold:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=4 \
  request_restart_drain_mode_or_fail "test" "test.label" 0 "$DUAL_PRIMARY" "smoke-test" \
  >/dev/null 2>&1
set -e
wait "$OBS_PID" 2>/dev/null || true
unset -f _launchd_job_state
armed_primary=$( (grep '^primary=' "$TMP_D/armed" 2>/dev/null || echo 'primary=absent') | cut -d= -f2)
armed_mirror=$( (grep '^mirror=' "$TMP_D/armed" 2>/dev/null || echo 'mirror=absent') | cut -d= -f2)
armed_same_nonce=$( (grep '^same=' "$TMP_D/armed" 2>/dev/null || echo 'same=absent') | cut -d= -f2)
assert_eq "8a: request armed restart_pending in the shell's own root" "1" "$armed_primary"
assert_eq "8a: request armed restart_pending in the runtime's root (mirror)" "1" "$armed_mirror"
assert_eq "8a: both roots carry the same nonce" "1" "$armed_same_nonce"


# --- 8b/8c (A/B): acknowledgement from EITHER root is accepted ---------------
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=dual-nonce\n' >"$DUAL_MIRROR/restart_persisted"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "dual-nonce" 2 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8b (A): ack written ONLY at the runtime root is accepted" "0" "$rc"

rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=dual-nonce\n' >"$DUAL_PRIMARY/restart_persisted"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "dual-nonce" 2 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8c (B): ack written ONLY at the shell root is accepted" "0" "$rc"

# --- 8d (C): no ack anywhere still fails ------------------------------------
# This is the safety property of the whole change: widening WHERE we look must
# not make "nothing acknowledged" pass.
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
touch "$DUAL_PRIMARY/restart_pending" "$DUAL_MIRROR/restart_pending"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "dual-nonce" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8d (C): ack absent from BOTH roots still refuses bootout" "1" "$rc"

# The refusal must also be observable in both roots, otherwise the runtime that
# does watch the other directory keeps a fenced admission path forever.
if [ -f "$DUAL_PRIMARY/restart_cancelled" ] && [ -f "$DUAL_MIRROR/restart_cancelled" ]; then
  pass "8d: refusal publishes cancellation to both roots"
else
  fail "8d: refusal publishes cancellation to both roots"
fi

# --- 8e (D): an ack with the wrong nonce is not an ack ----------------------
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=someone-elses-nonce\n' >"$DUAL_MIRROR/restart_persisted"
printf 'nonce=another-nonce\n' >"$DUAL_PRIMARY/restart_persisted"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "dual-nonce" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8e (D): ack present in both roots with a different nonce still fails" "1" "$rc"

rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=dual-nonce-suffix\n' >"$DUAL_MIRROR/restart_persisted"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "dual-nonce" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8e (D): a nonce that merely starts with ours is not a match" "1" "$rc"

# --- 8f: an empty expected nonce is refused, not compared -------------------
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=\n' >"$DUAL_MIRROR/restart_persisted"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8f: empty nonce is refused even when a bare 'nonce=' ack exists" "1" "$rc"

# --- 8g (E): cancellation is bound to the caller's request nonce -----------
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
_restart_stage_and_link_marker "$DUAL_PRIMARY" consumed-by-runtime test test test.label
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  clear_restart_drain_mode "$DUAL_PRIMARY" consumed-by-runtime >/dev/null 2>&1
rc=$?
set -e
assert_eq "8g (E): clear succeeds when only one root still holds the marker" "0" "$rc"
if grep -q '^nonce=consumed-by-runtime$' "$DUAL_MIRROR/restart_cancelled" 2>/dev/null \
  && grep -q '^nonce=consumed-by-runtime$' "$DUAL_PRIMARY/restart_cancelled" 2>/dev/null; then
  pass "8g: caller nonce publishes cancellation to every root"
else
  fail "8g: caller nonce publishes cancellation to every root"
fi

rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
_restart_stage_and_link_marker "$DUAL_MIRROR" only-in-mirror test test test.label
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  clear_restart_drain_mode "$DUAL_PRIMARY" only-in-mirror >/dev/null 2>&1
set -e
if grep -q '^nonce=only-in-mirror$' "$DUAL_PRIMARY/restart_cancelled" 2>/dev/null; then
  pass "8g: explicit nonce clears a request held only in the mirror"
else
  fail "8g: explicit nonce clears a request held only in the mirror"
fi

# --- 8h: the second root is the stated variable, never dirname --------------
# skills/agentdesk-restart passes "$HOME/.adk/release", whose dirname is
# "$HOME/.adk" — deriving the mirror would write markers into an unrelated
# directory. Point the mirror at a sibling and require the marker to follow the
# variable, with nothing created at dirname(primary).
DUAL_SIBLING="$TMP_D/sibling"
mkdir -p "$DUAL_SIBLING"
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
_launchd_job_state() { echo "not running"; }
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_SIBLING" \
  PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
  request_restart_drain_mode_or_fail "test" "sibling.label" 0 "$DUAL_PRIMARY" "smoke-test" \
  >/dev/null 2>&1
set -e
unset -f _launchd_job_state
# The job-not-running branch clears every root it armed; the observable proof
# is that the sibling was cleaned and dirname(primary) was never written.
if [ -e "$DUAL_MIRROR/restart_pending" ] || [ -e "$DUAL_MIRROR/restart_cancelled" ]; then
  fail "8h: mirror root is taken from the variable, not from dirname(primary)"
else
  pass "8h: mirror root is taken from the variable, not from dirname(primary)"
fi
if [ -e "$DUAL_SIBLING/restart_pending" ]; then
  fail "8h: sibling mirror marker cleared on the not-running branch"
else
  pass "8h: sibling mirror marker cleared on the not-running branch"
fi

# --- 8i: consumption in EITHER root is the idle-runtime acknowledgement -----
# Only one binary is running, so only one of the two markers is ever consumed.
# Requiring both to vanish would delete the #1447 ack path outright.
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
_launchd_job_state() { echo "running"; }
( for _ in $(seq 1 60); do
    [ -e "$DUAL_MIRROR/restart_pending" ] && break
    sleep 0.1
  done
  command rm -f "$DUAL_MIRROR/restart_pending" ) &
BG_PID=$!
set +e
consumed_out=$(AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=10 \
  request_restart_drain_mode_or_fail "test" "test.label" 0 "$DUAL_PRIMARY" "smoke-test" \
  2>&1)
rc=$?
set -e
wait "$BG_PID" 2>/dev/null || true
unset -f _launchd_job_state
assert_eq "8i: marker consumed in the runtime root alone counts as ack" "0" "$rc"
case "$consumed_out" in
  *"consumed by runtime at $DUAL_MIRROR"*)
    pass "8i: ack names the root whose marker was consumed" ;;
  *)
    fail "8i: ack names the root whose marker was consumed (got: $consumed_out)" ;;
esac

# Negative twin: while BOTH markers survive, the consumed-branch must not fire.
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
_launchd_job_state() { echo "running"; }
set +e
survived_out=$(AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=2 \
  request_restart_drain_mode_or_fail "test" "test.label" 0 "$DUAL_PRIMARY" "smoke-test" \
  2>&1)
set -e
unset -f _launchd_job_state
case "$survived_out" in
  *"consumed by runtime"*)
    fail "8i: both markers intact must NOT be read as consumption" ;;
  *)
    pass "8i: both markers intact must NOT be read as consumption" ;;
esac

# --- 8k: a half-owned lease is never left behind ---------------------------
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=foreign-owner\n' >"$DUAL_MIRROR/restart_pending"
_launchd_job_state() { echo "running"; }
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
  request_restart_drain_mode_or_fail "test" "test.label" 0 "$DUAL_PRIMARY" "smoke-test" \
  >/dev/null 2>&1
rc=$?
set -e
unset -f _launchd_job_state
assert_eq "8k: request fails when the mirror root is owned by another nonce" "1" "$rc"
if [ -e "$DUAL_PRIMARY/restart_pending" ]; then
  fail "8k: the root acquired first is rolled back on partial acquisition"
else
  pass "8k: the root acquired first is rolled back on partial acquisition"
fi
if grep -q '^nonce=foreign-owner$' "$DUAL_MIRROR/restart_pending" 2>/dev/null; then
  pass "8k: the foreign owner's marker is left untouched"
else
  fail "8k: the foreign owner's marker is left untouched"
fi

# --- 8n: a successful acknowledgement releases the lease nobody consumes ----
# Before #5245 the acknowledgement never arrived, so every exit from the gate
# ran through clear_restart_drain_mode and no marker survived. Now that the gate
# can succeed, the root that no runtime watches keeps its marker unless it is
# released — and the next deploy's O_EXCL acquisition would fail with
# "restart drain marker already owned".
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=ack-release\n' >"$DUAL_PRIMARY/restart_pending"
printf 'nonce=ack-release\n' >"$DUAL_MIRROR/restart_pending"
printf 'nonce=ack-release\n' >"$DUAL_MIRROR/restart_persisted"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "ack-release" 2 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8n: ack at the runtime root is accepted" "0" "$rc"
if [ -e "$DUAL_PRIMARY/restart_pending" ]; then
  fail "8n: the unwatched root's lease is released so the next deploy can acquire"
else
  pass "8n: the unwatched root's lease is released so the next deploy can acquire"
fi
if [ -e "$DUAL_MIRROR/restart_pending" ]; then
  pass "8n: the acknowledging runtime's own marker is left for it to remove"
else
  fail "8n: the acknowledging runtime's own marker is left for it to remove"
fi

# The release is nonce-scoped: a marker owned by somebody else is never freed.
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=someone-else\n' >"$DUAL_PRIMARY/restart_pending"
printf 'nonce=ack-release\n' >"$DUAL_MIRROR/restart_persisted"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "ack-release" 2 >/dev/null 2>&1
set -e
if grep -q '^nonce=someone-else$' "$DUAL_PRIMARY/restart_pending" 2>/dev/null; then
  pass "8n: a lease held under another nonce is not released"
else
  fail "8n: a lease held under another nonce is not released"
fi

# --- 8l: with no mirror configured, behaviour is byte-for-byte the old one --
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=single-root\n' >"$DUAL_MIRROR/restart_persisted"
set +e
wait_for_restart_persistence_or_fail "test" "$DUAL_PRIMARY" "single-root" 1 >/dev/null 2>&1
rc=$?
set -e
assert_eq "8l: without the mirror variable the parent directory is NOT consulted" "1" "$rc"

echo "== Test 9: #5254 S1 — drain verdict and durability-skip observability =="
S1_TMP=$(mktemp -d)
trap 'rm -rf "$TMP_FIXTURE_DIR" "$TMP_RUNTIME" "$TMPDIR_TEST" "$TMP_RUNTIME2" "$TMP_RUNTIME3" "$TMP_D" "$S1_TMP"' EXIT

# The health boolean proves only that the admission fence is armed. It carries
# no request nonce, so this path must not claim request acknowledgement.
mkdir -p "$S1_TMP/health-root" "$S1_TMP/bin_health"
cat >"$S1_TMP/bin_health/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s' '{"providers":[{"name":"a","restart_pending":true}]}'
EOF
chmod +x "$S1_TMP/bin_health/curl"
guard_no_foreign_active_turns_or_warn() { return 0; }
set +e
PATH="$S1_TMP/bin_health:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
  request_restart_drain_mode_or_fail \
    "test" "test.label" 0 "$S1_TMP/health-root" "smoke-test" \
    >"$S1_TMP/health.out" 2>&1
rc=$?
set -e
assert_eq "fence-observed path returns 0" "0" "$rc"
assert_eq "attribution is claimed only when a nonce artifact exists" \
  "fence-observed:nonce-unattributed" "${AGENTDESK_RESTART_DRAIN_VERDICT:-missing}"
if grep -Fq 'acknowledged by runtime' "$S1_TMP/health.out"; then
  fail "the fence-observed exit does not claim acknowledgement"
else
  pass "the fence-observed exit does not claim acknowledgement"
fi
if grep -Fq 'restart admission fence observed' "$S1_TMP/health.out"; then
  pass "the fence-observed exit names only the observed fence"
else
  fail "the fence-observed exit names only the observed fence"
fi
if export -p | grep -Fq 'AGENTDESK_RESTART_DRAIN_VERDICT'; then
  pass "the consumer observes the verdict"
else
  fail "the consumer observes the verdict"
fi

# A matching durable nonce artifact is the only S1 path allowed to use the
# acknowledged:nonce vocabulary. S2 will later make this artifact dispositive;
# S1 records it without changing the existing return decision.
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
guard_no_foreign_active_turns_or_warn() { return 0; }
mkdir -p "$S1_TMP/nonce-root"
( for _ in $(seq 1 50); do
    [ -f "$S1_TMP/nonce-root/restart_pending" ] && break
    sleep 0.1
  done
  nonce=$(grep '^nonce=' "$S1_TMP/nonce-root/restart_pending" 2>/dev/null | head -1 | cut -d= -f2- || true)
  [ -n "$nonce" ] || exit 1
  printf 'nonce=%s\n' "$nonce" >"$S1_TMP/nonce-root/restart_persisted"
  rm -f "$S1_TMP/nonce-root/restart_pending" ) &
S1_BG=$!
set +e
PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=10 \
  request_restart_drain_mode_or_fail \
    "test" "test.label" 0 "$S1_TMP/nonce-root" "smoke-test" \
    >"$S1_TMP/nonce.out" 2>&1
rc=$?
set -e
wait "$S1_BG" 2>/dev/null || true
assert_eq "matching nonce artifact path returns 0" "0" "$rc"
assert_eq "matching nonce artifact is named as acknowledged" \
  "acknowledged:nonce" "${AGENTDESK_RESTART_DRAIN_VERDICT:-missing}"

# Marker consumption without our durable nonce is a distinct observation. S1
# still preserves the existing success return; S2 will decide this state.
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
guard_no_foreign_active_turns_or_warn() { return 0; }
mkdir -p "$S1_TMP/consumed-root"
( for _ in $(seq 1 50); do
    [ -f "$S1_TMP/consumed-root/restart_pending" ] && break
    sleep 0.1
  done
  rm -f "$S1_TMP/consumed-root/restart_pending" ) &
S1_BG=$!
set +e
PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=10 \
  request_restart_drain_mode_or_fail \
    "test" "test.label" 0 "$S1_TMP/consumed-root" "smoke-test" \
    >"$S1_TMP/consumed.out" 2>&1
rc=$?
set -e
wait "$S1_BG" 2>/dev/null || true
assert_eq "consumed path without our nonce keeps its existing success return" "0" "$rc"
assert_eq "consumed path without our nonce has its own verdict" \
  "consumed:our-nonce-unobserved" "${AGENTDESK_RESTART_DRAIN_VERDICT:-missing}"

# The two NOT_REQUIRED outcomes remain behaviorally identical, but now carry
# distinguishable reasons for the deploy consumer.
# shellcheck source=/dev/null
. "$DEFAULTS_SH"
guard_no_foreign_active_turns_or_warn() { return 0; }
_launchd_job_state() { echo "not running"; }
mkdir -p "$S1_TMP/stopped-root"
set +e
PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
  request_restart_drain_mode_or_fail \
    "test" "stopped.label" 0 "$S1_TMP/stopped-root" "smoke-test" \
    >"$S1_TMP/stopped.out" 2>&1
rc=$?
set -e
assert_eq "stopped runtime path returns 0" "0" "$rc"
stopped_verdict="${AGENTDESK_RESTART_DRAIN_VERDICT:-missing}"
assert_eq "stopped runtime names the unevaluated reason" \
  "not evaluated: launchd job is not running" "$stopped_verdict"
unset -f _launchd_job_state

# shellcheck source=/dev/null
. "$DEFAULTS_SH"
guard_no_foreign_active_turns_or_warn() { return 0; }
_launchd_job_state() { echo "running"; }
mkdir -p "$S1_TMP/timeout-root"
set +e
PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
  request_restart_drain_mode_or_fail \
    "test" "running.label" 0 "$S1_TMP/timeout-root" "smoke-test" \
    >"$S1_TMP/timeout.out" 2>&1
rc=$?
set -e
assert_eq "timeout path retains its existing success return" "0" "$rc"
timeout_verdict="${AGENTDESK_RESTART_DRAIN_VERDICT:-missing}"
assert_eq "timeout names why durability was not evaluated" \
  "not evaluated: restart drain acknowledgement timed out" "$timeout_verdict"
if [ "$stopped_verdict" != "$timeout_verdict" ]; then
  pass "the two skip reasons are distinguishable"
else
  fail "the two skip reasons are distinguishable"
fi
unset -f _launchd_job_state

# Execute only the production durability region in a child shell, matching the
# existing #5244 region/eval convention without running a real deploy.
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
durability_begin=$(grep -nF '# >>> BEGIN restart-durability gate (#5254)' "$DEPLOY_SH" | cut -d: -f1 || true)
durability_end=$(grep -nF '# <<< END restart-durability gate (#5254)' "$DEPLOY_SH" | cut -d: -f1 || true)
terminal_line=$(grep -nF 'echo "═══ Deploy Complete ═══"' "$DEPLOY_SH" | tail -1 | cut -d: -f1 || true)
[ -n "$durability_begin" ] || fail "restart-durability BEGIN sentinel exists"
[ -n "$durability_end" ] || fail "restart-durability END sentinel exists"
if [ -n "$durability_begin" ] && [ -n "$durability_end" ]; then
  sed -n "$((durability_begin + 1)),$((durability_end - 1))p" "$DEPLOY_SH" >"$S1_TMP/durability-region.sh"
fi
if [ -n "$durability_end" ] && [ -n "$terminal_line" ] && [ "$durability_end" -lt "$terminal_line" ]; then
  pass "the skip observation precedes the terminal marker"
else
  fail "the skip observation precedes the terminal marker"
fi

skip_out=$(REGION="$S1_TMP/durability-region.sh" bash -c '
  set -euo pipefail
  ADK_REL="/unused"
  RESTART_REQUEST_NONCE=""
  AGENTDESK_RESTART_PERSISTENCE_NOT_REQUIRED=1
  AGENTDESK_RESTART_DRAIN_VERDICT="not evaluated: launchd job is not running"
  clear_restart_drain_mode() { :; }
  wait_for_restart_persistence_or_fail() { echo phase-2-ran; }
  eval "$(<"$REGION")"
')
case "$skip_out" in
  *'restart durability gate=not evaluated: launchd job is not running'*)
    pass "phase-2 skip is named on stdout" ;;
  *)
    fail "phase-2 skip is named on stdout (got: $skip_out)" ;;
esac
case "$skip_out" in
  *'launchd job is not running'*)
    pass "the skip names its reason" ;;
  *)
    fail "the skip names its reason (got: $skip_out)" ;;
esac
if [ "$(grep -o 'not evaluated:' <<<"$skip_out" | wc -l | tr -d ' ')" = "1" ]; then
  pass "the skip renders the not-evaluated prefix once"
else
  fail "the skip renders the not-evaluated prefix once (got: $skip_out)"
fi
case "$skip_out" in
  *'phase-2-ran'*)
    fail "the skip line appears iff phase 2 did not run" ;;
  *)
    pass "the skip line appears iff phase 2 did not run" ;;
esac

run_out=$(REGION="$S1_TMP/durability-region.sh" bash -c '
  set -euo pipefail
  ADK_REL="/unused"
  RESTART_REQUEST_NONCE="test-nonce"
  AGENTDESK_RESTART_PERSISTENCE_NOT_REQUIRED=0
  AGENTDESK_RESTART_DRAIN_VERDICT="fence-observed:nonce-unattributed"
  clear_restart_drain_mode() { :; }
  wait_for_restart_persistence_or_fail() { echo phase-2-ran; }
  eval "$(<"$REGION")"
')
case "$run_out" in
  *'phase-2-ran'*)
    pass "phase 2 still runs when persistence is required" ;;
  *)
    fail "phase 2 still runs when persistence is required (got: $run_out)" ;;
esac
case "$run_out" in
  *'restart durability gate='*)
    fail "the skip line appears iff phase 2 did not run" ;;
  *)
    pass "the run path does not print a skip line" ;;
esac

echo "== Test 10: #5254 S3a — shell restart lifecycle primitives =="
S3A_TMP=$(mktemp -d)
trap 'rm -rf "$TMP_FIXTURE_DIR" "$TMP_RUNTIME" "$TMPDIR_TEST" "$TMP_RUNTIME2" "$TMP_RUNTIME3" "$TMP_D" "$S1_TMP" "$S3A_TMP"' EXIT

for safe_nonce in a A0 a.b_c-d "$(printf '%0128d' 0)"; do
  if _restart_nonce_is_path_safe "$safe_nonce"; then
    pass "charset gate accepts path-safe nonce length=${#safe_nonce}"
  else
    fail "charset gate accepts path-safe nonce length=${#safe_nonce}"
  fi
done
for unsafe_nonce in '' . .. 'a/b' 'a b' 'x?y' "$(printf 'x\n../escape')" "$(printf '%0129d' 0)"; do
  if _restart_nonce_is_path_safe "$unsafe_nonce"; then
    fail "charset gate rejects unsafe nonce length=${#unsafe_nonce}"
  else
    pass "charset gate rejects unsafe nonce length=${#unsafe_nonce}"
  fi
done

mkdir -p "$S3A_TMP/stage"
set +e
_restart_stage_and_link_marker "$S3A_TMP/stage" 'bad/nonce' src scope label
unsafe_rc=$?
set -e
assert_eq "five-way stage rc=3 for unsafe nonce" "3" "$unsafe_rc"
if _restart_stage_and_link_marker "$S3A_TMP/stage" nonce-A src scope label; then
  pass "two-stage marker acquisition succeeds"
else
  fail "two-stage marker acquisition succeeds"
fi
if [ -f "$S3A_TMP/stage/restart_pending.nonce-A" ] \
  && [ -f "$S3A_TMP/stage/restart_pending" ] \
  && _restart_artifact_nonce_matches "$S3A_TMP/stage/restart_pending" nonce-A; then
  pass "identity and canonical marker names expose the complete body"
else
  fail "identity and canonical marker names expose the complete body"
fi
set +e
_restart_stage_and_link_marker "$S3A_TMP/stage" nonce-A src scope label
reused_rc=$?
_restart_stage_and_link_marker "$S3A_TMP/stage" nonce-B src scope label
held_rc=$?
set -e
assert_eq "five-way stage rc=4 for reused nonce" "4" "$reused_rc"
assert_eq "five-way stage rc=1 for held lease" "1" "$held_rc"

mkdir -p "$S3A_TMP/terminal"
_restart_terminal_publish "$S3A_TMP/terminal" restart_persisted terminal-A 'committed_at=now'
_restart_terminal_publish "$S3A_TMP/terminal" restart_persisted terminal-B 'committed_at=later'
if _restart_artifact_nonce_matches "$S3A_TMP/terminal/restart_persisted.terminal-A" terminal-A \
  && _restart_artifact_nonce_matches "$S3A_TMP/terminal/restart_persisted.terminal-B" terminal-B \
  && _restart_artifact_nonce_matches "$S3A_TMP/terminal/restart_persisted" terminal-B; then
  pass "terminal publish preserves identities and advances the fixed-name index"
else
  fail "terminal publish preserves identities and advances the fixed-name index"
fi

# Three actors: stale A claims B's canonical marker, then C installs its marker
# before A restores. Inject that interleaving inside the production helper.
mkdir -p "$S3A_TMP/three"
_restart_stage_and_link_marker "$S3A_TMP/three" actor-N-prime src scope label
real_nonce_match=$(declare -f _restart_artifact_nonce_matches)
_restart_artifact_nonce_matches() {
  if [ "$2" = actor-N ]; then
    _restart_stage_and_link_marker "$S3A_TMP/three" actor-N-double-prime src scope label
    return 1
  fi
  [ -f "$1" ] && grep -Fqx -- "nonce=$2" "$1" 2>/dev/null
}
three_dispose_out=$(_restart_dispose_marker_by_own_nonce "$S3A_TMP/three" actor-N 2>&1 || true)
eval "$real_nonce_match"
set -- "$S3A_TMP/three"/.restart_pending.dispose.actor-N.*
if [ -f "$1" ] \
  && _restart_artifact_nonce_matches "$1" actor-N-prime \
  && _restart_artifact_nonce_matches "$S3A_TMP/three/restart_pending.actor-N-prime" actor-N-prime \
  && _restart_artifact_nonce_matches "$S3A_TMP/three/restart_pending" actor-N-double-prime; then
  pass "three-actor EEXIST lower bound preserves residue and middle identity"
else
  fail "three-actor EEXIST lower bound preserves residue and middle identity"
fi
case "$three_dispose_out" in
  *"restart-dispose-restore-eexist"*"expected=actor-N"*"found=actor-N-prime"*)
    pass "EEXIST residue log names expected and found nonces" ;;
  *)
    fail "EEXIST residue log names expected and found nonces (got: $three_dispose_out)" ;;
esac

# A canonical link syscall failure without a competing canonical marker is a
# create failure (rc=2), never a held lease (rc=1).
mkdir -p "$S3A_TMP/link-fail"
real_ln=$(declare -f ln 2>/dev/null || true)
ln() {
  if [ "$2" = "$S3A_TMP/link-fail/restart_pending" ]; then
    return 1
  fi
  command ln "$@"
}
set +e
_restart_stage_and_link_marker "$S3A_TMP/link-fail" link-fail src scope label
link_fail_rc=$?
set -e
unset -f ln
[ -n "$real_ln" ] && eval "$real_ln"
assert_eq "canonical link failure without owner returns rc=2" "2" "$link_fail_rc"

# Entropy generation must reach /dev/urandom when an installed uuidgen fails.
mkdir -p "$S3A_TMP/entropy-bin"
printf '#!/usr/bin/env bash\nexit 1\n' >"$S3A_TMP/entropy-bin/uuidgen"
chmod +x "$S3A_TMP/entropy-bin/uuidgen"
set +e
fallback_entropy=$(PATH="$S3A_TMP/entropy-bin:$PATH" _restart_nonce_entropy)
entropy_rc=$?
set -e
assert_eq "uuidgen failure falls back to /dev/urandom" "0" "$entropy_rc"
if [ "${#fallback_entropy}" = 8 ] && _restart_nonce_is_path_safe "$fallback_entropy"; then
  pass "urandom fallback yields eight path-safe hex characters"
else
  fail "urandom fallback yields eight path-safe hex characters (got: $fallback_entropy)"
fi

# E8.9: force nonce reuse and publish a same-nonce terminal identity immediately
# after identity reservation. Cleanup must occur after identity reservation and
# before canonical publication; moving it to either side makes one fixture red.
mkdir -p "$S3A_TMP/reuse"
forced_nonce="forced-$$-0-entropy"
printf 'nonce=%s\n' "$forced_nonce" >"$S3A_TMP/reuse/restart_persisted.$forced_nonce"
printf 'nonce=%s\n' "$forced_nonce" >"$S3A_TMP/reuse/restart_cancelled.$forced_nonce"
printf 'nonce=foreign\n' >"$S3A_TMP/reuse/restart_persisted.foreign"
printf 'nonce=fixed\n' >"$S3A_TMP/reuse/restart_persisted"
real_entropy=$(declare -f _restart_nonce_entropy)
real_date=$(declare -f date 2>/dev/null || true)
real_stage_identity=$(declare -f _restart_stage_marker_identity)
real_link_canonical=$(declare -f _restart_link_canonical_marker)
eval "$(printf '%s\n' "$real_stage_identity" | sed '1s/_restart_stage_marker_identity/_restart_stage_marker_identity_real/')"
eval "$(printf '%s\n' "$real_link_canonical" | sed '1s/_restart_link_canonical_marker/_restart_link_canonical_marker_real/')"
_restart_nonce_entropy() { printf entropy; }
date() {
  if [ "$1" = -u ] && [ "$2" = +%Y%m%dT%H%M%S ]; then
    printf forced
  else
    command date "$@"
  fi
}
_restart_stage_marker_identity() {
  _restart_stage_marker_identity_real "$@" || return $?
  _restart_terminal_publish "$1" restart_persisted "$2" 'committed_at=concurrent'
}
unset RANDOM
RANDOM=0
_launchd_job_state() { echo "not running"; }
AGENTDESK_RESTART_DRAIN_ACK_WAIT=0 \
  request_restart_drain_mode_or_fail test test.label 0 "$S3A_TMP/reuse" src >/dev/null 2>&1
if [ ! -e "$S3A_TMP/reuse/restart_persisted.$forced_nonce" ] \
  && [ ! -e "$S3A_TMP/reuse/restart_cancelled.$forced_nonce" ] \
  && [ -e "$S3A_TMP/reuse/restart_persisted.foreign" ]; then
  pass "same-nonce terminal cleanup occurs after identity reservation and before canonical publication"
else
  fail "same-nonce terminal cleanup occurs after identity reservation and before canonical publication"
fi

# Crash exactly after canonical publication. The canonical and identity may
# remain, but the stale same-nonce terminal must already be absent.
mkdir -p "$S3A_TMP/post-lease-crash"
printf 'nonce=%s\n' "$forced_nonce" >"$S3A_TMP/post-lease-crash/restart_persisted.$forced_nonce"
unset RANDOM
RANDOM=0
set +e
(
  _restart_link_canonical_marker() {
    _restart_link_canonical_marker_real "$@" || return $?
    exit 97
  }
  request_restart_drain_mode_or_fail test test.label 0 "$S3A_TMP/post-lease-crash" src >/dev/null 2>&1
)
post_lease_crash_rc=$?
set -e
assert_eq "post-canonical crash seam exits at injected point" "97" "$post_lease_crash_rc"
if [ -e "$S3A_TMP/post-lease-crash/restart_pending" ] \
  && [ -e "$S3A_TMP/post-lease-crash/restart_pending.$forced_nonce" ] \
  && [ ! -e "$S3A_TMP/post-lease-crash/restart_persisted.$forced_nonce" ]; then
  pass "post-canonical crash cannot retain a same-nonce stale terminal"
else
  fail "post-canonical crash cannot retain a same-nonce stale terminal"
fi

# Identity-only rollback is the failure shape before canonical publication.
mkdir -p "$S3A_TMP/identity-only-rollback"
_restart_stage_marker_identity_real "$S3A_TMP/identity-only-rollback" identity-only src scope label
_restart_dispose_marker_by_own_nonce "$S3A_TMP/identity-only-rollback" identity-only
if [ ! -e "$S3A_TMP/identity-only-rollback/restart_pending" ] \
  && [ ! -e "$S3A_TMP/identity-only-rollback/restart_pending.identity-only" ]; then
  pass "identity-only rollback removes its reservation without a canonical marker"
else
  fail "identity-only rollback removes its reservation without a canonical marker"
fi

# Actor B must not dispose actor A's same-nonce identity or canonical lease when
# B loses the identity reservation with marker rc=4.
mkdir -p "$S3A_TMP/same-nonce-race"
printf 'nonce=%s\n' "$forced_nonce" >"$S3A_TMP/same-nonce-race/restart_pending.$forced_nonce"
ln "$S3A_TMP/same-nonce-race/restart_pending.$forced_nonce" \
  "$S3A_TMP/same-nonce-race/restart_pending"
unset RANDOM
RANDOM=0
set +e
AGENTDESK_RESTART_DRAIN_ACK_WAIT=0 \
  request_restart_drain_mode_or_fail test test.label 0 "$S3A_TMP/same-nonce-race" src \
  >"$S3A_TMP/same-nonce-race.out" 2>&1
same_nonce_race_rc=$?
set -e
assert_eq "same-nonce actor B fails after marker rc=4" "1" "$same_nonce_race_rc"
if _restart_artifact_nonce_matches "$S3A_TMP/same-nonce-race/restart_pending" "$forced_nonce" \
  && _restart_artifact_nonce_matches "$S3A_TMP/same-nonce-race/restart_pending.$forced_nonce" "$forced_nonce"; then
  pass "marker rc=4 preserves actor A's identity and canonical lease"
else
  fail "marker rc=4 preserves actor A's identity and canonical lease"
fi

# Actor B must not move a foreign canonical lease out of its fixed name when
# B loses canonical publication with marker rc=1. Observe the helper seam that
# the old rollback entered; any transient absence would let the owner falsely
# classify the request as consumed.
mkdir -p "$S3A_TMP/foreign-canonical-race"
_restart_stage_and_link_marker \
  "$S3A_TMP/foreign-canonical-race" actor-A-foreign src scope label
real_nonce_match=$(declare -f _restart_artifact_nonce_matches)
foreign_canonical_absent=0
_restart_artifact_nonce_matches() {
  if [ ! -e "$S3A_TMP/foreign-canonical-race/restart_pending" ]; then
    foreign_canonical_absent=1
  fi
  [ -f "$1" ] && grep -Fqx -- "nonce=$2" "$1" 2>/dev/null
}
_restart_nonce_entropy() { printf actor-B-entropy; }
date() {
  if [ "$1" = -u ] && [ "$2" = +%Y%m%dT%H%M%S ]; then
    printf actor-B
  else
    command date "$@"
  fi
}
unset RANDOM
RANDOM=0
set +e
AGENTDESK_RESTART_DRAIN_ACK_WAIT=0 \
  request_restart_drain_mode_or_fail test test.label 0 "$S3A_TMP/foreign-canonical-race" src \
  >"$S3A_TMP/foreign-canonical-race.out" 2>&1
foreign_canonical_race_rc=$?
set -e
assert_eq "foreign canonical actor B fails after marker rc=1" "1" "$foreign_canonical_race_rc"
assert_eq "marker rc=1 never removes actor A's canonical fixed name" \
  "0" "$foreign_canonical_absent"
if _restart_artifact_nonce_matches "$S3A_TMP/foreign-canonical-race/restart_pending" actor-A-foreign; then
  pass "marker rc=1 preserves actor A's canonical lease"
else
  fail "marker rc=1 preserves actor A's canonical lease"
fi

eval "$real_nonce_match"
unset -f _launchd_job_state date _restart_stage_marker_identity_real _restart_link_canonical_marker_real
[ -n "$real_date" ] && eval "$real_date"
eval "$real_entropy"
eval "$real_stage_identity"
eval "$real_link_canonical"

# The not-running check can race with a newer actor acquiring the canonical
# lease. Inject actor B at that seam and require actor A's cleanup to preserve B.
mkdir -p "$S3A_TMP/not-running-race"
real_entropy=$(declare -f _restart_nonce_entropy)
real_date=$(declare -f date 2>/dev/null || true)
_restart_nonce_entropy() { printf actor-A-entropy; }
date() {
  if [ "$1" = -u ] && [ "$2" = +%Y%m%dT%H%M%S ]; then
    printf actor-A
  else
    command date "$@"
  fi
}
unset RANDOM
RANDOM=0
_launchd_job_state() {
  rm -f "$S3A_TMP/not-running-race/restart_pending"
  _restart_stage_and_link_marker "$S3A_TMP/not-running-race" actor-B src scope label
  echo "not running"
}
request_restart_drain_mode_or_fail test test.label 0 "$S3A_TMP/not-running-race" src >/dev/null 2>&1
unset -f _launchd_job_state date
[ -n "$real_date" ] && eval "$real_date"
eval "$real_entropy"
if _restart_artifact_nonce_matches "$S3A_TMP/not-running-race/restart_pending" actor-B \
  && _restart_artifact_nonce_matches "$S3A_TMP/not-running-race/restart_pending.actor-B" actor-B; then
  pass "not-running cleanup preserves a newer actor's canonical marker"
else
  fail "not-running cleanup preserves a newer actor's canonical marker"
fi

# Reproduce the restart skill's no-argument cleanup form under set -e. The
# not-running success path must arm the nonce so bootstrap remains reachable.
mkdir -p "$S3A_TMP/no-arg"
set +e
(
  set -e
  _restart_nonce_entropy() { printf no-arg-entropy; }
  date() {
    if [ "$1" = -u ] && [ "$2" = +%Y%m%dT%H%M%S ]; then
      printf no-arg
    else
      command date "$@"
    fi
  }
  unset RANDOM
  RANDOM=0
  _launchd_job_state() { echo "not running"; }
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
    PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
    request_restart_drain_mode_or_fail test test.label 0 "$S3A_TMP/no-arg" src >/dev/null 2>&1
  clear_restart_drain_mode "$S3A_TMP/no-arg" >/dev/null 2>&1
  printf reached-bootstrap >"$S3A_TMP/no-arg/result"
)
no_arg_rc=$?
set -e
assert_eq "restart skill no-argument cleanup stays on success path" "0" "$no_arg_rc"
no_arg_result="$(command cat "$S3A_TMP/no-arg/result" 2>/dev/null || true)"
if [ "$no_arg_result" = reached-bootstrap ]; then
  pass "restart skill reaches bootstrap after no-argument cleanup"
else
  fail "restart skill reaches bootstrap after no-argument cleanup"
fi

mkdir -p "$S3A_TMP/cancel"
_restart_stage_and_link_marker "$S3A_TMP/cancel" cancel-order src scope label
clear_restart_drain_mode "$S3A_TMP/cancel" cancel-order
if _restart_artifact_nonce_matches "$S3A_TMP/cancel/restart_cancelled.cancel-order" cancel-order \
  && [ ! -e "$S3A_TMP/cancel/restart_pending" ] \
  && [ ! -e "$S3A_TMP/cancel/restart_pending.cancel-order" ]; then
  pass "cancellation leaves terminal identity and removes both marker names"
else
  fail "cancellation leaves terminal identity and removes both marker names"
fi

fi
if [ "${RESTART_S3B_ONLY:-0}" = "1" ]; then
  S3A_TMP=$(mktemp -d)
  trap 'rm -rf "$S3A_TMP"' EXIT
fi

# #5254 S3b reconstruction assets. These fixtures freeze mtime and inject at
# named helper seams; they do not claim to close the documented compare/unlink
# windows. A32-A34 instead keep those windows and their visible failure shape
# observable.
echo "== Test 11: #5254 S3b — crash-safe restart artifact sweep =="

# date dialect probe: GNU date reads an epoch as -d @N, BSD date as -r N
# (GNU -r means "file mtime" and fails on a bare epoch, feeding touch -t an
# empty stamp). Local time on both sides so touch -t interprets consistently.
if date -d @0 '+%Y' >/dev/null 2>&1; then
  _epoch_to_touch_stamp() { date -d "@$1" '+%Y%m%d%H%M.%S'; }
else
  _epoch_to_touch_stamp() { date -r "$1" '+%Y%m%d%H%M.%S'; }
fi

set_file_age() {
  local file="$1" age="$2"
  touch -t "$(_epoch_to_touch_stamp "$(( $(date +%s) - age ))")" "$file"
}

new_sweep_root() {
  local name="$1"
  local root="$S3A_TMP/s3b-$name"
  mkdir -p "$root"
  printf '%s' "$root"
}

new_aged_artifact() {
  local root
  root=$(new_sweep_root "$1")
  printf '%s\n' "$3" >"$root/$2"
  set_file_age "$root/$2" "$4"
  printf '%s' "$root"
}

shadow_function_as() {
  local source="$1" alias="$2" definition
  definition="$(declare -f "$source")" || return 1
  eval "$(printf '%s\n' "$definition" | sed "1s/$source/$alias/")"
}

# A0, A3-A5, A7-A10, A12-A16, A22, A24, A29, A30, A35.
root=$(new_sweep_root matrix)
printf 'legacy\n' >"$root/legacy-old"
printf 'legacy\n' >"$root/restart_pending"
set_file_age "$root/restart_pending" 61
_restart_reclaim_legacy_marker_if_stale "$root" >/dev/null 2>&1
[ ! -e "$root/restart_pending" ] && pass "A0 legacy marker fixture is deterministic" \
  || fail "A0 legacy marker fixture is deterministic"
printf 'nonce=orphan\n' >"$root/restart_pending.orphan"
printf 'nonce=young\n' >"$root/restart_pending.young"
printf 'nonce=old-p\n' >"$root/restart_persisted.old-p"
printf 'nonce=old-c\n' >"$root/restart_cancelled.old-c"
printf 'nonce=young-p\n' >"$root/restart_persisted.young-p"
printf 'nonce=active\n' >"$root/restart_persisted.active"
printf 'nonce=active-c\n' >"$root/restart_cancelled.active-c"
printf 'nonce=tmp\n' >"$root/restart_pending.123.tmp"
printf 'nonce=tmp\n' >"$root/restart_persisted.123.tmp"
printf 'nonce=tmp\n' >"$root/restart_cancelled.123.tmp"
printf 'dot\n' >"$root/.restart_pending.stage.dot"
printf 'dot\n' >"$root/.restart_pending.dispose.dot"
set_file_age "$root/restart_pending.orphan" 601
set_file_age "$root/restart_pending.young" 0
set_file_age "$root/restart_persisted.old-p" 3601
set_file_age "$root/restart_cancelled.old-c" 3601
set_file_age "$root/restart_persisted.young-p" 0
set_file_age "$root/restart_persisted.active" 3601
set_file_age "$root/restart_cancelled.active-c" 3601
set_file_age "$root/restart_pending.123.tmp" 3601
set_file_age "$root/restart_persisted.123.tmp" 3601
set_file_age "$root/restart_cancelled.123.tmp" 3601
set_file_age "$root/.restart_pending.stage.dot" 3601
set_file_age "$root/.restart_pending.dispose.dot" 3601
_restart_sweep_artifacts "$root" >/dev/null 2>&1
if [ ! -e "$root/restart_pending.orphan" ] \
  && [ -e "$root/restart_pending.young" ] \
  && [ ! -e "$root/restart_persisted.old-p" ] \
  && [ ! -e "$root/restart_cancelled.old-c" ] \
  && [ -e "$root/restart_persisted.young-p" ] \
  && [ -e "$root/restart_pending.123.tmp" ] \
  && [ -e "$root/restart_persisted.123.tmp" ] \
  && [ -e "$root/restart_cancelled.123.tmp" ] \
  && [ -e "$root/.restart_pending.stage.dot" ] \
  && [ -e "$root/.restart_pending.dispose.dot" ]; then
  pass "A3 A4 A7 A8 A12 A13 sweep matrix"
else
  fail "A3 A4 A7 A8 A12 A13 sweep matrix"
fi
active_root=$(new_sweep_root active)
printf 'nonce=active\n' >"$active_root/restart_pending"
printf 'nonce=active\n' >"$active_root/restart_persisted.active"
set_file_age "$active_root/restart_pending" 0
set_file_age "$active_root/restart_persisted.active" 3601
_restart_sweep_artifacts "$active_root" >/dev/null 2>&1
active_persisted_ok=0
[ -e "$active_root/restart_persisted.active" ] && active_persisted_ok=1
printf 'nonce=active-c\n' >"$active_root/restart_pending"
printf 'nonce=active-c\n' >"$active_root/restart_cancelled.active-c"
set_file_age "$active_root/restart_cancelled.active-c" 3601
_restart_sweep_artifacts "$active_root" >/dev/null 2>&1
if [ "$active_persisted_ok" -eq 1 ] \
  && [ -e "$active_root/restart_cancelled.active-c" ]; then
  pass "A5 A9 A10 canonical-bound identities remain visible"
else
  fail "A5 A9 A10 canonical-bound identities remain visible"
fi

# A4/A29: a request identity older than the historical 60-second grace but
# younger than the dedicated 600-second marker grace remains live.
root=$(new_sweep_root live-identity)
printf 'nonce=live-identity\n' >"$root/restart_pending.live-identity"
set_file_age "$root/restart_pending.live-identity" 61
_restart_sweep_artifacts "$root" >/dev/null 2>&1
[ -e "$root/restart_pending.live-identity" ] \
  && pass "A4 A29 live marker identity survives beyond 60 seconds" \
  || fail "A4 A29 live marker identity survives beyond 60 seconds"

# A11: basename-only extraction has one branch per terminal class.
for terminal_base in \
  restart_persisted.simple restart_cancelled.simple \
  restart_persisted.dot.ted restart_cancelled.dot.ted \
  restart_persisted.a_b-c restart_cancelled.a_b-c; do
  case "$terminal_base" in
    restart_persisted.*) extracted="${terminal_base#restart_persisted.}" ;;
    restart_cancelled.*) extracted="${terminal_base#restart_cancelled.}" ;;
  esac
  case "$terminal_base:$extracted" in
    restart_persisted.simple:simple|restart_cancelled.simple:simple|\
    restart_persisted.dot.ted:dot.ted|restart_cancelled.dot.ted:dot.ted|\
    restart_persisted.a_b-c:a_b-c|restart_cancelled.a_b-c:a_b-c) : ;;
    *) fail "A11 basename nonce extraction: $terminal_base" ;;
  esac
done
pass "A11 basename nonce extraction table"

# A14 future mtime is preserved with all diagnostic fields.
root=$(new_sweep_root future)
printf 'nonce=future\n' >"$root/restart_pending.future"
touch -t "$(_epoch_to_touch_stamp 4102444800)" "$root/restart_pending.future"
future_out=$(_restart_sweep_artifacts "$root" 2>&1)
case "$future_out" in
  *restart-artifact-future-mtime*"root=$root"*"age=-"*"grace=600"*"decision=preserve"*"class=marker-identity"*)
    if [ -e "$root/restart_pending.future" ]; then
      pass "A14 future mtime fails closed with six fields"
    else
      fail "A14 future mtime fails closed with six fields"
    fi ;;
  *) fail "A14 future mtime fails closed with six fields" ;;
esac

# A1, A2, A15, A17, A21, A25, A26: a fresh sweep reservation uses the same
# namespace as publishers. EEXIST chooses one winner and preserves the proof.
root=$(new_aged_artifact lock restart_persisted.terminal-lock nonce=terminal-lock 3601)
_restart_stage_marker_identity "$root" terminal-lock restart-sweep sweep lock-hold
set +e
_restart_stage_marker_identity "$root" terminal-lock publisher request live
lock_rc=$?
_restart_link_canonical_marker "$root" terminal-lock
canonical_rc=$?
set -e
assert_eq "A1 A15 A21 A25 fresh sweep reservation blocks same nonce" "4" "$lock_rc"
assert_eq "A2 canonical publication from reservation succeeds" "0" "$canonical_rc"
if _restart_artifact_nonce_matches "$root/restart_pending.terminal-lock" terminal-lock; then
  pass "A17 A26 losing sweeper leaves winner identity intact"
else
  fail "A17 A26 losing sweeper leaves winner identity intact"
fi
rm -f "$root/restart_pending" "$root/restart_pending.terminal-lock"

# A15: the real class-T path must honor a pre-existing nonce reservation. The
# terminal proof is old enough to reclaim, so EEXIST is its sole protection.
root=$(new_aged_artifact preheld-lock restart_persisted.preheld nonce=preheld 3601)
_restart_stage_marker_identity "$root" preheld publisher request live
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
if [ -e "$root/restart_persisted.preheld" ] \
  && _restart_artifact_nonce_matches "$root/restart_pending.preheld" preheld; then
  pass "A15 preheld nonce reservation blocks the real terminal sweep"
else
  fail "A15 preheld nonce reservation blocks the real terminal sweep"
fi

# A21: the class-T reservation is born fresh. At T-e, a concurrent class-M
# pass cannot reap it, and a publisher attempting the same nonce receives rc=4.
root=$(new_aged_artifact fresh-reservation restart_persisted.fresh-reservation \
  nonce=fresh-reservation 3601)
real_nonce_match=$(declare -f _restart_artifact_nonce_matches)
fresh_reservation_calls=0
fresh_reservation_rc=-1
_restart_artifact_nonce_matches() {
  if [ "$2" = fresh-reservation ] && [ "$1" = "$root/restart_pending" ]; then
    fresh_reservation_calls=$((fresh_reservation_calls + 1))
    if [ "$fresh_reservation_calls" -eq 2 ]; then
      _restart_sweep_marker_identities "$root" >/dev/null 2>&1
      set +e
      _restart_stage_marker_identity "$root" fresh-reservation publisher request live
      fresh_reservation_rc=$?
      set -e
    fi
  fi
  [ -f "$1" ] && grep -Fqx -- "nonce=$2" "$1" 2>/dev/null
}
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
eval "$real_nonce_match"
if [ "$fresh_reservation_rc" -eq 4 ]; then
  pass "A21 fresh class-T reservation survives class-M and blocks publisher"
else
  fail "A21 fresh class-T reservation survives class-M and blocks publisher"
fi

# A6 inode binding: replace the marker immediately after its age witness is
# captured. M-b must observe the different inode and preserve it.
root=$(new_aged_artifact marker-inode-recheck restart_pending.marker-recheck \
  $'nonce=marker-recheck\nold=yes' 601)
real_age_helper=$(declare -f _restart_artifact_age_allows_reclaim)
shadow_function_as _restart_artifact_age_allows_reclaim _restart_artifact_age_allows_reclaim_real
_restart_artifact_age_allows_reclaim() {
  local result
  result="$(_restart_artifact_age_allows_reclaim_real "$@")" || return $?
  if [ "$4" = marker-identity ] && [ "$2" = "$root/restart_pending.marker-recheck" ]; then
    printf 'nonce=marker-recheck\nfresh=yes\n' >"$root/.fresh-marker"
    mv "$root/.fresh-marker" "$2"
  fi
  printf '%s' "$result"
}
_restart_sweep_marker_identities "$root" >/dev/null 2>&1
eval "$real_age_helper"
unset -f _restart_artifact_age_allows_reclaim_real
if grep -Fqx 'fresh=yes' "$root/restart_pending.marker-recheck" 2>/dev/null; then
  pass "A6 marker inode recheck preserves a replacement"
else
  fail "A6 marker inode recheck preserves a replacement"
fi

# A5/M-c: canonical authority can appear after the class-level fast path. The
# final canonical check must preserve an unrelated marker identity.
root=$(new_aged_artifact marker-canonical-recheck restart_pending.marker-canonical \
  nonce=marker-canonical 601)
real_age_helper=$(declare -f _restart_artifact_age_allows_reclaim)
shadow_function_as _restart_artifact_age_allows_reclaim _restart_artifact_age_allows_reclaim_real
_restart_artifact_age_allows_reclaim() {
  local result
  result="$(_restart_artifact_age_allows_reclaim_real "$@")" || return $?
  if [ "$4" = marker-identity ] && [ "$2" = "$root/restart_pending.marker-canonical" ]; then
    printf 'nonce=other-live\n' >"$root/restart_pending"
  fi
  printf '%s' "$result"
}
_restart_sweep_marker_identities "$root" >/dev/null 2>&1
eval "$real_age_helper"
unset -f _restart_artifact_age_allows_reclaim_real
if [ -e "$root/restart_pending.marker-canonical" ]; then
  pass "A5 final canonical recheck preserves marker identity"
else
  fail "A5 final canonical recheck preserves marker identity"
fi

# A6/A18/A23: inode and canonical rechecks prevent stale observations from
# authorizing a replacement. Inject the canonical in the rm seam so the
# post-delete restoration sees the same inode.
root=$(new_aged_artifact marker-seam restart_pending.stale nonce=stale 601)
real_rm=$(declare -f rm 2>/dev/null || true)
rm() {
  if [ "$2" = "$root/restart_pending.stale" ]; then
    command rm "$@"
    ln "$root/restart_pending.stale-backup" "$root/restart_pending" 2>/dev/null || true
    return 0
  fi
  command rm "$@"
}
ln "$root/restart_pending.stale" "$root/restart_pending.stale-backup"
_restart_sweep_marker_identities "$root" >/dev/null 2>&1
unset -f rm
[ -n "$real_rm" ] && eval "$real_rm"
[ -e "$root/restart_pending.stale" ] && pass "A6 marker deletion is restored from canonical inode" \
  || fail "A6 marker deletion is restored from canonical inode"
command rm -f "$root/restart_pending.stale-backup" "$root/restart_pending"

# A18: two class-M sweepers observe the same aged inode. The first replaces it
# before the second's M-b check; only one deletion is then authorized.
root=$(new_aged_artifact marker-two-sweeper restart_pending.m-two \
  $'nonce=m-two\nold=yes' 601)
real_age_helper=$(declare -f _restart_artifact_age_allows_reclaim)
shadow_function_as _restart_artifact_age_allows_reclaim _restart_artifact_age_allows_reclaim_real
m_age_calls=0
_restart_artifact_age_allows_reclaim() {
  local result
  result="$(_restart_artifact_age_allows_reclaim_real "$@")" || return $?
  if [ "$4" = marker-identity ]; then
    m_age_calls=$((m_age_calls + 1))
    if [ "$m_age_calls" -eq 1 ]; then
      printf 'nonce=m-two\nfresh=yes\n' >"$root/.fresh-m-two"
      mv "$root/.fresh-m-two" "$2"
    fi
  fi
  printf '%s' "$result"
}
_restart_sweep_marker_identities "$root" >/dev/null 2>&1
eval "$real_age_helper"
unset -f _restart_artifact_age_allows_reclaim_real
if grep -Fqx 'fresh=yes' "$root/restart_pending.m-two" 2>/dev/null; then
  pass "A18 class-M loser preserves the replacement inode"
else
  fail "A18 class-M loser preserves the replacement inode"
fi

# A23: the adjacent content recheck preserves a post-stat fresh canonical.
root=$(new_aged_artifact legacy-seam restart_pending legacy 61)
real_grep=$(declare -f grep 2>/dev/null || true)
legacy_grep_calls=0
grep() {
  if [ "$1" = -q ] && [ "$2" = '^nonce=' ] && [ "$3" = "$root/restart_pending" ]; then
    legacy_grep_calls=$((legacy_grep_calls + 1))
    if [ "$legacy_grep_calls" -eq 2 ]; then
      command rm -f "$root/restart_pending"
      _restart_stage_and_link_marker "$root" fresh-legacy publisher request live
    fi
  fi
  command grep "$@"
}
_restart_reclaim_legacy_marker_if_stale "$root" >/dev/null 2>&1
unset -f grep
[ -n "$real_grep" ] && eval "$real_grep"
if _restart_artifact_nonce_matches "$root/restart_pending" fresh-legacy; then
  pass "A23 adjacent legacy content recheck preserves a fresh canonical"
else
  fail "A23 adjacent legacy content recheck preserves a fresh canonical"
fi

# A16: normal terminal sweep releases its lock and stage.
root=$(new_aged_artifact release restart_persisted.release nonce=release 3601)
_restart_sweep_artifacts "$root" >/dev/null 2>&1
set -- "$root"/.restart_pending.stage.*
if [ ! -e "$root/restart_pending.release" ] && [ ! -e "$1" ]; then
  pass "A16 normal sweep leaves no lock or stage"
else
  fail "A16 normal sweep leaves no lock or stage"
fi

# A19/A20: the terminal authority remains at its published pathname while a
# fresh lock is acquired; cancellation publication likewise precedes cleanup.
root=$(new_aged_artifact crash-authority restart_persisted.authority nonce=authority 3601)
_restart_stage_marker_identity "$root" authority restart-sweep sweep lock-hold
if [ -e "$root/restart_persisted.authority" ]; then
  pass "A19 lock acquisition preserves terminal authority pathname"
else
  fail "A19 lock acquisition preserves terminal authority pathname"
fi
rm -f "$root/restart_pending.authority"
_restart_stage_and_link_marker "$root" cancelled src scope label
clear_restart_drain_mode "$root" cancelled >/dev/null 2>&1 || true
[ -e "$root/restart_cancelled.cancelled" ] && pass "A20 cancellation authority is published first" \
  || fail "A20 cancellation authority is published first"

# A22: an abandoned sweep lock is recovered after marker grace; that pass can
# then reserve the nonce and reclaim the retained terminal proof.
root=$(new_sweep_root abandoned-lock)
printf 'nonce=abandoned\nsource=restart-sweep\n' >"$root/restart_pending.abandoned"
printf 'nonce=abandoned\n' >"$root/restart_persisted.abandoned"
set_file_age "$root/restart_pending.abandoned" 601
set_file_age "$root/restart_persisted.abandoned" 3601
_restart_sweep_artifacts "$root" >/dev/null 2>&1
if [ ! -e "$root/restart_pending.abandoned" ] \
  && [ ! -e "$root/restart_persisted.abandoned" ]; then
  pass "A22 abandoned sweep reservation is eventually reclaimed"
else
  fail "A22 abandoned sweep reservation is eventually reclaimed"
fi

# A24: sweep-on-drain defaults on, while an explicit zero is the only kill
# switch. Run the real request entry point with service probes stubbed.
root=$(new_sweep_root wiring-on)
kill_root=$(new_sweep_root wiring-off)
wiring_log="$S3A_TMP/sweep-wiring.log"
(
  guard_no_foreign_active_turns_or_warn() { return 0; }
  _launchd_job_state() { echo "not running"; }
  _restart_sweep_artifacts() { printf '%s\n' "$1" >>"$wiring_log"; }
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=0 \
    request_restart_drain_mode_or_fail test test.label 0 "$root" src >/dev/null 2>&1
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=0 AGENTDESK_RESTART_SWEEP_ON_DRAIN=0 \
    request_restart_drain_mode_or_fail test test.label 0 "$kill_root" src >/dev/null 2>&1
)
if [ "$(grep -Fxc -- "$root" "$wiring_log" 2>/dev/null || true)" -eq 1 ] \
  && ! grep -Fqx -- "$kill_root" "$wiring_log" 2>/dev/null; then
  pass "A24 sweep defaults on and explicit zero disables it"
else
  fail "A24 sweep defaults on and explicit zero disables it"
fi

# A27: replacing the class-T reservation before its final lock recheck is
# observed. The publisher identity survives and can publish canonically.
root=$(new_aged_artifact lock-recheck restart_persisted.lock-recheck nonce=lock-recheck 3601)
real_deadline=$(declare -f _restart_sweep_deadline_ok)
lock_deadline_calls=0
_restart_sweep_deadline_ok() {
  lock_deadline_calls=$((lock_deadline_calls + 1))
  if [ "$lock_deadline_calls" -eq 2 ]; then
    command rm -f "$root/restart_pending.lock-recheck"
    _restart_stage_marker_identity "$root" lock-recheck publisher request live
    # ext4 recycles the freed inode and both locks are born in the same
    # second, so force a distinct mtime to keep d:i:m distinguishable.
    set_file_age "$root/restart_pending.lock-recheck" 5
  fi
  return 0
}
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
eval "$real_deadline"
set +e
_restart_link_canonical_marker "$root" lock-recheck >/dev/null 2>&1
lock_recheck_rc=$?
set -e
if [ "$lock_recheck_rc" -eq 0 ] \
  && _restart_artifact_nonce_matches "$root/restart_pending" lock-recheck; then
  pass "A27 pre-recheck lock replacement survives and publishes"
else
  fail "A27 pre-recheck lock replacement survives and publishes"
fi

# A31: after the sweep lock is removed, a publisher can make canonical binding
# visible at T-e; that unconditional second check blocks terminal deletion.
root=$(new_aged_artifact canonical-recheck restart_persisted.canonical-recheck nonce=canonical-recheck 3601)
real_nonce_match=$(declare -f _restart_artifact_nonce_matches)
canonical_match_calls=0
_restart_artifact_nonce_matches() {
  if [ "$2" = canonical-recheck ] && [ "$1" = "$root/restart_pending" ]; then
    canonical_match_calls=$((canonical_match_calls + 1))
    if [ "$canonical_match_calls" -eq 2 ]; then
      command rm -f "$root/restart_pending.canonical-recheck"
      _restart_stage_and_link_marker "$root" canonical-recheck publisher request live
    fi
  fi
  [ -f "$1" ] && grep -Fqx -- "nonce=$2" "$1" 2>/dev/null
}
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
eval "$real_nonce_match"
if [ -e "$root/restart_persisted.canonical-recheck" ] \
  && _restart_artifact_nonce_matches "$root/restart_pending" canonical-recheck; then
  pass "A31 T-e canonical recheck blocks terminal deletion"
else
  fail "A31 T-e canonical recheck blocks terminal deletion"
fi

# A28: replace the terminal immediately after its age witness is captured.
# T-d must observe the new inode and suppress T-f.
root=$(new_aged_artifact terminal-seams restart_persisted.before-recheck \
  $'nonce=before-recheck\nold=yes' 3601)
real_age_helper=$(declare -f _restart_artifact_age_allows_reclaim)
shadow_function_as _restart_artifact_age_allows_reclaim _restart_artifact_age_allows_reclaim_real
_restart_artifact_age_allows_reclaim() {
  local result
  result="$(_restart_artifact_age_allows_reclaim_real "$@")" || return $?
  if [ "$4" = terminal-identity ] && [ "$2" = "$root/restart_persisted.before-recheck" ]; then
    printf 'nonce=before-recheck\nfresh=yes\n' >"$root/.fresh"
    mv "$root/.fresh" "$2"
  fi
  printf '%s' "$result"
}
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
eval "$real_age_helper"
unset -f _restart_artifact_age_allows_reclaim_real
if grep -Fqx 'fresh=yes' "$root/restart_persisted.before-recheck"; then
  pass "A28 terminal inode recheck preserves replacement"
else
  fail "A28 terminal inode recheck preserves replacement"
fi

# A30: marker-first order makes the same run able to reclaim its terminal.
root=$(new_sweep_root same-run)
printf 'nonce=same-run\n' >"$root/restart_pending.same-run"
printf 'nonce=same-run\n' >"$root/restart_persisted.same-run"
set_file_age "$root/restart_pending.same-run" 601
set_file_age "$root/restart_persisted.same-run" 3601
_restart_sweep_artifacts "$root" >/dev/null 2>&1
if [ ! -e "$root/restart_pending.same-run" ] \
  && [ ! -e "$root/restart_persisted.same-run" ]; then
  pass "A30 marker-first sweep reclaims both artifacts in one run"
else
  fail "A30 marker-first sweep reclaims both artifacts in one run"
fi

# A32: S1 is intentionally observable. Deletion after the final check can hit
# a replacement, but publication fails visibly once and a new nonce retries.
root=$(new_aged_artifact residual-s1 restart_pending.s1 nonce=s1 601)
real_deadline=$(declare -f _restart_sweep_deadline_ok)
_restart_sweep_deadline_ok() {
  command rm -f "$root/restart_pending.s1"
  _restart_stage_marker_identity "$root" s1 publisher request live
  return 0
}
_restart_sweep_marker_identities "$root" >/dev/null 2>&1
set +e
_restart_link_canonical_marker "$root" s1 >"$root/s1.out" 2>&1
s1_rc=$?
set -e
eval "$real_deadline"
if [ ! -e "$root/restart_pending.s1" ] && [ "$s1_rc" -eq 2 ]; then
  _restart_stage_and_link_marker "$root" s1-retry publisher request live
  pass "A32 residual S1 is visible and a fresh nonce retries"
else
  fail "A32 residual S1 is visible and a fresh nonce retries"
fi

# A33: S2 removes a fresh terminal identity and its same-inode fixed proof, so
# the persistence gate fails instead of returning a false green.
root=$(new_aged_artifact residual-s2 restart_persisted.s2 nonce=s2 3601)
real_deadline=$(declare -f _restart_sweep_deadline_ok)
s2_deadline_counter="$root/deadline-calls"
printf 0 >"$s2_deadline_counter"
_restart_sweep_deadline_ok() {
  local calls
  calls=$(($(command cat "$s2_deadline_counter") + 1))
  printf '%s' "$calls" >"$s2_deadline_counter"
  [ "$calls" -ne 1 ] || _restart_terminal_publish "$root" restart_persisted s2 fresh=yes
  return 0
}
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
eval "$real_deadline"
set +e
wait_for_restart_persistence_or_fail probe "$root" s2 1 >/dev/null 2>&1
s2_gate_rc=$?
set -e
if [ ! -e "$root/restart_persisted.s2" ] \
  && [ ! -e "$root/restart_persisted" ] && [ "$s2_gate_rc" -ne 0 ]; then
  pass "A33 residual S2 removes its fixed index and remains gate-visible"
else
  fail "A33 residual S2 removes its fixed index and remains gate-visible"
fi

# The fixed-index recheck is unlink-adjacent: replacement with a live request's
# new inode at that seam must preserve the replacement and its successful gate.
root=$(new_aged_artifact residual-s2-fixed-replacement restart_persisted.old nonce=old 3601)
ln "$root/restart_persisted.old" "$root/restart_persisted"
real_stat=$(declare -f stat 2>/dev/null || true)
s2_fixed_stat_calls=0
stat() {
  if { [ "$1" = -f ] || [ "$1" = -c ]; } && [ "$2" = '%d:%i' ] \
    && [ "$3" = "$root/restart_persisted" ]; then
    s2_fixed_stat_calls=$((s2_fixed_stat_calls + 1))
    if [ "$s2_fixed_stat_calls" -eq 1 ]; then
      command rm -f "$root/restart_persisted"
      _restart_terminal_publish "$root" restart_persisted live fresh=yes
    fi
  fi
  command stat "$@"
}
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
unset -f stat
[ -n "$real_stat" ] && eval "$real_stat"
set +e
wait_for_restart_persistence_or_fail probe "$root" live 1 >/dev/null 2>&1
live_gate_rc=$?
set -e
if grep -Fqx 'nonce=live' "$root/restart_persisted" 2>/dev/null \
  && [ -e "$root/restart_persisted.live" ] && [ "$live_gate_rc" -eq 0 ]; then
  pass "A33 adjacent fixed recheck preserves a replacement and live gate"
else
  fail "A33 adjacent fixed recheck preserves a replacement and live gate"
fi

root=$(new_aged_artifact residual-s2-unrelated restart_persisted.s2-unrelated nonce=s2-unrelated 3601)
printf 'nonce=someone-else\n' >"$root/restart_persisted"
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
[ ! -e "$root/restart_persisted.s2-unrelated" ] \
  && grep -Fqx 'nonce=someone-else' "$root/restart_persisted" 2>/dev/null \
  && pass "A33 residual S2 preserves an unrelated fixed index" \
  || fail "A33 residual S2 preserves an unrelated fixed index"

# A34: S3 after lock recheck has the same visible failure and retry shape.
root=$(new_aged_artifact residual-s3 restart_persisted.s3 nonce=s3 3601)
real_rm=$(declare -f rm 2>/dev/null || true)
rm() {
  if [ "$2" = "$root/restart_pending.s3" ]; then
    command rm "$@"
    _restart_stage_marker_identity "$root" s3 publisher request live
    command rm "$@"
    return 0
  fi
  command rm "$@"
}
_restart_sweep_terminal_identities "$root" >/dev/null 2>&1
unset -f rm
[ -n "$real_rm" ] && eval "$real_rm"
set +e
_restart_link_canonical_marker "$root" s3 >/dev/null 2>&1
s3_rc=$?
set -e
if [ ! -e "$root/restart_pending.s3" ] && [ "$s3_rc" -eq 2 ]; then
  _restart_stage_and_link_marker "$root" s3-retry publisher request live
  pass "A34 residual S3 is visible and a fresh nonce retries"
else
  fail "A34 residual S3 is visible and a fresh nonce retries"
fi

# A35: each destruction site gets a successful guard immediately followed by a
# failing adjacent guard, modeling stop/resume with virtual time advanced.
root=$(new_aged_artifact deadline restart_pending.deadline nonce=deadline 601)
root_t=$(new_aged_artifact deadline-terminal restart_persisted.deadline-terminal nonce=deadline-terminal 3601)
real_deadline=$(declare -f _restart_sweep_deadline_ok)
deadline_calls=0
_restart_sweep_deadline_ok() {
  deadline_calls=$((deadline_calls + 1))
  case "$deadline_calls" in
    1|4) return 0 ;;
    *) return 1 ;;
  esac
}
_restart_sweep_marker_identities "$root" >/dev/null 2>&1
marker_preserved=0
[ -e "$root/restart_pending.deadline" ] && marker_preserved=1
deadline_calls=0
_restart_sweep_terminal_identities "$root_t" >/dev/null 2>&1
eval "$real_deadline"
if [ "$marker_preserved" -eq 1 ] \
  && [ -e "$root_t/restart_persisted.deadline-terminal" ] \
  && [ -e "$root_t/restart_pending.deadline-terminal" ]; then
  pass "A35 adjacent deadline guards stop resumed destruction"
else
  fail "A35 adjacent deadline guards stop resumed destruction"
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
