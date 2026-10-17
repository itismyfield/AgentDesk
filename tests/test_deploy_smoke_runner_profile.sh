#!/usr/bin/env bash
# A runner profile (/api/health modules.admin_api=false, modules.gateway=false)
# mounts only the access and runtime API routers and runs no relay, so the
# post-deploy smoke must not count those absences as failures. Every other
# shape of the health body keeps the full-node checks (fail-closed).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="${DEPLOY_SH_OVERRIDE:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-smoke-runner-profile.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

extract_function() {
    awk -v start="^${1}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}
extract_array() {
    awk -v start="^${1}=[(]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^[)]$/ { exit }
    ' "$DEPLOY_SH"
}

# Load the production smoke functions and endpoint lists without deploying.
for fn in _post_deploy_smoke_note _post_deploy_smoke_fail \
    _post_deploy_smoke_optional_accounts_absent _post_deploy_smoke_resolve_health_module \
    _post_deploy_smoke_profile_unmounted _post_deploy_smoke_probe_apis \
    _post_deploy_smoke_resolve_cluster_standby _post_deploy_smoke_check_relay_round_trip; do
    eval "$(extract_function "$fn")"
done
eval "$(extract_array POST_DEPLOY_SMOKE_CORE_API_ENDPOINTS)"
eval "$(extract_array POST_DEPLOY_SMOKE_ADMIN_API_ENDPOINTS)"

POST_DEPLOY_SMOKE_EVIDENCE="$TMP_ROOT/evidence.log"
ADK_REL="$TMP_ROOT/release"
REPO="$REPO_ROOT"
POST_DEPLOY_SMOKE_TMP_DIR="$TMP_ROOT/tmp"
POST_DEPLOY_SMOKE_RELAY_CELL="claude/adk-cc"
POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID=""
ADK_DEFAULT_LOOPBACK=127.0.0.1
REL_PORT=1
POST_DEPLOY_SMOKE_READY=true
POST_DEPLOY_SMOKE_HEALTH_BODY=""
export POST_DEPLOY_SMOKE_EVIDENCE ADK_REL REPO POST_DEPLOY_SMOKE_TMP_DIR
export POST_DEPLOY_SMOKE_RELAY_CELL POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID
export ADK_DEFAULT_LOOPBACK REL_PORT POST_DEPLOY_SMOKE_READY POST_DEPLOY_SMOKE_HEALTH_BODY
mkdir -p "$POST_DEPLOY_SMOKE_TMP_DIR"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"

RUNNER_MODULES='{"gateway":false,"voice":false,"dashboard":false,"admin_api":false,"leader_services":false}'
FULL_MODULES='{"gateway":true,"voice":true,"dashboard":true,"admin_api":true,"leader_services":true}'
HEALTH_JSON=""
HEALTH_STATUS=200
ADMIN_STATUS=404
SESSIONS_STATUS=200
# Fake transport: /api/health serves HEALTH_JSON, admin-router paths ADMIN_STATUS.
curl() {
    local out="" url="" status=200 body='{}'
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -o) out="$2"; shift 2; continue ;;
            http://*) url="$1" ;;
        esac
        shift
    done
    case "${url#http://"${ADK_DEFAULT_LOOPBACK}:${REL_PORT}"}" in
        /api/health) status="$HEALTH_STATUS"; body="$HEALTH_JSON" ;;
        /api/sessions) status="$SESSIONS_STATUS" ;;
        /api/agents | /api/claude-accounts | /api/docs) status="$ADMIN_STATUS" ;;
    esac
    printf '%s\n' "$body" > "$out"
    printf '%s' "$status"
}
# Records any attempt to read the E2E cell config; the relay checks resolve it via python3.
python3() {
    printf 'python3 %s\n' "$*" >> "$TMP_ROOT/python3.calls"
    return 1
}

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

probe_rc=0
probe_out="$TMP_ROOT/probe.out"
run_probe() {
    HEALTH_JSON="$1"
    ADMIN_STATUS="$2"
    SESSIONS_STATUS="${3:-200}"
    POST_DEPLOY_SMOKE_FAILURES=()
    probe_rc=0
    _post_deploy_smoke_probe_apis > "$probe_out" 2>&1 || probe_rc=$?
}
expect_admin_failures() {
    local label="$1" code="$2" path
    [ "$probe_rc" -ne 0 ] || fail_test "$label: the probe must fail"
    for path in /api/agents /api/claude-accounts /api/docs; do
        grep -qF "FAIL: core API ${path}: expected HTTP 200, got ${code}" "$probe_out" \
            || fail_test "$label: ${path} ${code} must stay a failure"
    done
    if grep -q 'skipped=profile' "$probe_out"; then
        fail_test "$label: no admin path may be skipped as a profile absence"
    fi
}

# (a) runner body: the unmounted admin routers answer 404 and are noted, not failed.
run_probe "{\"cluster_standby\":false,\"modules\":${RUNNER_MODULES}}" 404
[ "$probe_rc" -eq 0 ] || fail_test "(a) runner admin 404s must not fail the probe (rc=$probe_rc)"
[ "${#POST_DEPLOY_SMOKE_FAILURES[@]}" -eq 0 ] || fail_test "(a) runner must record no failures, got ${#POST_DEPLOY_SMOKE_FAILURES[@]}"
for path in /api/agents /api/claude-accounts /api/docs; do
    grep -qxF "api endpoint=${path} skipped=profile(admin_api=false)" "$probe_out" \
        || fail_test "(a) runner must note the profile skip for ${path}"
done
[ "$(grep -c 'skipped=profile' "$probe_out")" -eq 3 ] || fail_test "(a) exactly three admin paths are profile skips"

# (b) full body: an admin 404 is a real regression.
run_probe "{\"cluster_standby\":false,\"modules\":${FULL_MODULES}}" 404
expect_admin_failures "(b) admin_api=true" 404
[ "${#POST_DEPLOY_SMOKE_FAILURES[@]}" -eq 3 ] || fail_test "(b) full node must record three failures"

# (c) fail-closed shapes of the health body keep the 200 requirement.
run_probe '{"cluster_standby":false}' 404
expect_admin_failures "(c) modules missing" 404
run_probe '{"cluster_standby":false,"modules":["admin_api"]}' 404
expect_admin_failures "(c) modules not an object" 404
run_probe '{"cluster_standby":false,"modules":{"gateway":false}}' 404
expect_admin_failures "(c) admin_api missing" 404
run_probe 'not json' 404
expect_admin_failures "(c) health body not JSON" 404
HEALTH_STATUS=503
run_probe "{\"cluster_standby\":false,\"modules\":${RUNNER_MODULES}}" 404
HEALTH_STATUS=200
expect_admin_failures "(c) health body unavailable" 404

# (d) a non-boolean admin_api is unreadable, not off.
run_probe '{"cluster_standby":false,"modules":{"admin_api":"false"}}' 404
expect_admin_failures "(d) admin_api string" 404
run_probe '{"cluster_standby":false,"modules":{"admin_api":null}}' 404
expect_admin_failures "(d) admin_api null" 404

# (e) only a 404 is a profile absence: a runner admin path answering 500 still fails.
run_probe "{\"cluster_standby\":false,\"modules\":${RUNNER_MODULES}}" 500
expect_admin_failures "(e) runner admin 500" 500

# A runtime-router path stays required on a runner.
run_probe "{\"cluster_standby\":false,\"modules\":${RUNNER_MODULES}}" 404 404
grep -qF 'FAIL: core API /api/sessions: expected HTTP 200, got 404' "$probe_out" \
    || fail_test "a runner /api/sessions 404 must stay a failure"

health_path="$TMP_ROOT/health.json"
relay_out="$TMP_ROOT/relay.out"
run_relay() {
    printf '%s\n' "$1" > "$health_path"
    POST_DEPLOY_SMOKE_HEALTH_BODY="$health_path"
    POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID=""
    : > "$TMP_ROOT/python3.calls"
    relay_rc=0
    # The runner case runs in this shell so the channel E-1 leaves behind is visible.
    if [ "${2:-}" = same-shell ]; then
        _post_deploy_smoke_check_relay_round_trip > "$relay_out" 2>&1 || relay_rc=$?
    else
        ( _post_deploy_smoke_check_relay_round_trip ) > "$relay_out" 2>&1 || relay_rc=$?
    fi
}
expect_round_trip() {
    grep -q 'relay E-1=round-trip proceeding cluster_standby=false' "$relay_out" \
        || fail_test "$1: the round-trip must proceed"
    grep -q 'modules.gateway=false' "$relay_out" && fail_test "$1: must not skip as a runner"
    [ -s "$TMP_ROOT/python3.calls" ] || fail_test "$1: the round-trip must reach cell resolution"
}

# (f) runner: the relay belongs to the gateway, so E-1 skips before any config or yaml read.
run_relay "{\"cluster_standby\":false,\"modules\":${RUNNER_MODULES}}" same-shell
[ "$relay_rc" -eq 0 ] || fail_test "(f) runner E-1 must return 0, got $relay_rc"
grep -qxF 'relay E-1=skipped modules.gateway=false (runner: relay belongs to the gateway)' "$relay_out" \
    || fail_test "(f) runner must note the gateway skip"
grep -q 'round-trip proceeding\|FAIL:' "$relay_out" && fail_test "(f) runner must neither proceed nor fail"
[ -s "$TMP_ROOT/python3.calls" ] && fail_test "(f) runner must not read the E2E cell config"
[ -z "$POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID" ] || fail_test "(f) runner must leave no relay channel for E-35/E-50/E-51"

# (g) full node keeps the round-trip.
run_relay "{\"cluster_standby\":false,\"modules\":${FULL_MODULES}}"
expect_round_trip "(g) gateway=true"

# (h) fail-closed shapes keep the round-trip.
run_relay '{"cluster_standby":false,"modules":{"admin_api":false}}'
expect_round_trip "(h) gateway missing"
run_relay '{"cluster_standby":false}'
expect_round_trip "(h) modules missing"
run_relay '{"cluster_standby":false,"modules":{"gateway":"false"}}'
expect_round_trip "(h) gateway string"
run_relay '{"cluster_standby":false,"modules":"runner"}'
expect_round_trip "(h) modules not an object"

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_deploy_smoke_runner_profile: $failures assertion(s) failed" >&2
    exit 1
fi
printf '%s\n' "test_deploy_smoke_runner_profile: all assertions passed"
