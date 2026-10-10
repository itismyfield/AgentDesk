#!/usr/bin/env bash
# Exercise the production rollback/stop branches with isolated files and stubbed
# process commands, so delayed bootout and stale lock PIDs never touch a service.

# shellcheck disable=SC2034  # Fixture globals are read by functions loaded through eval.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-rollback-lock-test.XXXXXX")
trap 'python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1])" "$TMP_ROOT"' EXIT
FAILURES=0
fail() { echo "  FAIL: $1" >&2; FAILURES=$((FAILURES + 1)); }
pass() { echo "  PASS: $1"; }
assert_eq() {
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: expected '$2', got '$3'"; fi
}

# Reuse the rollback guard suite's extractor; heredoc braces do not end a function.
extract_function() {
    local function_name="$1" source_file="${2:-$DEPLOY_SH}"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && heredoc && /^PY$/ { heredoc = 0; next }
        printing && /<<.?PY.?$/ { heredoc = 1; next }
        printing && !heredoc && /^}$/ { exit }
    ' "$source_file"
}

for fn in _rollback_release_binary _signal_release_lock_pid _wait_release_stopped; do
    body="$(extract_function "$fn")"
    # Older copies lack the new helpers; still execute their real rollback and
    # main stop branches so a missing definition alone cannot manufacture a RED.
    if [ -z "$body" ]; then continue; fi
    bash -n <<<"$body" || { fail "$fn cannot be extracted"; exit 1; }
    eval "$body"
done
main_stop="$(awk '
    /^echo "▸ Stopping release[.][.][.]"$/ { printing = 1 }
    printing && /^_post_deploy_smoke_log_identity_and_size[(][)] [{]$/ { exit }
    printing { print }
' "$DEPLOY_SH")"
if [ -z "$main_stop" ] || ! bash -n <<<"$main_stop"; then
    fail "main stop branch cannot be extracted"
    exit 1
fi

ADK_REL="$TMP_ROOT/release"
REL_BINARY="$ADK_REL/bin/agentdesk"
REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
PLIST_REL="com.agentdesk.test"
REL_PORT=18791
DEPLOY_HEALTH_RETRIES=1
DEPLOY_HEALTH_DELAY_SECS=1
ADK_DEFAULT_LOOPBACK=127.0.0.1
mkdir -p "$ADK_REL/bin" "$ADK_REL/runtime"
event() { printf '%s at=%s\n' "$*" "$(cat "$TMP_ROOT/tick")" >>"$TMP_ROOT/events"; }
_launchd_domain() { echo gui/501; }
_rollback_would_brick_on_migration() { return 1; }
_rollback_would_revert_o_writer() { return 1; }
chflags() { return 0; }
xattr() { return 0; }
tmux() { event "tmux $*"; return 0; }
start_release_tmux_fallback() { event fallback; return 0; }
wait_for_http_service_health() { event health; return 0; }
sleep() {
    event "sleep $*"
    printf '%s\n' "$(( $(cat "$TMP_ROOT/tick") + $1 ))" >"$TMP_ROOT/tick"
}
launchctl() {
    event "launchctl $*"
    case "$1" in
        bootout)
            if [ "$BOOTOUT_RC" -ne 0 ]; then echo "stub bootout error=$BOOTOUT_RC" >&2; fi
            return "$BOOTOUT_RC" ;;
        print) [ "$(cat "$TMP_ROOT/tick")" -lt "$JOB_EXIT_AT" ] ;;
        bootstrap) return 0 ;;
        *) fail "unexpected launchctl invocation: $*"; return 99 ;;
    esac
}
ps() {
    event "ps $*"
    [ "${*: -1}" = "$FIXTURE_PID" ] || return 1
    [ "$(cat "$TMP_ROOT/tick")" -lt "$PID_EXIT_AT" ] || return 1
    [ ! -f "$TMP_ROOT/killed" ] || return 1
    if [ -f "$TMP_ROOT/reused" ] || [ "$(cat "$TMP_ROOT/tick")" -ge "$REUSE_AT" ]; then
        echo "/usr/bin/python3 unrelated.py"
    else
        printf '%s\n' "$PS_COMMAND"
    fi
}
# Shadow the Bash builtin as well as any PATH command: no signal reaches the OS.
kill() {
    event "kill $*"
    [ "$2" = "$FIXTURE_PID" ] || return 1
    case "$1" in
        -0)
            [ "$(cat "$TMP_ROOT/tick")" -lt "$PID_EXIT_AT" ] && [ ! -f "$TMP_ROOT/killed" ] || return 1
            if [ "$REUSE_BEFORE_SIGKILL" = 1 ] && [ "$(cat "$TMP_ROOT/tick")" = 15 ]; then
                printf reused >"$TMP_ROOT/reused"
            fi
            return 0 ;;
        -9) printf killed >"$TMP_ROOT/killed"; return 0 ;;
        *) fail "unexpected signal: $*"; return 99 ;;
    esac
}

setup_case() {
    mkdir -p "$ADK_REL/bin" "$ADK_REL/runtime"
    printf '0\n' >"$TMP_ROOT/tick"
    : >"$TMP_ROOT/events"
    rm -f "$TMP_ROOT/killed" "$TMP_ROOT/reused"
    printf 'new\n' >"$REL_BINARY"
    printf 'old\n' >"$REL_BINARY_BACKUP"
    FIXTURE_PID=5261
    printf '%s\n' "$FIXTURE_PID" >"$ADK_REL/runtime/dcserver.lock"
    OLD_PID=9999
    PS_COMMAND="$REL_BINARY dcserver"
    JOB_EXIT_AT=0 PID_EXIT_AT=0 REUSE_AT=100 REUSE_BEFORE_SIGKILL=0 BOOTOUT_RC=0
}
run_rollback() {
    local rc=0
    OUT="$(_rollback_release_binary 2>&1)" || rc=$?
    assert_eq "rollback preserves its best-effort return" 0 "$rc"
}
run_main_stop() {
    MAIN_RC=0
    OUT="$(eval "$main_stop" 2>&1)" || MAIN_RC=$?
}
assert_no_event() {
    if grep -Eq "$2" "$TMP_ROOT/events"; then fail "$1: $(cat "$TMP_ROOT/events")"; else pass "$1"; fi
}
assert_event() {
    if grep -Eq "$2" "$TMP_ROOT/events"; then pass "$1"; else fail "$1: $(cat "$TMP_ROOT/events")"; fi
}

echo "rollback waits for both launchd removal and the current lock PID"
setup_case
JOB_EXIT_AT=3 PID_EXIT_AT=5
run_rollback
assert_event "the real rollback branch reached bootout" '^launchctl bootout gui/501/com.agentdesk.test at=0$'
assert_event "bootstrap waits for PID exit after job removal" '^launchctl bootstrap .* at=5$'
assert_event "rollback reads the current lock PID" '^kill -0 5261 at='
assert_no_event "rollback does not use the pre-deploy OLD_PID" '^kill .*9999'
assert_eq "rollback restored the backup" old "$(cat "$REL_BINARY")"

setup_case
JOB_EXIT_AT=5 PID_EXIT_AT=3
run_rollback
assert_event "bootstrap waits for job removal after PID exit" '^launchctl bootstrap .* at=5$'

setup_case
JOB_EXIT_AT=15 PID_EXIT_AT=15
run_rollback
assert_event "a stop completed exactly at the cap is accepted" '^launchctl bootstrap .* at=15$'

echo "rollback timeout cannot claim successful restore or health"
setup_case
JOB_EXIT_AT=99 PID_EXIT_AT=99
run_rollback
assert_eq "rollback waits only to the 15-second cap" 15 "$(cat "$TMP_ROOT/tick")"
assert_eq "timeout leaves the promoted binary in place" new "$(cat "$REL_BINARY")"
assert_eq "timeout preserves the rollback backup" old "$(cat "$REL_BINARY_BACKUP")"
assert_no_event "timeout never bootstraps, checks health, or starts fallback" '^(launchctl bootstrap|health|fallback)'
assert_no_event "rollback never escalates to SIGKILL" '^kill -9 '
if [[ "$OUT" == *"Release stop timed out"* ]]; then pass "timeout emits a diagnostic"; else fail "timeout diagnostic missing: $OUT"; fi

setup_case
JOB_EXIT_AT=2 PID_EXIT_AT=2 BOOTOUT_RC=5
run_rollback
if [[ "$OUT" == *"stub bootout error=5"* ]]; then pass "bootout diagnostics remain visible"; else fail "bootout diagnostics were swallowed: $OUT"; fi
assert_event "failed bootout still waits for observed stop" '^launchctl bootstrap .* at=2$'

echo "the main stop branch narrows every lock-derived signal"
for command in "/usr/bin/python3 unrelated.py" "/bin/bash -c $REL_BINARY dcserver" \
    "$REL_BINARY codex-tmux-wrapper dcserver" "$REL_BINARY dcserver-extra" \
    "$REL_BINARY --json codex-tmux-wrapper dcserver" "$REL_BINARY --json dcserver-extra" \
    "$REL_BINARY.backup dcserver" ""; do
    setup_case
    OLD_PID="$FIXTURE_PID" PID_EXIT_AT=99 PS_COMMAND="$command"
    run_main_stop
    assert_event "bystander case reaches the real main stop branch" '^launchctl bootout gui/501/com.agentdesk.test at=0$'
    assert_eq "main stops after the job is removed" 0 "$MAIN_RC"
    assert_no_event "bystander argv receives zero kill calls: $command" '^kill '
    if [[ "$OUT" == *"⚠"* ]]; then pass "bystander warning emitted"; else fail "bystander warning missing: $OUT"; fi
done
for pid in 0 -1 52616x '5261 6'; do
    setup_case
    OLD_PID="$pid" PID_EXIT_AT=99
    run_main_stop
    assert_no_event "invalid lock PID receives zero kill calls: $pid" '^kill '
    assert_no_event "invalid lock PID is rejected before ps: $pid" '^ps '
done

setup_case
OLD_PID="$FIXTURE_PID" JOB_EXIT_AT=4 PID_EXIT_AT=6
run_main_stop
assert_eq "main stop waits for a matched dcserver to exit" 0 "$MAIN_RC"
assert_eq "main waits for the PID after launchd removal" 6 "$(cat "$TMP_ROOT/tick")"
assert_event "positive control reaches the existing signal path" '^kill -0 5261 at='
assert_no_event "normal exit does not escalate" '^kill -9 '

setup_case
OLD_PID="$FIXTURE_PID" JOB_EXIT_AT=16 PID_EXIT_AT=99
run_main_stop
assert_eq "existing main SIGKILL escalation succeeds after final observation" 0 "$MAIN_RC"
assert_event "main retains SIGKILL only at its existing cap" '^kill -9 5261 at=15$'
assert_eq "main verifies stop after the escalation grace" 16 "$(cat "$TMP_ROOT/tick")"

setup_case
OLD_PID="$FIXTURE_PID" JOB_EXIT_AT=16 PID_EXIT_AT=99 REUSE_BEFORE_SIGKILL=1
run_main_stop
assert_event "reuse happens after the last positive existence probe" '^kill -0 5261 at=15$'
assert_no_event "PID reused before escalation receives no SIGKILL" '^kill -9 '
assert_eq "main confirms job removal after identity changed" 0 "$MAIN_RC"

setup_case
OLD_PID="$FIXTURE_PID" JOB_EXIT_AT=99 PID_EXIT_AT=99
run_main_stop
assert_eq "main fails closed if job registration survives SIGKILL" 1 "$MAIN_RC"
assert_eq "main's final post-escalation check stays bounded" 16 "$(cat "$TMP_ROOT/tick")"

ADK_REL="$TMP_ROOT/release with spaces"
REL_BINARY="$ADK_REL/bin/agentdesk"
REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
setup_case
JOB_EXIT_AT=3 PID_EXIT_AT=5 PS_COMMAND="$REL_BINARY dcserver --json"
run_rollback
assert_event "a canonical path with spaces and dcserver arguments is matched" '^kill -0 5261 at='
assert_event "a path with spaces still waits for the live PID" '^launchctl bootstrap .* at=5$'

setup_case
JOB_EXIT_AT=3 PID_EXIT_AT=5 PS_COMMAND="$REL_BINARY --json dcserver"
run_rollback
assert_event "the global --json flag before dcserver is matched" '^kill -0 5261 at='
assert_event "a global flag does not skip waiting for the live PID" '^launchctl bootstrap .* at=5$'

if [ "$FAILURES" -ne 0 ]; then echo "$FAILURES assertion(s) failed" >&2; exit 1; fi
echo "all rollback/lock PID checks passed"
