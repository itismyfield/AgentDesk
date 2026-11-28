#!/usr/bin/env bash
# Run the production deploy tail with fixture peers and smoke probes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY_SH="${DEPLOY_SH_OVERRIDE:-$ROOT/scripts/deploy-release.sh}"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/deploy-smoke-order.XXXXXX")"
trap 'rm -r "$TMP_ROOT"' EXIT

extract_function() {
    awk -v start="$1() {" '$0 == start {copy=1} copy {print} copy && /^}$/ {exit}' "$DEPLOY_SH"
}

for fn in _deploy_to_all_peers _finish_cluster_stage _post_deploy_smoke_note _run_post_deploy_functional_smoke; do
    body="$(extract_function "$fn")"
    test -n "$body"
    eval "$body"
done
awk '
    /^_report_post_deploy_smoke_failure\(\) \{$/ {report=1}
    tail {print}
    report && /^}$/ {tail=1}
' "$DEPLOY_SH" > "$TMP_ROOT/tail.sh"
test -s "$TMP_ROOT/tail.sh"

emit() { printf '%s\n' "$1" >> "$TRACE"; }
_resolve_deploy_peers() { printf '%s\n' "$PEERS"; }
_wait_for_peer_deploy_verdict() {
    emit "verdict:$1"
    [ "$1" != "$FAILED_PEER" ]
}
_deploy_to_one_peer() {
    emit "deploy:$1"
    _wait_for_peer_deploy_verdict "$1"
}
_post_deploy_smoke_wedge_reset() { :; }
_post_deploy_smoke_wait_for_startup_recovery() { emit recovery; }
_post_deploy_smoke_probe_apis() { emit api; export POST_DEPLOY_SMOKE_READY=true; }
_post_deploy_smoke_check_wedges() { emit wedge; }
_post_deploy_smoke_check_fail_closed_warn_rate() { emit warn-rate; }
_post_deploy_smoke_check_relay_round_trip() { emit E-1; }
_post_deploy_smoke_check_durable_record() { emit E-35; export POST_DEPLOY_SMOKE_DURABLE_COVERAGE=evaluated; }
_post_deploy_smoke_check_turn_scenarios() {
    emit E-50
    emit E-51
    export POST_DEPLOY_SMOKE_TURN_COVERAGE="E-50=pass E-51=pass"
    if [ "$TURN_RC" != 0 ]; then
        POST_DEPLOY_SMOKE_TURN_COVERAGE="E-50=FAIL E-51=pass"
        POST_DEPLOY_SMOKE_FAILURES+=("fixture turn failure")
    fi
    return "$TURN_RC"
}
_report_post_deploy_smoke_failure() { emit report; return 1; }
_write_release_source_manifest() { emit manifest; }
launchctl() { emit watchdog; return 1; }
bash() { test "$1" = "$SCRIPT_DIR/e2e/continuous/after_deploy.sh" && emit companion; }

run_case() (
    local name="$1" expected_rc="$2" expected_trace="$3"
    shift 3
    local case_root="$TMP_ROOT/$name" runner_rc=0
    mkdir -p "$case_root/home"
    export HOME="$case_root/home" TRACE="$case_root/trace"
    : > "$TRACE"
    export REPO="$ROOT" SCRIPT_DIR="$ROOT/scripts" ADK_REL="$case_root"
    export REL_PORT=1 DEPLOY_RUN_STARTED="$SECONDS" DEPLOY_OK=1
    export DEPLOY_ALL_NODES=1 DEPLOY_PEER_INVOCATION=0 SCHEMA_PEERS_FIRST_STATE="" SCHEMA_PEERS_FIRST_FAILED=0
    export POST_DEPLOY_SMOKE_SCOPE=full POST_DEPLOY_SMOKE_STAMP=fixture
    export POST_DEPLOY_SMOKE_EVIDENCE="$case_root/logs/post-deploy-smoke-fixture.log"
    export POST_DEPLOY_SMOKE_WEDGE_COVERAGE=clean POST_DEPLOY_SMOKE_WEDGE_CLEAN_COVERAGE=clean
    export POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE=evaluated
    export PEERS=$'one\ntwo' FAILED_PEER="" TURN_RC=0
    for setting in "$@"; do export "${setting?}"; done
    export -f emit _resolve_deploy_peers _wait_for_peer_deploy_verdict _deploy_to_one_peer \
        _deploy_to_all_peers _finish_cluster_stage _post_deploy_smoke_note _run_post_deploy_functional_smoke \
        _post_deploy_smoke_wedge_reset _post_deploy_smoke_wait_for_startup_recovery _post_deploy_smoke_probe_apis \
        _post_deploy_smoke_check_wedges _post_deploy_smoke_check_fail_closed_warn_rate \
        _post_deploy_smoke_check_relay_round_trip _post_deploy_smoke_check_durable_record \
        _post_deploy_smoke_check_turn_scenarios _report_post_deploy_smoke_failure \
        _write_release_source_manifest launchctl bash
    command bash -euo pipefail -c '
        tail_source="$1"
        shift
        record_rollback() {
            if [ "$DEPLOY_OK" != 1 ]; then emit rollback; fi
        }
        trap record_rollback EXIT
        if [ -n "$SCHEMA_PEERS_FIRST_STATE" ]; then emit verdict:early; fi
        . "$tail_source"
    ' deploy-tail "$TMP_ROOT/tail.sh" > "$case_root/output" 2>&1 || runner_rc=$?

    local trace
    trace="$(paste -sd ' ' "$TRACE")"
    printf '%s exit=%s trace=%s\n' "$name" "$runner_rc" "$trace"
    if [ "$runner_rc" != "$expected_rc" ] || [ "$trace" != "$expected_trace" ]; then
        printf 'FAIL %s expected exit=%s trace=%s\n' "$name" "$expected_rc" "$expected_trace" >&2
        cat "$case_root/output" >&2
        return 1
    fi
    if [ "$expected_rc" = 0 ]; then
        if [ "$(tail -n 1 "$case_root/output")" != "═══ Deploy Complete ═══" ]; then
            printf 'FAIL %s missing terminal success marker\n' "$name" >&2
            return 1
        fi
    else
        local skip='E-1=skipped E-35=skipped E-50=skipped E-51=skipped: peer deployment failed'
        if ! grep -Fq "turn scenarios: $skip" "$case_root/output" ||
           ! grep -Fq "$skip" "$case_root"/logs/post-deploy-smoke-*.log ||
           ! grep -Fq 'completed with coverage gap' "$case_root/output"; then
            printf 'FAIL %s missing peer failure skip reason in summary or evidence\n' "$name" >&2
            return 1
        fi
        grep -F 'turn scenarios:' "$case_root/output"
    fi
    printf 'PASS %s\n' "$name"
)

checks="recovery api wedge warn-rate"
turns="E-1 E-35 E-50 E-51"
success_order="watchdog manifest deploy:one verdict:one deploy:two verdict:two $checks"
early_order="$checks $turns companion watchdog manifest"
failed=0
run_case cluster_pass 0 "$success_order $turns companion" || failed=$((failed + 1))
run_case cluster_fail 1 "$success_order companion" FAILED_PEER=one || failed=$((failed + 1))
run_case smoke_fail_open 0 "$success_order $turns report companion" TURN_RC=1 || failed=$((failed + 1))
run_case single_node 0 "$early_order" DEPLOY_ALL_NODES=0 || failed=$((failed + 1))
run_case peer_invocation 0 "$early_order" DEPLOY_PEER_INVOCATION=1 || failed=$((failed + 1))
run_case schema_first_pass 0 "verdict:early $early_order" SCHEMA_PEERS_FIRST_STATE=completed || failed=$((failed + 1))
run_case schema_first_fail 1 "verdict:early $checks companion watchdog manifest" \
    SCHEMA_PEERS_FIRST_STATE=failed SCHEMA_PEERS_FIRST_FAILED=1 || failed=$((failed + 1))
printf 'RESULT passed=%s failed=%s\n' "$((7 - failed))" "$failed"
[ "$failed" = 0 ]
