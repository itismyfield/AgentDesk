#!/usr/bin/env bash
# Runs the deploy's real promotion, policy swap, health gate and EXIT-trap rollback on a
# temp release tree with launchd/health stubbed: live binary and policies stay one generation.

set -euo pipefail
unset AGENTDESK_CONFIG

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
POLICY_HELPER="$(dirname "$DEPLOY_SH")/_deploy_policy_generation.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-rollback-policies-test.XXXXXX")
trap 'python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1])" "$TMP_ROOT"' EXIT
CODE="$TMP_ROOT/code"
ADK_REL="$TMP_ROOT/release"
FIXTURE_HOME="$TMP_ROOT/home"
mkdir -p "$CODE" "$FIXTURE_HOME"

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

# Stubs for everything that would reach launchd, the network or the operator's config.
cat >"$CODE/lib.sh" <<'STUBS'
ADK_REL="$FIXTURE_ROOT/release"
export HOME="$FIXTURE_ROOT/home"
SCRIPT_DIR="$FIXTURE_ROOT/scripts"
REPO="$FIXTURE_ROOT/src"
SOURCE_BINARY="$REPO/target/release/agentdesk"
PLIST_REL="${FIXTURE_PLIST_REL:-com.agentdesk.test}"
REL_PORT=18791
DEPLOY_HEALTH_RETRIES=1
DEPLOY_HEALTH_DELAY_SECS=1
HEALTH_PHASE_STARTED="$SECONDS" ACTIVATION_PHASE_STARTED="$SECONDS"
ADK_DEFAULT_LOOPBACK=127.0.0.1
POST_DEPLOY_SMOKE_RELAY_CELL=""
POST_DEPLOY_SMOKE_CODEX_TURNS_CELL=""
_launchd_domain() { echo gui/501; }
_release_config_path() { printf '%s/config/agentdesk.yaml\n' "$ADK_REL"; }
_deploy_e2e_smoke_channel_ids() { echo ""; }
_rollback_would_brick_on_migration() { [ "${FIXTURE_MIGRATION_AHEAD:-0}" = 1 ]; }
_rollback_would_revert_o_writer() { return 1; }
_rollback_would_revert_o_spool() { return 1; }
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
launchctl() {
    case "$1" in
        print) return 1 ;;
        bootout) return 0 ;;
        bootstrap)
            echo "bin=$(cat "$ADK_REL/bin/agentdesk") policies=$(cat "$ADK_REL/policies/generation" 2>/dev/null || echo none)" \
                >>"$FIXTURE_ROOT/boots"
            return 0 ;;
        *) echo "unexpected launchctl $*" >&2; return 99 ;;
    esac
}
# Only the plist regeneration is intercepted; it fails when FIXTURE_FAIL_PLIST=1.
python3() {
    case "${1:-}" in
        */refresh_release_launchd_plist.py)
            if [ "${FIXTURE_FAIL_PLIST:-0}" = 1 ]; then echo "plist stub failed" >&2; return 1; fi
            return 0 ;;
    esac
    command python3 "$@"
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
# Fails only the success-path removal of the binary backup when FIXTURE_FAIL_BINARY_DROP=1.
rm() {
    if [ "${FIXTURE_FAIL_BINARY_DROP:-0}" = 1 ] && [ "$*" = "-f $ADK_REL/bin/agentdesk.prev" ]; then
        return 1
    fi
    command rm "$@"
}
STUBS
for fn in _rollback_release_binary _signal_release_lock_pid _wait_release_stopped _cleanup_on_exit; do
    body="$(extract_function "$fn")"
    require_code "$fn" "$body"
    printf '%s\n' "$body" >>"$CODE/lib.sh"
done
if [ -f "$POLICY_HELPER" ]; then
    printf '. %q\n' "$POLICY_HELPER" >>"$CODE/lib.sh"
fi
printf '. %q\n' "$REPO_ROOT/scripts/lib/deploy_allow_degraded.sh" >>"$CODE/lib.sh"
# Promotion through the plist regeneration; the dashboard and skills swaps are not modelled.
# shellcheck disable=SC2016  # Literal source lines, matched verbatim.
PROMOTE_SEG="$(extract_segment 'REL_BINARY="$ADK_REL/bin/agentdesk"' \
    '# Atomic swap: old → .old, staged → dist, cleanup')"
require_code "promotion block" "$PROMOTE_SEG"
# Whatever sits between the skills swap and the policies.dir self-heal still runs in order.
LATE_SEG="$(awk '
    /^# #3288: self-heal policies[.]dir/ { exit }
    printing { print }
    after_skills && $0 == "rm -rf \"$ADK_REL/skills.old\"" { printing = 1 }
    $0 == "mv \"$SKILLS_STAGED\" \"$ADK_REL/skills\"" { after_skills = 1 }
' "$DEPLOY_SH")"
bash -n <<<"$LATE_SEG" || { fail "post-skills block cannot be extracted"; exit 1; }
if ! grep -q POLICIES_STAGED <<<"$PROMOTE_SEG$LATE_SEG"; then fail "no policy swap found after promotion"; exit 1; fi
HEALTH_SEG="$(extract_segment 'REL_HEALTHY=false' \
    'if _health_json_unhealthy_only_no_provider_runtimes "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then')"
require_code "health gate and success path" "$HEALTH_SEG"
printf '%s\n' "$PROMOTE_SEG" >"$CODE/promote.sh"
printf '%s\n' "$LATE_SEG" >"$CODE/late.sh"
printf '%s\n' "$HEALTH_SEG" >"$CODE/health.sh"
# One deploy per independent /bin/bash with production errexit; the EXIT trap fires as in production.
cat >"$CODE/child.sh" <<'CHILD'
set -euo pipefail
. "$FIXTURE_CODE/lib.sh"
ROLLBACK_ARMED=0
DEPLOY_OK=0
STAGED_BINARY="$ADK_REL/bin/agentdesk.staged"
POLICIES_STAGED="$ADK_REL/policies.new"
printf 'bin-%s\n' "$1" >"$STAGED_BINARY"
mkdir -p "$POLICIES_STAGED"
printf '%s\n' "$1" >"$POLICIES_STAGED/generation"
trap _cleanup_on_exit EXIT
. "$FIXTURE_CODE/promote.sh"
. "$FIXTURE_CODE/late.sh"
. "$FIXTURE_CODE/health.sh"
CHILD

case "$ADK_REL" in
    "$TMP_ROOT"/*) pass "fixture release root is inside the test temp dir" ;;
    *) fail "fixture release root escaped the temp dir: $ADK_REL"; exit 1 ;;
esac

policy_gen() { cat "$ADK_REL/policies/generation" 2>/dev/null || echo none; }
live_bin() { cat "$ADK_REL/bin/agentdesk" 2>/dev/null || echo none; }
prev_policy_gen() { cat "$ADK_REL/policies.prev/generation" 2>/dev/null || echo none; }
# $1 live binary generation ("" = none), $2 live policy generation ("" = none).
setup_release() {
    python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1], ignore_errors=True)" "$ADK_REL"
    mkdir -p "$ADK_REL/bin" "$ADK_REL/runtime"
    [ -z "$1" ] || printf 'bin-%s\n' "$1" >"$ADK_REL/bin/agentdesk"
    if [ -n "$2" ]; then mkdir -p "$ADK_REL/policies"; printf '%s\n' "$2" >"$ADK_REL/policies/generation"; fi
}
# $1 deployed generation, $2 failing generation ("none" = healthy), then FIXTURE_*=value switches.
deploy_gen() {
    local gen="$1" bad="$2"
    shift 2
    : >"$TMP_ROOT/boots"
    DEPLOY_RC=0
    env FIXTURE_ROOT="$TMP_ROOT" FIXTURE_CODE="$CODE" FIXTURE_BAD_GEN="$bad" "$@" \
        /bin/bash "$CODE/child.sh" "$gen" >"$TMP_ROOT/out" 2>&1 || DEPLOY_RC=$?
}
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
deploy_gen 2 2 FIXTURE_MIGRATION_AHEAD=1
assert_out "the fail-forward branch ran" "ROLLBACK REFUSED"
assert_generation "fail-forward" 2 2
assert_eq "nothing was restarted" "" "$(cat "$TMP_ROOT/boots")"
assert_eq "binary backup kept for manual use" bin-1 "$(cat "$ADK_REL/bin/agentdesk.prev")"
assert_eq "paired policy backup kept for manual use" 1 "$(prev_policy_gen)"

echo "a redeploy after fail-forward rolls back to the preserved pair"
deploy_gen 3 3
assert_out "the redeploy preserved the prior policy backup" "Preserving existing policy backup"
assert_generation "rolled back to the last-known-good pair" 1 1
assert_absent "policy backup consumed" "$ADK_REL/policies.prev"

echo "a successful redeploy after fail-forward clears the preserved pair"
setup_release 1 1
deploy_gen 2 2 FIXTURE_MIGRATION_AHEAD=1
deploy_gen 3 none
assert_generation "redeployed" 3 3
assert_absent "binary backup dropped" "$ADK_REL/bin/agentdesk.prev"
assert_absent "policy backup dropped" "$ADK_REL/policies.prev"

echo "a plist failure right after promotion under migration fail-forward keeps both new"
setup_release 1 1
deploy_gen 2 none FIXTURE_MIGRATION_AHEAD=1 FIXTURE_PLIST_REL=com.agentdesk.release FIXTURE_FAIL_PLIST=1
assert_eq "the plist failure aborts the deploy" 1 "$DEPLOY_RC"
assert_out "the injected plist failure ran" "plist stub failed"
assert_out "the fail-forward branch ran" "ROLLBACK REFUSED"
assert_generation "fail-forward after a plist failure" 2 2
assert_eq "binary backup kept" bin-1 "$(cat "$ADK_REL/bin/agentdesk.prev" 2>/dev/null || echo none)"
assert_eq "paired policy backup kept" 1 "$(prev_policy_gen)"
assert_absent "staged policies were consumed, not discarded" "$ADK_REL/policies.new"

echo "a plist failure right after promotion rolls both back"
setup_release 1 1
deploy_gen 2 none FIXTURE_PLIST_REL=com.agentdesk.release FIXTURE_FAIL_PLIST=1
assert_eq "the plist failure aborts the deploy" 1 "$DEPLOY_RC"
assert_out "the rollback branch ran" "Rolling back release binary"
assert_generation "rolled back after a plist failure" 1 1
assert_absent "policy backup consumed" "$ADK_REL/policies.prev"

echo "a binary backup that survives the success path keeps its paired policy backup"
setup_release 1 1
deploy_gen 2 none FIXTURE_FAIL_BINARY_DROP=1
assert_eq "the healthy deploy exits zero" 0 "$DEPLOY_RC"
assert_generation "deployed" 2 2
assert_out "the kept pair is reported" "keeping its paired policy backup"
assert_eq "paired policy backup kept" 1 "$(prev_policy_gen)"
deploy_gen 3 3
assert_generation "a later failed deploy rolls back to the kept pair" 1 1

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
deploy_gen 2 2 FIXTURE_FAIL_RESTORE=policy
assert_out "the restore failure is reported" "Failed to restore previous policies"
assert_generation "binary restored, new policies put back" 1 2
assert_eq "previous policies kept for manual recovery" 1 "$(prev_policy_gen)"
assert_eq "the release was still restarted" "bin=bin-1 policies=2" "$(cat "$TMP_ROOT/boots")"

echo "a failed binary restore leaves both generations new"
setup_release 1 1
deploy_gen 2 2 FIXTURE_FAIL_RESTORE=binary
assert_out "the binary restore failure is reported" "Failed to restore previous binary"
assert_generation "nothing restored" 2 2
assert_eq "previous policies kept with the binary backup" 1 "$(prev_policy_gen)"

echo "a binary backup left without a paired policy backup is reported"
setup_release 1 1
printf 'bin-0\n' >"$ADK_REL/bin/agentdesk.prev"
deploy_gen 2 2
assert_out "the unpaired binary backup was reported" "no paired policy backup"
assert_generation "rolled back to the preserved binary and the live policies" 0 1

assert_absent "the real release root was never created under the fixture HOME" "$FIXTURE_HOME/.adk"
if [ "$FAILURES" -ne 0 ]; then echo "$FAILURES of $CHECKS check(s) failed" >&2; exit 1; fi
echo "all $CHECKS rollback/policy generation checks passed"
