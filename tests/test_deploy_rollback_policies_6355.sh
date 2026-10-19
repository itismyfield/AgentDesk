#!/usr/bin/env bash
# Runs the deploy's real promotion, policy swap, health gate and EXIT-trap rollback on a
# temp release tree with launchd/health stubbed: live binary and policies stay one generation.

# shellcheck disable=SC2034  # Fixture globals are read by code loaded through eval.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
POLICY_HELPER="$(dirname "$DEPLOY_SH")/_deploy_policy_generation.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-rollback-policies-test.XXXXXX")
trap 'python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1])" "$TMP_ROOT"' EXIT

FAILURES=0
CHECKS=0
fail() { echo "  FAIL: $1" >&2; FAILURES=$((FAILURES + 1)); CHECKS=$((CHECKS + 1)); }
pass() { echo "  PASS: $1"; CHECKS=$((CHECKS + 1)); }
assert_eq() {
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: expected '$2', got '$3'"; fi
}
assert_absent() {
    if [ ! -e "$2" ]; then pass "$1"; else fail "$1: $2 still exists"; fi
}
assert_out() {
    if grep -Fq -- "$2" "$TMP_ROOT/out"; then pass "$1"; else fail "$1: '$2' missing from: $(cat "$TMP_ROOT/out")"; fi
}

# Heredoc braces do not end a function.
extract_function() {
    awk -v start="^${1}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && heredoc && /^PY$/ { heredoc = 0; next }
        printing && /<<.?PY.?$/ { heredoc = 1; next }
        printing && !heredoc && /^}$/ { exit }
    ' "$DEPLOY_SH"
}
# $1 first line (inclusive), $2 line that ends the segment (exclusive).
extract_segment() {
    awk -v start="$1" -v stop="$2" '
        $0 == start { printing = 1 }
        printing && $0 == stop { exit }
        printing { print }
    ' "$DEPLOY_SH"
}
require_code() {
    if [ -z "$2" ] || ! bash -n <<<"$2" 2>/dev/null; then
        fail "$1 cannot be extracted from $DEPLOY_SH"
        exit 1
    fi
}

for fn in _rollback_release_binary _signal_release_lock_pid _wait_release_stopped _cleanup_on_exit; do
    body="$(extract_function "$fn")"
    require_code "$fn" "$body"
    eval "$body"
done
# shellcheck disable=SC2016  # Literal source lines, matched verbatim.
PROMOTE_SEG="$(extract_segment 'REL_BINARY="$ADK_REL/bin/agentdesk"' \
    '# NOTE: the immutable re-lock (chflags uchg) is deferred until AFTER the health')"
require_code "promotion block" "$PROMOTE_SEG"
# Everything between the skills swap and the policies.dir self-heal is the policy swap.
POLICY_SWAP_SEG="$(awk '
    /^# #3288: self-heal policies[.]dir/ { exit }
    printing { print }
    after_skills && $0 == "rm -rf \"$ADK_REL/skills.old\"" { printing = 1 }
    $0 == "mv \"$SKILLS_STAGED\" \"$ADK_REL/skills\"" { after_skills = 1 }
' "$DEPLOY_SH")"
require_code "policy swap" "$POLICY_SWAP_SEG"
if ! grep -q POLICIES_STAGED <<<"$POLICY_SWAP_SEG"; then fail "policy swap segment holds no policy swap"; exit 1; fi
HEALTH_SEG="$(extract_segment 'REL_HEALTHY=false' \
    'if _health_json_unhealthy_only_no_provider_runtimes "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then')"
require_code "health gate and success path" "$HEALTH_SEG"
if [ -f "$POLICY_HELPER" ]; then
    # shellcheck source=/dev/null
    . "$POLICY_HELPER"
fi

export HOME="$TMP_ROOT/home"
ADK_REL="$TMP_ROOT/release"
REPO="$TMP_ROOT/src"
SOURCE_BINARY="$REPO/target/release/agentdesk"
PLIST_REL="com.agentdesk.test"
REL_PORT=18791
DEPLOY_HEALTH_RETRIES=1
DEPLOY_HEALTH_DELAY_SECS=1
ADK_DEFAULT_LOOPBACK=127.0.0.1
POST_DEPLOY_SMOKE_RELAY_CELL=""
POST_DEPLOY_SMOKE_CODEX_TURNS_CELL=""
case "$ADK_REL" in
    "$TMP_ROOT"/*) pass "fixture release root is inside the test temp dir" ;;
    *) fail "fixture release root escaped the temp dir: $ADK_REL"; exit 1 ;;
esac

_launchd_domain() { echo gui/501; }
_deploy_e2e_smoke_channel_ids() { echo ""; }
_rollback_would_brick_on_migration() { [ "${FIXTURE_MIGRATION_AHEAD:-0}" = 1 ]; }
_rollback_would_revert_o_writer() { return 1; }
_rollback_would_revert_o_ledger() { return 1; }
_cleanup_owned_pg_tunnel_preflight() { :; }
_rollback_pg_tunnel_migration() { :; }
_recover_or_preserve_past_migration_floor() { :; }
_preserve_staged_binary_for_recovery() { :; }
_emit_terminal_deploy_marker() { :; }
_finalize_detached_helper() { :; }
_health_json_unhealthy_only_no_provider_runtimes() { return 1; }
chflags() { return 0; }
xattr() { return 0; }
tmux() { return 0; }
start_release_tmux_fallback() { return 0; }
policy_gen() { cat "$ADK_REL/policies/generation" 2>/dev/null || echo none; }
launchctl() {
    case "$1" in
        print) return 1 ;;
        bootout) return 0 ;;
        bootstrap) echo "bin=$(cat "$ADK_REL/bin/agentdesk") policies=$(policy_gen)" >>"$TMP_ROOT/boots"; return 0 ;;
        *) echo "unexpected launchctl $*" >&2; return 99 ;;
    esac
}
# The binary of the failing generation never turns healthy; every other one does.
wait_for_http_service_health() {
    WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON=""
    [ "$(cat "$ADK_REL/bin/agentdesk")" != "bin-${FIXTURE_BAD_GEN:-none}" ]
}
# Fails only the rollback rename named by FIXTURE_FAIL_RESTORE (policy | binary).
mv() {
    case "${FIXTURE_FAIL_RESTORE:-}:$*" in
        "policy:$ADK_REL/policies.prev $ADK_REL/policies"|"binary:-f $ADK_REL/bin/agentdesk.prev $ADK_REL/bin/agentdesk")
            echo "mv: stub refused $*" >&2
            return 1 ;;
    esac
    command mv "$@"
}

# $1 live binary generation ("" = none), $2 live policy generation ("" = none).
setup_release() {
    python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1], ignore_errors=True)" "$ADK_REL"
    mkdir -p "$ADK_REL/bin" "$ADK_REL/runtime" "$HOME"
    [ -z "$1" ] || printf 'bin-%s\n' "$1" >"$ADK_REL/bin/agentdesk"
    if [ -n "$2" ]; then mkdir -p "$ADK_REL/policies"; printf '%s\n' "$2" >"$ADK_REL/policies/generation"; fi
}
# Runs one deploy of generation $1 through the real code; the EXIT trap fires as in production.
# $2 failing generation ("none" = healthy), $3 migration ahead of the backup, $4 rename to fail.
deploy_gen() {
    : >"$TMP_ROOT/boots"
    DEPLOY_RC=0
    FIXTURE_BAD_GEN="$2"
    FIXTURE_MIGRATION_AHEAD="${3:-0}"
    FIXTURE_FAIL_RESTORE="${4:-}"
    (
        ROLLBACK_ARMED=0
        DEPLOY_OK=0
        STAGED_BINARY="$ADK_REL/bin/agentdesk.staged"
        POLICIES_STAGED="$ADK_REL/policies.new"
        printf 'bin-%s\n' "$1" >"$STAGED_BINARY"
        mkdir -p "$POLICIES_STAGED"
        printf '%s\n' "$1" >"$POLICIES_STAGED/generation"
        trap _cleanup_on_exit EXIT
        eval "$PROMOTE_SEG"
        eval "$POLICY_SWAP_SEG"
        eval "$HEALTH_SEG"
    ) >"$TMP_ROOT/out" 2>&1 || DEPLOY_RC=$?
}
live_bin() { cat "$ADK_REL/bin/agentdesk" 2>/dev/null || echo none; }
assert_generation() {
    assert_eq "$1: live binary" "bin-$2" "$(live_bin)"
    assert_eq "$1: live policies" "$3" "$(policy_gen)"
}

echo "health failure after the policy swap rolls both back"
setup_release 1 1
deploy_gen 2 2
assert_eq "the failed deploy exits non-zero" 1 "$DEPLOY_RC"
assert_out "the rollback branch ran" "Rolling back release binary"
assert_generation "rolled back" 1 1
assert_eq "the old binary restarted on the old policies" "bin=bin-1 policies=1" "$(cat "$TMP_ROOT/boots")"
assert_absent "binary backup consumed" "$ADK_REL/bin/agentdesk.prev"
assert_absent "policy backup consumed" "$ADK_REL/policies.prev"
assert_absent "no transient policy dir left" "$ADK_REL/policies.old"
deploy_gen 3 none
assert_generation "a redeploy after the rollback" 3 3
assert_absent "policy backup dropped after the redeploy" "$ADK_REL/policies.prev"

echo "successful deploy keeps the new generation and drops both backups"
setup_release 1 1
deploy_gen 2 none
assert_eq "the healthy deploy exits zero" 0 "$DEPLOY_RC"
assert_generation "deployed" 2 2
assert_absent "binary backup dropped" "$ADK_REL/bin/agentdesk.prev"
assert_absent "policy backup dropped" "$ADK_REL/policies.prev"
assert_absent "no transient policy dir left" "$ADK_REL/policies.old"

echo "migration fail-forward keeps the new binary and the new policies"
setup_release 1 1
deploy_gen 2 2 1
assert_out "the fail-forward branch ran" "ROLLBACK REFUSED"
assert_generation "fail-forward" 2 2
assert_eq "nothing was restarted" "" "$(cat "$TMP_ROOT/boots")"
assert_eq "binary backup kept for manual use" bin-1 "$(cat "$ADK_REL/bin/agentdesk.prev")"
assert_eq "paired policy backup kept for manual use" 1 "$(cat "$ADK_REL/policies.prev/generation" 2>/dev/null || echo none)"

echo "a redeploy after fail-forward rolls back to the preserved pair"
deploy_gen 3 3
assert_out "the redeploy preserved the prior policy backup" "Preserving existing policy backup"
assert_generation "rolled back to the last-known-good pair" 1 1
assert_absent "policy backup consumed" "$ADK_REL/policies.prev"

echo "a successful redeploy after fail-forward clears the preserved pair"
setup_release 1 1
deploy_gen 2 2 1
deploy_gen 3 none
assert_generation "redeployed" 3 3
assert_absent "binary backup dropped" "$ADK_REL/bin/agentdesk.prev"
assert_absent "policy backup dropped" "$ADK_REL/policies.prev"

echo "a stale policy backup without its binary backup is never restored"
setup_release 1 1
mkdir -p "$ADK_REL/policies.prev"
printf '0\n' >"$ADK_REL/policies.prev/generation"
deploy_gen 2 2
assert_out "the stale backup was reported" "Discarding stale policy backup"
assert_generation "rolled back past the stale backup" 1 1
assert_absent "stale backup gone" "$ADK_REL/policies.prev"

echo "first deploy (no previous policies) says the generations diverge"
setup_release 1 ""
deploy_gen 2 2
assert_generation "binary rolled back, only policies remain" 1 2
assert_out "the missing policy generation is reported" "No previous policy generation to restore"

echo "a failed policy restore still restores the binary and says so"
setup_release 1 1
deploy_gen 2 2 0 policy
assert_out "the restore failure is reported" "Failed to restore previous policies"
assert_generation "binary restored, new policies put back" 1 2
assert_eq "previous policies kept for manual recovery" 1 "$(cat "$ADK_REL/policies.prev/generation" 2>/dev/null || echo none)"
assert_eq "the release was still restarted" "bin=bin-1 policies=2" "$(cat "$TMP_ROOT/boots")"

echo "a failed binary restore leaves both generations new"
setup_release 1 1
deploy_gen 2 2 0 binary
assert_out "the binary restore failure is reported" "Failed to restore previous binary"
assert_generation "nothing restored" 2 2
assert_eq "previous policies kept with the binary backup" 1 "$(cat "$ADK_REL/policies.prev/generation" 2>/dev/null || echo none)"

echo "a binary backup left without a paired policy backup is reported"
setup_release 1 1
printf 'bin-0\n' >"$ADK_REL/bin/agentdesk.prev"
deploy_gen 2 2
assert_out "the unpaired binary backup was reported" "no paired policy backup"
assert_generation "rolled back to the preserved binary and the live policies" 0 1

assert_absent "the real release root was never created under the fixture HOME" "$HOME/.adk"
if [ "$FAILURES" -ne 0 ]; then echo "$FAILURES of $CHECKS check(s) failed" >&2; exit 1; fi
echo "all $CHECKS rollback/policy generation checks passed"
