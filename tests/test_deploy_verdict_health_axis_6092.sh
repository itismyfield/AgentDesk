#!/usr/bin/env bash
# #6092: a landed deploy must not be failed by degradation it cannot cause or clear.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"

FAILURES=0
pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; FAILURES=$((FAILURES + 1)); }

# $1 reasons JSON array, $2 fully_recovered, $3.. extra top-level JSON pairs
body() {
    local reasons="$1" recovered="$2"
    shift 2
    printf '{"db":true,"dashboard":true,"server_up":true,"status":"degraded",'
    printf '"ok":false,"fully_recovered":%s,"degraded_reasons":%s%s}' \
        "$recovered" "$reasons" "${1:+,$1}"
}

ready() { health_json_is_ready "$1" 1 1 1 1 >/dev/null 2>&1; }

expect_ready() {
    if ready "$2"; then pass "$1"; else fail "$1 — a landed deploy would be failed"; fi
}
expect_blocked() {
    if ready "$2"; then fail "$1 — a broken node would be reported as a good deploy"; else pass "$1"; fi
}

echo "§1 the measured incident: two relay reasons and a queue depth together"

INCIDENT='["relay_verdict_transport_unknown_claude_1479671298497183835","relay_verdict_unknown_codex_1479671301387059200","provider:codex:pending_queue_depth:4"]'
expect_ready "the exact 2026-09-22 payload verifies instead of polling to the deadline" \
    "$(body "$INCIDENT" true)"

echo "§2 membership, not homogeneity: mixing two accepted classes stays accepted"

for mix in \
    '["relay_verdict_unknown_codex_c1","provider:codex:pending_queue_depth:1"]' \
    '["relay_verdict_expired_claude_c1","relay_verdict_degraded_codex_c2"]' \
    '["relay_verdict_unknown_codex_c1","provider:codex:reconcile_in_progress"]' \
    '["provider:codex:pending_queue_depth:0","provider:claude:reconcile_in_progress"]'; do
    expect_ready "accepted mix: $mix" "$(body "$mix" true)"
done

echo "§3 reordering or combining accepted reasons never turns them into a refusal"

a='["relay_verdict_unknown_codex_c1","provider:codex:pending_queue_depth:4"]'
b='["provider:codex:pending_queue_depth:4","relay_verdict_unknown_codex_c1"]'
if ready "$(body "$a" true)" && ready "$(body "$b" true)"; then
    pass "reason order does not change the verdict"
else
    fail "reason order changes the verdict"
fi

echo "§4 any depth is a backlog, never evidence that a promotion failed"

for depth in 0 4 97 100000; do
    expect_ready "pending_queue_depth:$depth does not block a deploy" \
        "$(body "[\"provider:codex:pending_queue_depth:$depth\"]" true)"
done

echo "§5 a structural reason blocks even when mixed with accepted ones"

for blocker in db_unavailable doctor_missing latest_postgres_migration_missing \
    manifest_missing registry_unavailable repo_head_missing repo_dirty_missing \
    runtime_root_unavailable provider:codex:reconcile_stalled; do
    expect_blocked "$blocker blocks alongside an accepted reason" \
        "$(body "[\"relay_verdict_unknown_codex_c1\",\"$blocker\"]" true)"
done

echo "§6 an unrecognised reason fails closed, so a newly added one cannot slip through"

expect_blocked "an unknown reason blocks" \
    "$(body '["relay_verdict_unknown_codex_c1","a_reason_this_script_has_never_seen"]' true)"

echo "§7 fully_recovered=false no longer waves reasons through unread"

expect_blocked "a blocking reason is not rescued by fully_recovered=false" \
    "$(body '["provider:codex:reconcile_stalled"]' false)"
expect_blocked "an unknown reason is not rescued by fully_recovered=false" \
    "$(body '["a_reason_this_script_has_never_seen"]' false)"

echo "§8 reconcile is accepted only while the caller allows it"

RECONCILE="$(body '["provider:codex:reconcile_in_progress"]' true)"
if health_json_is_ready "$RECONCILE" 1 1 1 1 >/dev/null 2>&1; then
    pass "allow_reconcile_degraded=1 accepts reconcile_in_progress"
else
    fail "allow_reconcile_degraded=1 rejected reconcile_in_progress"
fi
if health_json_is_ready "$RECONCILE" 1 0 1 1 >/dev/null 2>&1; then
    fail "allow_reconcile_degraded=0 accepted reconcile_in_progress anyway"
else
    pass "allow_reconcile_degraded=0 still refuses it"
fi

echo "§9 class preconditions survive the move to one matcher"

expect_blocked "gateway_standby without cluster_standby is not a free pass" \
    "$(body '["gateway_standby"]' true)"
STANDBY='{"db":true,"dashboard":true,"server_up":true,"cluster_standby":true,"status":"degraded","ok":false,"degraded_reasons":["gateway_standby","provider:codex:gateway_standby"]}'
expect_ready "a settled standby peer still verifies" "$STANDBY"

echo "§9b standby tokens join the set only once the body proves it is a standby"

sb() { printf '{"db":true,"dashboard":true,"server_up":true,"cluster_standby":true,"status":"degraded","ok":false,"degraded_reasons":%s}' "$1"; }
expect_ready "standby mixes gateway with a relay verdict" \
    "$(sb '["gateway_standby","relay_verdict_unknown_codex_c1"]')"
expect_ready "standby mixes a provider gateway token with a queue depth" \
    "$(sb '["provider:codex:gateway_standby","provider:codex:pending_queue_depth:4"]')"
expect_blocked "the same mix without cluster_standby is refused" \
    "$(body '["gateway_standby","relay_verdict_unknown_codex_c1"]' true)"
expect_blocked "a standby claiming healthy is contradictory" \
    '{"db":true,"dashboard":true,"server_up":true,"cluster_standby":true,"status":"healthy","ok":true,"degraded_reasons":[]}'

echo "§9c a queue depth stops counting only for a deploy verdict"

QUEUE="$(body '["provider:codex:pending_queue_depth:4"]' true)"
if health_json_is_ready "$QUEUE" 1 1 1 1 >/dev/null 2>&1; then
    pass "the deploy verdict accepts a backlog"
else
    fail "the deploy verdict refused a backlog"
fi
if health_json_is_ready "$QUEUE" 1 1 1 >/dev/null 2>&1; then
    fail "a non-deploy caller accepted it — the deploy policy leaked into the shared predicate"
else
    pass "a non-deploy caller still refuses it"
fi
if health_json_is_ready "$(body '["relay_verdict_unknown_codex_c1"]' true)" 1 0 0 >/dev/null 2>&1; then
    pass "a relay verdict stays acceptable to every caller, as it was before"
else
    fail "a relay verdict became blocking for non-deploy callers"
fi

echo "§9d the other flags keep their meaning"

NO_DASH='{"db":true,"dashboard":false,"server_up":true,"status":"degraded","ok":false,"fully_recovered":true,"degraded_reasons":["relay_verdict_unknown_codex_c1"]}'
if health_json_is_ready "$NO_DASH" 1 1 1 1 >/dev/null 2>&1; then
    fail "require_dashboard=1 accepted a body with dashboard=false"
else
    pass "require_dashboard=1 still refuses dashboard=false"
fi
if health_json_is_ready "$NO_DASH" 0 1 1 1 >/dev/null 2>&1; then
    pass "require_dashboard=0 still ignores the dashboard"
else
    fail "require_dashboard=0 refused a body it used to accept"
fi
UNHEALTHY='{"ok":false,"status":"unhealthy","version":"x","db":true,"dashboard":true,"server_up":true,"fully_recovered":false,"cluster_standby":false,"degraded":true,"startup_status":"doctor_skipped","startup_degraded":false,"startup_degraded_reasons":[],"latest_startup_doctor":{"available":true,"doctor_status":"skipped","skipped":true,"skipped_reason":"no_provider_runtimes_registered"}}'
if health_json_is_ready "$UNHEALTHY" 1 1 1 1 >/dev/null 2>&1; then
    pass "allow_no_provider_runtimes=1 still rescues the unhealthy leader-only node"
else
    fail "allow_no_provider_runtimes=1 stopped rescuing it"
fi
if health_json_is_ready "$UNHEALTHY" 1 1 0 1 >/dev/null 2>&1; then
    fail "allow_no_provider_runtimes=0 rescued it anyway"
else
    pass "allow_no_provider_runtimes=0 still refuses it"
fi

echo "§10 the blocking reasons are named, so a timeout says what it waited on"

named=$(_health_json_deploy_blocking_reasons \
    "$(body '["relay_verdict_unknown_codex_c1","db_unavailable","provider:codex:pending_queue_depth:4"]' true)" \
    "$(_health_json_deploy_nonblocking_ere 1 1 0)")
if [ "$named" = "db_unavailable" ]; then
    pass "only the blocking reason is named ($named)"
else
    fail "expected 'db_unavailable', got '$named'"
fi

echo "§10b a list the matcher cannot read is blocking, not an absence of blockers"

for recovered in true false; do
    expect_blocked "an empty reason element blocks with fully_recovered=$recovered" \
        "$(body '["relay_verdict_unknown_codex_c1",""]' "$recovered")"
done
named=$(_health_json_deploy_blocking_reasons \
    "$(body '["relay_verdict_unknown_codex_c1",""]' false)" \
    "$(_health_json_deploy_nonblocking_ere 1 1 0)")
if [ "$named" = "unreadable_degraded_reasons" ]; then
    pass "an unreadable list is named as the blocker ($named)"
else
    fail "an unreadable list produced '$named', so fully_recovered=false would wave it through"
fi

echo "§10c the timeout diagnostic uses the policy the verdict used"

verdict_ere() {
    local b="$1" standby=0
    _health_json_field_is_true "$b" "cluster_standby" && standby=1
    _health_json_deploy_nonblocking_ere 1 1 "$standby"
}
accepted=$(_health_json_deploy_blocking_reasons "$QUEUE" "$(verdict_ere "$QUEUE")")
if [ -z "$accepted" ]; then
    pass "a backlog the health axis accepted is not named as deploy-blocking"
else
    fail "the diagnostic named '$accepted', which the verdict had accepted"
fi
STANDBY_BODY="$(sb '["provider:codex:gateway_standby"]')"
accepted=$(_health_json_deploy_blocking_reasons "$STANDBY_BODY" "$(verdict_ere "$STANDBY_BODY")")
if [ -z "$accepted" ]; then
    pass "a proven standby's gateway token is not named as deploy-blocking"
else
    fail "the diagnostic named '$accepted' on a standby the verdict accepted"
fi

echo "§10d the deploy script never rebuilds a partial policy at a call site"

# The readiness decision and the diagnostic must not derive the accepted set
# independently, or the diagnostic names causes the verdict just accepted.
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
partial=$(grep -nE '_health_json_deploy_nonblocking_ere( +"?[^ "]+"?){0,2} *\)' "$DEPLOY_SH" || true)
if [ -z "$partial" ]; then
    pass "every call passes all three authorizations"
else
    fail "a call site reconstructs a partial policy: $partial"
fi
if grep -q 'cluster_standby' <<<"$(grep -A3 -B3 '_health_json_deploy_nonblocking_ere' "$DEPLOY_SH")"; then
    pass "the diagnostic derives standby proof from the body it is explaining"
else
    fail "the diagnostic does not read cluster_standby, so a proven standby is named as blocking"
fi

echo "§11 the jq and no-jq paths agree on every shape above"

if ! command -v jq >/dev/null 2>&1; then
    fail "jq is absent, so this comparison would prove nothing"
else
    for shape in "$INCIDENT" '["relay_verdict_unknown_codex_c1","db_unavailable"]' \
        '["a_reason_this_script_has_never_seen"]' '["provider:codex:pending_queue_depth:4"]' \
        '["relay_verdict_unknown_codex_c1","provider:codex:reconcile_in_progress"]' \
        '["relay_verdict_unknown_codex_c1",""]' '["provider:a,b:pending_queue_depth:4"]'; do
        b="$(body "$shape" true)"
        with_jq=0; ready "$b" || with_jq=1
        # Force the fallback by making the detector answer no, not by breaking PATH.
        without_jq=0
        (
            _health_json_has_jq() { return 1; }
            health_json_is_ready "$b" 1 1 1 1 >/dev/null 2>&1
        ) || without_jq=1
        if [ "$with_jq" = "$without_jq" ]; then
            pass "jq and fallback agree on $shape (both $([ "$with_jq" = 0 ] && echo ready || echo blocked))"
        else
            fail "jq said $with_jq but the fallback said $without_jq for $shape"
        fi
    done
fi

if [ "$FAILURES" -gt 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all migration-independent verdict health-axis checks passed"
