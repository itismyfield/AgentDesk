#!/usr/bin/env bash
set -euo pipefail

# #5244 is a report-coverage test. It extracts only the sentinel region and
# never sources deploy-release.sh (which would execute a real deployment).
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY_SH="${DEPLOY_SH_OVERRIDE:-$ROOT_DIR/scripts/deploy-release.sh}"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-wedge-coverage-5244.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

failures=0
fail() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

begin_line=$(grep -nF '# >>> BEGIN wedge-check region (#5244)' "$DEPLOY_SH" | cut -d: -f1 || true)
end_line=$(grep -nF '# <<< END wedge-check region (#5244)' "$DEPLOY_SH" | cut -d: -f1 || true)
[ -n "$begin_line" ] || fail 'wedge-check BEGIN sentinel missing'
[ -n "$end_line" ] || fail 'wedge-check END sentinel missing'
if [ -n "$begin_line" ] && [ -n "$end_line" ]; then
    [ "$begin_line" -lt "$end_line" ] || fail 'wedge-check sentinels are reversed'
fi

REGION="$TMP_ROOT/wedge-region.sh"
if [ -n "$begin_line" ] && [ -n "$end_line" ]; then
    sed -n "$((begin_line + 1)),$((end_line - 1))p" "$DEPLOY_SH" > "$REGION"
fi

# Boundary declaration: this suite fixes behavior but cannot protect itself
# from harness bypass; seven measured forms include multiline/eval functions,
# trap handler/signal variants, and eval/compound source hooks. CASE_DONE is
# not an authenticated return witness (a forged token plus exit 0 passes).
# Code review and CI's required-suite existence check own harness integrity.

runner_source="$TMP_ROOT/runner.sh"
sed -n '/^_run_post_deploy_functional_smoke()/,/^_report_post_deploy_smoke_failure()/p' "$DEPLOY_SH" \
    | sed '$d' > "$runner_source"
DISPOSITION="$TMP_ROOT/disposition.sh"
disposition_begin=$(grep -nF '# >>> BEGIN smoke-disposition region (#5244)' "$DEPLOY_SH" | cut -d: -f1 || true)
disposition_end=$(grep -nF '# <<< END smoke-disposition region (#5244)' "$DEPLOY_SH" | cut -d: -f1 || true)
sed -n "$((disposition_begin + 1)),$((disposition_end - 1))p" "$DEPLOY_SH" > "$DISPOSITION"
durable_clean_coverage=$(sed -n 's/^POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE="\([^"]*\)"$/\1/p' "$DEPLOY_SH" | head -1 || true)
[ -n "$durable_clean_coverage" ] || fail 'durable clean coverage declaration is missing'

runner_output=$(DURABLE_CLEAN_COVERAGE="$durable_clean_coverage" bash -s -- "$runner_source" "$TMP_ROOT" "$DISPOSITION" <<'CHILD'
set -euo pipefail
runner_source="$1"; root="$2"; disposition_source="$3"; eval "$(<"$runner_source")"
ADK_REL="$root"; POST_DEPLOY_SMOKE_EVIDENCE="$root/runner.evidence"; POST_DEPLOY_SMOKE_TMP_DIR=""; POST_DEPLOY_SMOKE_FAILURES=(); POST_DEPLOY_SMOKE_STAMP=runner; REL_PORT=0; runner_wedge_called=0
POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID=""; POST_DEPLOY_SMOKE_WEDGE_COVERAGE="clean-sentinel"; POST_DEPLOY_SMOKE_WEDGE_CLEAN_COVERAGE="clean-sentinel"; POST_DEPLOY_SMOKE_DURABLE_COVERAGE="unevaluable: E-35 did not run"; POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE="$DURABLE_CLEAN_COVERAGE"
_post_deploy_smoke_note() { :; }; _post_deploy_smoke_probe_apis() { return 0; }; _post_deploy_smoke_check_wedges() { runner_wedge_called=1; return 0; }; _post_deploy_smoke_check_fail_closed_warn_rate() { return 0; }
# E-1 rc-0 skip fixture: leave the channel unset, so the durable probe returns 0.
_post_deploy_smoke_check_relay_round_trip() { POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID=""; return 0; }
_post_deploy_smoke_check_durable_record() { [ -z "$POST_DEPLOY_SMOKE_RELAY_CHANNEL_ID" ] || return 1; return 0; }
if _run_post_deploy_functional_smoke; then rc=0; else rc=$?; fi
eval "$(<"$disposition_source")"
printf 'CASE_DONE runner rc=%s wedge_called=%s\n' "$rc" "$runner_wedge_called"
CHILD
)
grep -q 'CASE_DONE runner rc=0 wedge_called=1' <<< "$runner_output" || fail 'smoke runner did not execute check_wedges in its parent shell'
grep -q '△ Post-deploy functional smoke completed with coverage gap' <<< "$runner_output" || fail 'E-1 skip did not produce a coverage gap'
grep -q 'durable record coverage: unevaluable: E-35 did not run' <<< "$runner_output" || fail 'E-1 skip omitted durable coverage'
! grep -q '✓ Post-deploy functional smoke passed' <<< "$runner_output" || fail 'E-1 skip produced a false pass'

cleanup_to_signal=$(sed -n '/^_cleanup_on_exit()/,/^_handle_cleanup_signal()/p' "$DEPLOY_SH")
grep -q '_finalize_detached_helper' <<< "$cleanup_to_signal" || fail 'cleanup text does not contain finalizer'
coverage_decl_line=$(grep -nE '^POST_DEPLOY_SMOKE_WEDGE_COVERAGE=' "$DEPLOY_SH" | head -1 | cut -d: -f1 || true)
trap_line=$(grep -nF 'trap _cleanup_on_exit EXIT' "$DEPLOY_SH" | head -1 | cut -d: -f1 || true)
[ -n "$coverage_decl_line" ] && [ -n "$trap_line" ] && [ "$coverage_decl_line" -lt "$trap_line" ] || fail 'coverage declaration is not before EXIT trap'
while IFS=: read -r assignment_line _; do
    [ -n "$assignment_line" ] || continue
    if [ "$assignment_line" != "$coverage_decl_line" ]; then
        [ "$assignment_line" -ge "$begin_line" ] && [ "$assignment_line" -le "$end_line" ] || fail "coverage assignment escaped region at line $assignment_line"
    fi
done < <(grep -nE '^[[:space:]]*POST_DEPLOY_SMOKE_WEDGE_COVERAGE=' "$DEPLOY_SH" || true)

scanner_text=$(sed -n '/^_post_deploy_smoke_wedge_scan_from_file()/,/^_post_deploy_smoke_wedge_unevaluable()/p' "$DEPLOY_SH")
check_text=$(sed -n '/^_post_deploy_smoke_check_wedges()/,/^# <<< END wedge-check region/p' "$DEPLOY_SH")
grep -qF 'count=\($markers | length)' <<< "$scanner_text" || fail 'count is not produced by jq array length'
grep -q 'queue_blocked' <<< "$scanner_text" || fail 'queue_blocked observation was removed'
grep -q 'unknown_stall_state' <<< "$scanner_text" || fail 'unknown stall observation was removed'
for state in tmux_alive_relay_dead stale_thread_proof orphan_pending_token; do
    grep -q "$state" <<< "$scanner_text" || fail "marker state $state is not in the classifier filter"
done
grep -q 'relay_stall_state.*type' <<< "$scanner_text" || fail 'relay_stall_state type guard is missing'
! grep -q 'degraded_reasons' <<< "$scanner_text" || fail 'degraded_reasons became a second authority'
! grep -qE 'desynced|watcher_attached_stale|relay_owner_kind' <<< "$scanner_text" || fail 'input booleans were re-promoted'
! grep -qE '\b(sleep|curl|comm|sort)\b' <<< "$check_text" || fail 'deleted transient machinery remains in check_wedges'
! grep -qE 'marker_count.*(\$\(\(|-gt|-ge|-lt|-le)' <<< "$check_text" || fail 'marker count uses shell arithmetic'
grep -qF 'POST_DEPLOY_SMOKE_STAMP="$(date' "$DEPLOY_SH" || fail 'stamp assignment disappeared'
grep -qF ')-$$"' "$DEPLOY_SH" || fail 'stamp has no PID suffix'
for old_name in consecutive_skips WEDGE_SETTLE RECOVERY_WAIT; do
    ! grep -q "$old_name" "$DEPLOY_SH" || fail "deleted persistent/wait mechanism remains: $old_name"
done

fixture() {
    local name="$1" body="$2"
    printf '%s\n' "$body" > "$TMP_ROOT/$name.json"
}
fixture clean '{"fully_recovered":true,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"healthy"}]}'
fixture active_stream '{"fully_recovered":true,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"active_foreground_stream","relay_health":{"desynced":true,"stale_thread_proof":true,"watcher_attached_stale":true}}]}'
fixture recovering '{"fully_recovered":false,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"healthy"}]}'
fixture tmux_dead '{"fully_recovered":true,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"tmux_alive_relay_dead"}]}'
fixture stale_thread '{"fully_recovered":true,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"stale_thread_proof"}]}'
fixture orphan_token '{"fully_recovered":true,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"orphan_pending_token"}]}'
fixture observations '{"fully_recovered":true,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"queue_blocked"},{"provider":"claude","channel_id":2,"relay_stall_state":"future_state"},{"provider":"claude","channel_id":3,"relay_stall_state":"unpaired_active_token"}]}'
fixture recovering_observations '{"fully_recovered":false,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":"tmux_alive_relay_dead"},{"provider":"claude","channel_id":2,"relay_stall_state":"queue_blocked"},{"provider":"claude","channel_id":3,"relay_stall_state":"future_state"}]}'
fixture malformed '{"fully_recovered":true,"mailboxes":[{"provider":"claude","channel_id":1,"relay_stall_state":42}]}'
fixture malformed_json '{broken'
fixture empty_mailboxes '{"fully_recovered":true,"mailboxes":null}'
fixture empty_mailboxes_valid '{"fully_recovered":true,"mailboxes":[]}'
fixture root_scalar '[]'
fixture recovery_missing '{"mailboxes":[]}'
fixture mailbox_scalar '{"fully_recovered":true,"mailboxes":["mailbox"]}'
fixture mailbox_missing_state '{"fully_recovered":true,"mailboxes":[{"provider":"claude"}]}'
fixture recovery_string '{"fully_recovered":"yes","mailboxes":[]}'

WEDGE_MARKER_COVERAGE='evaluated: %s stall-state marker(s) observed (point-in-time)'
WEDGE_CLEAN_COVERAGE="${WEDGE_MARKER_COVERAGE/\%s/0}"

run_case() {
    local name="$1" expected_rc="$2" fixture_path="$3" expected_coverage="$4"
    local output rc case_line coverage_line
    output=$(bash -s -- "$REGION" "$fixture_path" <<'CHILD'
set -euo pipefail
region_path="$1"
body_path="$2"
eval "$(<"$region_path")"
POST_DEPLOY_SMOKE_EVIDENCE="${body_path}.evidence"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$body_path"
POST_DEPLOY_SMOKE_FAILURES=()
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
if _post_deploy_smoke_check_wedges; then
    rc=0
else
    rc=$?
fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
    )
    case_line=$(grep '^CASE_DONE ' <<< "$output" || true)
    [ -n "$case_line" ] || { fail "$name: completion token missing"; return; }
    rc=${case_line#*rc=}
    rc=${rc%% *}
    coverage_line=${case_line#*coverage=}
    [ "$rc" -eq "$expected_rc" ] || fail "$name: expected rc $expected_rc, got $rc"
    [ "$coverage_line" = "$expected_coverage" ] || fail "$name: expected coverage [$expected_coverage], got [$coverage_line]"
}

run_case clean 0 "$TMP_ROOT/clean.json" "$WEDGE_CLEAN_COVERAGE"
run_case active_stream 0 "$TMP_ROOT/active_stream.json" "$WEDGE_CLEAN_COVERAGE"
run_case recovering 0 "$TMP_ROOT/recovering.json" 'not evaluated: startup recovery in progress'
run_case tmux_alive_relay_dead 1 "$TMP_ROOT/tmux_dead.json" "${WEDGE_MARKER_COVERAGE/\%s/1}"
run_case stale_thread_proof 1 "$TMP_ROOT/stale_thread.json" "${WEDGE_MARKER_COVERAGE/\%s/1}"
run_case orphan_pending_token 1 "$TMP_ROOT/orphan_token.json" "${WEDGE_MARKER_COVERAGE/\%s/1}"
run_case observations 0 "$TMP_ROOT/observations.json" "$WEDGE_CLEAN_COVERAGE"
run_case recovering_observations 0 "$TMP_ROOT/recovering_observations.json" 'not evaluated: startup recovery in progress'
run_case malformed 1 "$TMP_ROOT/malformed.json" 'unevaluable: health/detail scan failed'
run_case malformed_json 1 "$TMP_ROOT/malformed_json.json" 'unevaluable: health/detail scan failed'
run_case empty_mailboxes 1 "$TMP_ROOT/empty_mailboxes.json" 'unevaluable: health/detail scan failed'
run_case empty_mailboxes_valid 0 "$TMP_ROOT/empty_mailboxes_valid.json" "$WEDGE_CLEAN_COVERAGE"
run_case root_scalar 1 "$TMP_ROOT/root_scalar.json" 'unevaluable: health/detail scan failed'
run_case recovery_missing 1 "$TMP_ROOT/recovery_missing.json" 'unevaluable: health/detail scan failed'
run_case mailbox_scalar 1 "$TMP_ROOT/mailbox_scalar.json" 'unevaluable: health/detail scan failed'
run_case mailbox_missing_state 1 "$TMP_ROOT/mailbox_missing_state.json" 'unevaluable: health/detail scan failed'
run_case recovery_string 1 "$TMP_ROOT/recovery_string.json" 'unevaluable: health/detail scan failed'

marker_template_output=$(MARKER_COVERAGE='marker-sentinel-%s' bash -s -- "$REGION" "$TMP_ROOT/tmux_dead.json" <<'CHILD'
set -euo pipefail
eval "$(<"$1")"; POST_DEPLOY_SMOKE_WEDGE_MARKER_COVERAGE="$MARKER_COVERAGE"; POST_DEPLOY_SMOKE_WEDGE_CLEAN_COVERAGE="${POST_DEPLOY_SMOKE_WEDGE_MARKER_COVERAGE/\%s/0}"
POST_DEPLOY_SMOKE_EVIDENCE="${TMPDIR:-/tmp}/5244-marker-template.evidence"; POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$2"; POST_DEPLOY_SMOKE_FAILURES=(); POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"; : > "$POST_DEPLOY_SMOKE_EVIDENCE"
if _post_deploy_smoke_check_wedges; then rc=0; else rc=$?; fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
)
grep -q 'CASE_DONE rc=1 coverage=marker-sentinel-1' <<< "$marker_template_output" || fail 'marker coverage drifted from the shared template'

missing_body_output=$(bash -s -- "$REGION" <<'CHILD'
set -euo pipefail
eval "$(<"$1")"
POST_DEPLOY_SMOKE_EVIDENCE="${TMPDIR:-/tmp}/5244-missing.evidence"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY=""
POST_DEPLOY_SMOKE_FAILURES=()
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
if _post_deploy_smoke_check_wedges; then rc=0; else rc=$?; fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
)
grep -q 'CASE_DONE rc=1 coverage=unevaluable: /api/health/detail body unavailable' <<< "$missing_body_output" || fail 'body-unavailable case did not fail with unevaluable coverage'

jq_missing_output=$(bash -s -- "$REGION" "$TMP_ROOT/clean.json" <<'CHILD'
set -euo pipefail
eval "$(<"$1")"
POST_DEPLOY_SMOKE_EVIDENCE="${TMPDIR:-/tmp}/5244-jq-missing.evidence"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$2"
POST_DEPLOY_SMOKE_FAILURES=()
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
PATH="${TMPDIR:-/tmp}/does-not-contain-jq"
if _post_deploy_smoke_check_wedges; then rc=0; else rc=$?; fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
)
grep -q 'CASE_DONE rc=1 coverage=unevaluable: jq unavailable' <<< "$jq_missing_output" || fail 'jq-unavailable case did not use separate vocabulary'

run_contract_case() {
    local label="$1" encoded_count="$2"
    local output
    output=$(COUNT_VALUE="$encoded_count" bash -s -- "$REGION" "$TMP_ROOT/clean.json" <<'CHILD'
set -euo pipefail
eval "$(<"$1")"
POST_DEPLOY_SMOKE_EVIDENCE="${TMPDIR:-/tmp}/5244-contract.evidence"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$2"
POST_DEPLOY_SMOKE_FAILURES=()
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"
_post_deploy_smoke_wedge_scan_from_file() {
    printf 'recovered=true\ncount=%s\nmarker=fake\n' "$COUNT_VALUE"
}
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
if _post_deploy_smoke_check_wedges; then rc=0; else rc=$?; fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
    )
    grep -q 'CASE_DONE rc=1 coverage=unevaluable: wedge scan output contract violated' <<< "$output" || fail "$label: malformed count was accepted"
}
run_contract_case count_10x '10x'
run_contract_case count_space '10 2'
run_contract_case count_newline $'10\n2'
run_contract_case count_08 '08'
run_contract_case count_negative '-1'
run_contract_case count_empty ''

observation_output=$(bash -s -- "$REGION" "$TMP_ROOT/observations.json" <<'CHILD'
set -euo pipefail
eval "$(<"$1")"
POST_DEPLOY_SMOKE_EVIDENCE="${TMPDIR:-/tmp}/5244-observation.evidence"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$2"
POST_DEPLOY_SMOKE_FAILURES=()
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
if _post_deploy_smoke_check_wedges; then rc=0; else rc=$?; fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
)
grep -q 'relay wedge observation: queue_blocked' <<< "$observation_output" || fail 'queue_blocked was not reported as an observation'
grep -q 'relay wedge observation: unknown_stall_state' <<< "$observation_output" || fail 'unknown stall was not reported as an observation'
grep -q 'relay wedge observation: unpaired_active_token provider=claude channel=3' <<< "$observation_output" || fail 'unpaired active token was not reported as its dedicated observation'
! grep -q 'unknown_stall_state provider=claude channel=3' <<< "$observation_output" || fail 'known unpaired active token leaked into unknown stall observations'

recovery_observation_output=$(RECOVERY_EVIDENCE="$TMP_ROOT/recovery-observation.evidence" bash -s -- "$REGION" "$TMP_ROOT/recovering_observations.json" <<'CHILD'
set -euo pipefail
eval "$(<"$1")"
POST_DEPLOY_SMOKE_EVIDENCE="$RECOVERY_EVIDENCE"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$2"
POST_DEPLOY_SMOKE_FAILURES=()
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
if _post_deploy_smoke_check_wedges; then rc=0; else rc=$?; fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
)
grep -q 'relay wedge observation: stall-state provider=claude channel=1 state=tmux_alive_relay_dead' <<< "$recovery_observation_output" || fail 'recovery marker observation was discarded from stdout'
grep -q 'relay wedge observation: queue_blocked' <<< "$recovery_observation_output" || fail 'recovery queue observation was discarded'
grep -q 'relay wedge observation: unknown_stall_state' <<< "$recovery_observation_output" || fail 'recovery unknown observation was discarded'
grep -q 'relay wedge observation: stall-state provider=claude channel=1 state=tmux_alive_relay_dead' "$TMP_ROOT/recovery-observation.evidence" || fail 'recovery marker observation was discarded from evidence'

observation_race_output=$(bash -s -- "$REGION" "$TMP_ROOT/recovering_observations.json" "$TMP_ROOT/race.evidence" 2>&1 <<'CHILD'
set -euo pipefail
eval "$(<"$1")"
POST_DEPLOY_SMOKE_EVIDENCE="$3"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$2"
POST_DEPLOY_SMOKE_FAILURES=()
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="not run: wedge check did not execute"
eval "$(declare -f _post_deploy_smoke_note | sed 's/_post_deploy_smoke_note/_post_deploy_smoke_note_real/')"
_post_deploy_smoke_note() {
    local message="$1"
    if [[ "$message" == relay\ wedge\ observation:* ]]; then
        rm -f "$POST_DEPLOY_SMOKE_EVIDENCE"
        mkdir "$POST_DEPLOY_SMOKE_EVIDENCE"
    fi
    _post_deploy_smoke_note_real "$message"
}
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
if _post_deploy_smoke_check_wedges; then rc=0; else rc=$?; fi
printf 'CASE_DONE rc=%s coverage=%s\n' "$rc" "$POST_DEPLOY_SMOKE_WEDGE_COVERAGE"
CHILD
)
grep -q 'CASE_DONE rc=1 coverage=not evaluated: startup recovery in progress' <<< "$observation_race_output" || fail 'observation evidence failure reverted to not-run coverage'

disposition_case() {
    local label="$1" smoke_rc="$2" coverage="$3" durable_coverage="$4" expected="$5"
    local output
    output=$(SMOKE_RC="$smoke_rc" COVERAGE="$coverage" DURABLE_COVERAGE="$durable_coverage" CLEAN_COVERAGE=clean-sentinel DURABLE_CLEAN_COVERAGE="$durable_clean_coverage" bash -s -- "$DISPOSITION" <<'CHILD'
set -euo pipefail
POST_DEPLOY_SMOKE_WEDGE_COVERAGE="$COVERAGE"
POST_DEPLOY_SMOKE_WEDGE_CLEAN_COVERAGE="$CLEAN_COVERAGE"
POST_DEPLOY_SMOKE_DURABLE_COVERAGE="$DURABLE_COVERAGE"
POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE="$DURABLE_CLEAN_COVERAGE"
POST_DEPLOY_SMOKE_EVIDENCE=/tmp/5244-disposition.evidence
POST_DEPLOY_SMOKE_TMP_DIR=""
POST_DEPLOY_SMOKE_FAILURES=()
_run_post_deploy_functional_smoke() { return "$SMOKE_RC"; }
_report_post_deploy_smoke_failure() { printf 'REPORT_CALLED\n'; }
eval "$(<"$1")"
printf 'CASE_DONE disposition\n'
CHILD
    )
    grep -q 'CASE_DONE disposition' <<< "$output" || fail "$label: disposition completion token missing"
    grep -q "$expected" <<< "$output" || fail "$label: expected [$expected]"
    if [ "$label" = evaluated_clean ]; then
        grep -q 'durable record coverage: evaluated' <<< "$output" || fail "$label: durable summary missing"
    fi
}
disposition_case evaluated_clean 0 clean-sentinel evaluated 'passed (relay wedge coverage: clean-sentinel'
disposition_case skip_unevaluable 0 clean-sentinel 'unevaluable: E-35 did not run' 'completed with coverage gap (relay wedge coverage: clean-sentinel; durable record coverage: unevaluable: E-35 did not run'
disposition_case plain_unevaluable 0 clean-sentinel unevaluable 'completed with coverage gap'
disposition_case failed_nonclean 0 clean-sentinel failed 'completed with coverage gap'
disposition_case coverage_gap 0 'not evaluated: startup recovery in progress' evaluated 'completed with coverage gap'
disposition_case failed 1 'unevaluable: health/detail scan failed' failed 'REPORT_CALLED'

if [ -z "${MUTATION_CHILD:-}" ]; then
    for mutation in logical_and_removed unevaluable_as_clean; do
        mut="$TMP_ROOT/mut-$mutation.sh"
        if [ "$mutation" = logical_and_removed ]; then
            sed \
                -e 's/\$POST_DEPLOY_SMOKE_WEDGE_COVERAGE:\$POST_DEPLOY_SMOKE_DURABLE_COVERAGE/\$POST_DEPLOY_SMOKE_WEDGE_COVERAGE/g' \
                -e 's/\$POST_DEPLOY_SMOKE_WEDGE_CLEAN_COVERAGE:\$POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE/\$POST_DEPLOY_SMOKE_WEDGE_CLEAN_COVERAGE/g' \
                "$DEPLOY_SH" > "$mut"
            expected='E-1 skip produced a false pass'
        else
            sed 's/^POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE="evaluated"$/POST_DEPLOY_SMOKE_DURABLE_CLEAN_COVERAGE="unevaluable: E-35 did not run"/' \
                "$DEPLOY_SH" > "$mut"
            expected='skip_unevaluable: expected'
        fi
        if ! bash -n "$mut" > "$mut.bash-n" 2>&1; then
            fail "$mutation was not syntactically valid"
        elif MUTATION_CHILD=1 DEPLOY_SH_OVERRIDE="$mut" bash "$0" > "$mut.out" 2>&1; then
            fail "$mutation survived the skip fixture"
        elif grep -q "$expected" "$mut.out"; then
            printf 'MUTATION_CASE %s bash_n=0 fixture_rc=1\n' "$mutation"
        else
            fail "$mutation died without its target fixture self-assertion"
        fi
    done
fi

if [ "$failures" -ne 0 ]; then
    printf 'test_deploy_smoke_wedge_coverage_5244: %s assertion(s) failed\n' "$failures" >&2
    exit 1
fi
printf 'test_deploy_smoke_wedge_coverage_5244: all assertions passed\n'
