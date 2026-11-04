#!/usr/bin/env bash
# Exercise the deploy signing functions with isolated command stubs only.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-keychain-test.XXXXXX")
cleanup() {
    if [ "${AGENTDESK_TEST_KEEP_TMP:-0}" = 1 ]; then
        printf 'stub evidence: %s\n' "$TMP_ROOT"
    else
        python3 -c 'import shutil,sys; shutil.rmtree(sys.argv[1])' "$TMP_ROOT"
    fi
}
trap cleanup EXIT

extract_function() {
    awk -v start="^${1}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

mkdir -p "$TMP_ROOT/stubs" "$TMP_ROOT/tools"
ln -s /bin/bash "$TMP_ROOT/tools/bash"
for tool in grep head cat mkdir dirname date chmod; do
    ln -s "$(command -v "$tool")" "$TMP_ROOT/tools/$tool"
done
ln -s "$(command -v python3)" "$TMP_ROOT/tools/python3"
cat >"$TMP_ROOT/stubs/security" <<'STUB'
#!/bin/bash
set -euo pipefail
printf 'security' >>"$CASE_ROOT/argv"
printf ' <%s>' "$@" >>"$CASE_ROOT/argv"
printf '\n' >>"$CASE_ROOT/argv"
case "$1" in
    show-keychain-info) exit "$STUB_INFO_RC" ;;
    find-identity) printf '1) ABCDEF "%s"\n' "$AGENTDESK_CODESIGN_IDENTITY" ;;
    unlock-keychain)
        [ ! -t 0 ] || exit 93
        if ( : </dev/tty ) 2>/dev/null; then
            printf 'controlling tty available\n' >"$CASE_ROOT/unlock.tty"
            exit 94
        fi
        printf 'no controlling tty; stdin is not tty\n' >"$CASE_ROOT/unlock.tty"
        cat >"$CASE_ROOT/unlock.stdin"
        if [ "$STUB_NOISY" = 1 ]; then
            printf '%s\n' 'stub-password-7775-never-argv' >&2
        fi
        exit "$STUB_UNLOCK_RC"
        ;;
    *) exit 91 ;;
esac
STUB
cat >"$TMP_ROOT/stubs/agent-op" <<'STUB'
#!/bin/bash
set -euo pipefail
printf 'agent-op' >>"$CASE_ROOT/argv"
printf ' <%s>' "$@" >>"$CASE_ROOT/argv"
printf '\n' >>"$CASE_ROOT/argv"
if [ "$STUB_NOISY" = 1 ]; then
    printf '%s\n' 'stub-password-7775-never-argv' >&2
fi
printf '%s\n' 'stub-password-7775-never-argv'
exit "$STUB_OP_RC"
STUB
cat >"$TMP_ROOT/stubs/codesign" <<'STUB'
#!/bin/bash
set -euo pipefail
printf 'codesign' >>"$CASE_ROOT/argv"
printf ' <%s>' "$@" >>"$CASE_ROOT/argv"
printf '\n' >>"$CASE_ROOT/argv"
case "$1" in
    -v) [ "$STUB_MATCHING" = 1 ] || [ -f "$CASE_ROOT/signed" ] ;;
    -dvv)
        printf 'Authority=%s\nIdentifier=%s\n' "$AGENTDESK_CODESIGN_IDENTITY" "$BUNDLE_ID" >&2
        ;;
    -f)
        [ "$STUB_SIGN_RC" = 0 ] || exit "$STUB_SIGN_RC"
        : >"$CASE_ROOT/signed"
        ;;
    *) exit 92 ;;
esac
STUB
cat >"$TMP_ROOT/stubs/tmux" <<'STUB'
#!/bin/bash
printf 'tmux stub\n' >>"$CASE_ROOT/argv"
STUB
chmod +x "$TMP_ROOT/stubs/"*

{
    printf 'set -euo pipefail\n'
    awk '/^(CODESIGN_IDENTITY|ALLOW_ADHOC_RELEASE_SIGN|CODESIGN_KEYCHAIN_[A-Z_]+|RESOLVED_RELEASE_SIGNING_MODE)=/ { print }' "$DEPLOY_SH"
    for function_name in _unlock_codesign_keychain_if_configured sign_binary_with_fallback _deploy_peer_env_prelude _spawn_detached_helper; do
        body="$(extract_function "$function_name")"
        [ -n "$body" ] || { printf 'FAIL: missing function %s\n' "$function_name" >&2; exit 1; }
        printf '%s\n' "$body"
    done
    cat <<'DRIVER'
if [ "$CASE_ACTION" = detached ]; then
    ADK_REL="$CASE_ROOT/release"
    REPORT_CHANNEL_ID=7775
    REPORT_PROVIDER=stub
    REPO="$CASE_ROOT"
    SCRIPT_DIR="$CASE_ROOT"
    DEPLOY_DELAY_SECS=0
    DEPLOY_TEST_MODE=1
    POST_DEPLOY_SMOKE_SCOPE=api
    DEPLOY_LOCK_FILE="$CASE_ROOT/lock"
    DEPLOY_LOCK_TIMEOUT_SECS=1
    DEPLOY_PEER_VERDICT_TIMEOUT_SECS=1
    DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS=1
    PLIST_REL=stub
    _spawn_detached_helper
elif [ "$CASE_ACTION" = prelude ]; then
    _deploy_peer_env_prelude
else
    [ "$STUB_XTRACE" = 0 ] || set -x
    sign_binary_with_fallback "$CASE_ROOT/agentdesk"
    printf 'SIGNING_MODE=%s\n' "$RESOLVED_RELEASE_SIGNING_MODE"
fi
DRIVER
} >"$TMP_ROOT/driver.sh"

failures=0
cases=0
checks=0
check() {
    local message="$1"
    shift
    checks=$((checks + 1))
    if ! "$@"; then
        printf 'FAIL: %s: %s\n' "$case_name" "$message" >&2
        failures=$((failures + 1))
    fi
}
not_contains() { ! grep -Fq -- "$1" "$2"; }
contains() { grep -Fq -- "$1" "$2"; }
new_case() {
    case_name="$1"
    cases=$((cases + 1))
    export CASE_ROOT="$TMP_ROOT/$case_name"
    mkdir -p "$CASE_ROOT/bin"
    : >"$CASE_ROOT/argv"
    local stub
    for stub in security codesign tmux agent-op; do
        ln -s "$TMP_ROOT/stubs/$stub" "$CASE_ROOT/bin/$stub"
    done
    export STUB_INFO_RC=36 STUB_UNLOCK_RC=0 STUB_OP_RC=0 STUB_SIGN_RC=0
    export STUB_MATCHING=0 STUB_NOISY=0 STUB_XTRACE=0 CASE_ACTION=sign
    export AGENTDESK_CODESIGN_IDENTITY='Developer ID Application: Stub Identity'
    export AGENTDESK_CODESIGN_KEYCHAIN_OP_REF='op://AgentDesk/test-host/password'
    export AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=0 BUNDLE_ID=com.stub.agentdesk
    export AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE="$CASE_ROOT/retired-password-file"
    unset AGENTDESK_CODESIGN_KEYCHAIN_NAME
}
run_case() {
    set +e
    if [ "${1:-}" = pty ]; then
        PATH="$CASE_ROOT/bin:$TMP_ROOT/tools" python3 - "$TMP_ROOT/driver.sh" "$CASE_ROOT" >"$CASE_ROOT/output" 2>&1 <<'PY'
import errno
import os
import pty
import sys
import traceback

driver, case_root = sys.argv[1:]
output_fd = os.dup(1)
pid, master = pty.fork()
if pid == 0:
    try:
        with open("/dev/tty", "rb", buffering=0) as tty:
            if not os.isatty(tty.fileno()):
                raise RuntimeError("PTY child has no controlling tty")
        with open(os.path.join(case_root, "driver.tty"), "w") as evidence:
            evidence.write("controlling tty available before driver redirection\n")
        with open(os.devnull, "rb") as stdin:
            os.dup2(stdin.fileno(), 0)
        os.dup2(output_fd, 1)
        os.dup2(output_fd, 2)
        os.close(output_fd)
        os.execv("/bin/bash", ["/bin/bash", driver])
    except BaseException:
        traceback.print_exc()
        os._exit(125)

try:
    while True:
        try:
            data = os.read(master, 4096)
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not data:
            break
        os.write(output_fd, data)
finally:
    _, status = os.waitpid(pid, 0)
    os.close(master)
    os.close(output_fd)
exit_code = os.waitstatus_to_exitcode(status)
sys.exit(exit_code if exit_code >= 0 else 128 - exit_code)
PY
    else
        PATH="$CASE_ROOT/bin:$TMP_ROOT/tools" /bin/bash "$TMP_ROOT/driver.sh" >"$CASE_ROOT/output" 2>&1 </dev/null
    fi
    case_rc=$?
    set -e
    if [ "${1:-}" = pty ]; then
        check 'PTY child had a controlling tty before driver redirection' contains 'controlling tty available before driver redirection' "$CASE_ROOT/driver.tty"
    fi
    check 'password absent from every recorded argv' not_contains 'stub-password-7775-never-argv' "$CASE_ROOT/argv"
    check 'password absent from output, errors, and xtrace' not_contains 'stub-password-7775-never-argv' "$CASE_ROOT/output"
}
assert_sign_attempt() {
    check 'Developer ID codesign attempted' contains 'codesign <-f> <-s> <Developer ID Application: Stub Identity>' "$CASE_ROOT/argv"
    check 'no ad-hoc signing fallback' not_contains 'codesign <-f> <-s> <->' "$CASE_ROOT/argv"
}
assert_warning() { check 'one warning emitted' test "$(grep -c '^⚠' "$CASE_ROOT/output")" = 1; }
assert_no_unlock() {
    check 'agent-op not called' not_contains 'agent-op' "$CASE_ROOT/argv"
    check 'unlock not called' not_contains 'security <unlock-keychain>' "$CASE_ROOT/argv"
}

new_case locked_with_ref
STUB_XTRACE=1
run_case pty
check 'signing succeeds' test "$case_rc" = 0
check 'host-local reference read' contains 'agent-op <read> <op://AgentDesk/test-host/password>' "$CASE_ROOT/argv"
check 'login keychain default used' contains "security <unlock-keychain> <$HOME/Library/Keychains/login.keychain-db>" "$CASE_ROOT/argv"
printf '%s\n' 'stub-password-7775-never-argv' >"$TMP_ROOT/expected.stdin"
check 'agent-op output arrives unchanged on unlock stdin' cmp -s "$TMP_ROOT/expected.stdin" "$CASE_ROOT/unlock.stdin"
check 'unlock has no controlling tty and uses non-tty stdin' contains 'no controlling tty; stdin is not tty' "$CASE_ROOT/unlock.tty"
check 'unlock has no password argument' not_contains 'security <unlock-keychain> <-p>' "$CASE_ROOT/argv"
assert_sign_attempt

new_case unlocked
STUB_INFO_RC=0
run_case
check 'signing succeeds' test "$case_rc" = 0
assert_no_unlock
assert_sign_attempt

new_case matching_signature
STUB_MATCHING=1
run_case
check 'matching signature succeeds' test "$case_rc" = 0
assert_no_unlock
check 'keychain state not probed when skipping' not_contains 'security <show-keychain-info>' "$CASE_ROOT/argv"
check 'signature preserved' not_contains 'codesign <-f>' "$CASE_ROOT/argv"

new_case locked_without_ref
unset AGENTDESK_CODESIGN_KEYCHAIN_OP_REF
run_case
check 'signing still succeeds' test "$case_rc" = 0
assert_no_unlock
assert_warning
assert_sign_attempt

new_case locked_without_agent_op
unlink "$CASE_ROOT/bin/agent-op"
run_case
check 'signing still succeeds' test "$case_rc" = 0
assert_no_unlock
assert_warning
assert_sign_attempt

new_case unlock_failure
STUB_UNLOCK_RC=36
STUB_NOISY=1
STUB_XTRACE=1
run_case
check 'unlock failure still permits signing attempt' test "$case_rc" = 0
assert_warning
assert_sign_attempt

new_case agent_op_failure
STUB_OP_RC=1
STUB_NOISY=1
STUB_XTRACE=1
run_case
check 'agent-op failure still permits signing attempt' test "$case_rc" = 0
assert_warning
assert_sign_attempt

new_case signing_failure
STUB_UNLOCK_RC=36
STUB_SIGN_RC=1
run_case
check 'signing failure propagates nonzero' test "$case_rc" != 0
assert_warning
assert_sign_attempt
check 'failed signing does not report success mode' not_contains 'SIGNING_MODE=' "$CASE_ROOT/output"

new_case keychain_override
export AGENTDESK_CODESIGN_KEYCHAIN_NAME="$CASE_ROOT/custom.keychain-db"
run_case
check 'override signing succeeds' test "$case_rc" = 0
check 'keychain override honored' contains "security <unlock-keychain> <$CASE_ROOT/custom.keychain-db>" "$CASE_ROOT/argv"

new_case peer_prelude
CASE_ACTION=prelude
export AGENTDESK_DEPLOY_FAST=1
run_case
check 'prelude succeeds' test "$case_rc" = 0
check 'peer keeps other forwarded settings' contains 'AGENTDESK_DEPLOY_FAST=1' "$CASE_ROOT/output"
check 'peer excludes host-local reference' not_contains 'AGENTDESK_CODESIGN_KEYCHAIN_OP_REF' "$CASE_ROOT/output"
check 'peer excludes retired password file' not_contains 'AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE' "$CASE_ROOT/output"

new_case detached_helper
CASE_ACTION=detached
run_case
check 'stub helper generation succeeds' test "$case_rc" = 0
helpers=( "$CASE_ROOT/release/runtime/self_hosted_deploy/deploy-release-stub-7775-"*.sh )
check 'one helper generated' test "${#helpers[@]}" = 1
check 'same-host helper preserves OP_REF' grep -Fq 'export AGENTDESK_CODESIGN_KEYCHAIN_OP_REF=op://AgentDesk/test-host/password' "${helpers[0]}"
check 'same-host helper omits retired password file' not_contains 'AGENTDESK_CODESIGN_KEYCHAIN_PW_FILE' "${helpers[0]}"

printf 'test_deploy_codesign_keychain_7775: cases=%s checks=%s failures=%s skip=0\n' "$cases" "$checks" "$failures"
[ "$failures" = 0 ]
