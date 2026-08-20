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
touch "$TMP_RUNTIME3/still-pending/restart_pending"
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
rm() {
  if [ "$1" = "-f" ] && [ "$2" = "$TMPDIR_TEST/restart_pending" ] \
    && [ -f "$TMPDIR_TEST/restart_cancelled" ]; then
    marker_remove_saw_cancel=1
  fi
  command rm "$@"
}
clear_restart_drain_mode "$TMPDIR_TEST" >/dev/null 2>&1 || true
unset -f rm
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

# --- 8m: a new request pre-cleans EVERY root it will later consult ----------
# Not cosmetic. gateway_lease_recovery.rs (~:428) treats the mere presence of a
# restart_persisted written during the current process lifetime as proof that a
# promotion handoff committed — that check is lifetime-scoped, not nonce-scoped.
# Two deploys inside one runtime lifetime is the ordinary retry pattern, so an
# acknowledgement from the previous request left lying in the runtime root is a
# stale positive for the next one. The old code pre-cleaned only the directory
# it wrote to; now that both are written, both must be cleaned.
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=stale-request\n' >"$DUAL_PRIMARY/restart_persisted"
printf 'nonce=stale-request\n' >"$DUAL_PRIMARY/restart_cancelled"
printf 'nonce=stale-request\n' >"$DUAL_MIRROR/restart_persisted"
printf 'nonce=stale-request\n' >"$DUAL_MIRROR/restart_cancelled"
_launchd_job_state() { echo "running"; }
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  PATH="$TMP_FIXTURE_DIR/bin_fail:$PATH" \
  AGENTDESK_RESTART_DRAIN_ACK_WAIT=1 \
  request_restart_drain_mode_or_fail "test" "test.label" 0 "$DUAL_PRIMARY" "smoke-test" \
  >/dev/null 2>&1
set -e
unset -f _launchd_job_state
if [ -e "$DUAL_PRIMARY/restart_persisted" ]; then
  fail "8m: stale ack removed from the shell root before the new request arms"
else
  pass "8m: stale ack removed from the shell root before the new request arms"
fi
if [ -e "$DUAL_MIRROR/restart_persisted" ]; then
  fail "8m: stale ack removed from the runtime root before the new request arms"
else
  pass "8m: stale ack removed from the runtime root before the new request arms"
fi
if grep -q '^nonce=stale-request$' "$DUAL_MIRROR/restart_cancelled" 2>/dev/null; then
  fail "8m: the previous request's cancellation does not survive into this one"
else
  pass "8m: the previous request's cancellation does not survive into this one"
fi

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

# --- 8g (E): cancellation recovers the nonce from whichever root still has it
# The runtime consumes the marker in the ONE directory it watches. The nonce
# for the cancellation must then be read from the other root, or the surviving
# poller receives a cancellation it cannot bind to its own request.
rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=consumed-by-runtime\n' >"$DUAL_PRIMARY/restart_pending"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  clear_restart_drain_mode "$DUAL_PRIMARY" >/dev/null 2>&1
rc=$?
set -e
assert_eq "8g (E): clear succeeds when only one root still holds the marker" "0" "$rc"
if grep -q '^nonce=consumed-by-runtime$' "$DUAL_MIRROR/restart_cancelled" 2>/dev/null \
  && grep -q '^nonce=consumed-by-runtime$' "$DUAL_PRIMARY/restart_cancelled" 2>/dev/null; then
  pass "8g: nonce-bound cancellation reaches the root whose marker was consumed"
else
  fail "8g: nonce-bound cancellation reaches the root whose marker was consumed"
fi

rm -f "$DUAL_PRIMARY"/restart_* "$DUAL_MIRROR"/restart_* 2>/dev/null || true
printf 'nonce=only-in-mirror\n' >"$DUAL_MIRROR/restart_pending"
set +e
AGENTDESK_RESTART_MARKER_MIRROR_ROOT="$DUAL_MIRROR" \
  clear_restart_drain_mode "$DUAL_PRIMARY" >/dev/null 2>&1
set -e
if grep -q '^nonce=only-in-mirror$' "$DUAL_PRIMARY/restart_cancelled" 2>/dev/null; then
  pass "8g: nonce is recovered from the mirror when the shell root has none"
else
  fail "8g: nonce is recovered from the mirror when the shell root has none"
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

echo
echo "==== Results ===="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '  failed: %s\n' "${FAIL_NAMES[@]}" >&2
  exit 1
fi
exit 0
