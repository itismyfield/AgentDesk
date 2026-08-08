#!/usr/bin/env bash
# Regression suite for #5244 (PR-A): the post-deploy relay wedge check must not
# report a pass for a check it never performed.
#
# Two behaviours are pinned here.
#
#   S1' skip accounting — a skip is recorded, not swallowed. Consecutive skips
#       accumulate across deploys in $ADK_REL/runtime and, at the limit, return
#       nonzero all the way out through _run_post_deploy_functional_smoke. The
#       accumulation is what the repeated-deploy tests below exercise: a call
#       counter cannot see a streak that resets itself.
#
#   The single gate — "cannot evaluate" is decided in one function and enforced
#       in one place. Three earlier rounds each patched one site and exposed the
#       next, so these tests drive the WHOLE _post_deploy_smoke_check_wedges
#       under a stubbed PATH rather than calling helpers directly: a gate that
#       only protects the helper it lives in is precisely the defect.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-wedge-skip-test.XXXXXX")
# `set -u` aborts mid-suite report rc=0 through some `|| rc=$?` contexts, which
# would read as a pass. Only the completion flag can make this exit 0.
SUITE_COMPLETED=0
cleanup() {
    local rc=$?
    rm -rf "$TMP_ROOT"
    if [ "$SUITE_COMPLETED" -ne 1 ] && [ "$rc" -eq 0 ]; then
        echo "suite aborted before completing" >&2
        exit 1
    fi
    exit "$rc"
}
trap cleanup EXIT

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

for fn in \
    _post_deploy_smoke_note \
    _post_deploy_smoke_fail \
    _post_deploy_smoke_wedge_gate_jq \
    _post_deploy_smoke_wedge_gate_config \
    _post_deploy_smoke_wedge_gate_state \
    _post_deploy_smoke_wedge_gate_body \
    _post_deploy_smoke_wedge_gate \
    _post_deploy_smoke_wedge_jq \
    _post_deploy_smoke_wedge_fetch_health_detail \
    _post_deploy_smoke_wedge_state_write \
    _post_deploy_smoke_wedge_state_read \
    _post_deploy_smoke_wedge_corrupt_tally_read \
    _post_deploy_smoke_wedge_skip_state_bump \
    _post_deploy_smoke_wedge_skip_state_reset \
    _post_deploy_smoke_wedge_skip \
    _post_deploy_smoke_wedge_markers_from_file \
    _post_deploy_smoke_fully_recovered_from_file \
    _post_deploy_smoke_wedge_await_recovery \
    _post_deploy_smoke_check_wedges_inner \
    _post_deploy_smoke_check_wedges \
    _run_post_deploy_functional_smoke
do
    body=$(extract_function "$fn")
    if [ -z "$body" ]; then
        echo "harness: could not extract $fn from $DEPLOY_SH" >&2
        exit 1
    fi
    eval "$body"
done

# The three sibling checks are out of scope; stub them so the end-to-end
# propagation assertion isolates the wedge check's return value.
_post_deploy_smoke_probe_apis() { return 0; }
_post_deploy_smoke_check_fail_closed_warn_rate() { return 0; }
_post_deploy_smoke_check_relay_round_trip() { return 0; }

REL_PORT=65535
ADK_DEFAULT_LOOPBACK="127.0.0.1"
STUB_STATE="$TMP_ROOT/stub"
REAL_JQ="$(command -v jq)"
mkdir -p "$STUB_STATE"
export STUB_STATE REL_PORT ADK_DEFAULT_LOOPBACK

# curl stub: `mode` selects fetch failure / empty body / a canned body.
STUB_BIN="$TMP_ROOT/bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/curl" <<'STUB'
#!/usr/bin/env bash
dest=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "-o" ]; then dest="$arg"; fi
    prev="$arg"
done
case "$(cat "$STUB_STATE/curl.mode" 2>/dev/null || printf 'fail')" in
    empty) : > "$dest" ;;
    body)  cat "$STUB_STATE/curl.body" > "$dest" ;;
    *)     exit 22 ;;
esac
STUB
# jq stub: reports whatever version the test asks for and logs that the gate
# actually consulted it, then delegates every real filter to the real jq.
cat > "$STUB_BIN/jq" <<STUB
#!/usr/bin/env bash
if [ "\$1" = "--version" ]; then
    printf 'x\n' >> "\$STUB_STATE/jq.version.calls"
    cat "\$STUB_STATE/jq.version"
    exit 0
fi
exec "$REAL_JQ" "\$@"
STUB
chmod +x "$STUB_BIN/curl" "$STUB_BIN/jq"
PATH="$STUB_BIN:$PATH"

# A PATH with the externals the wedge path needs but deliberately no jq.
NOJQ_BIN="$TMP_ROOT/nojq"
mkdir -p "$NOJQ_BIN"
for tool in cat mv rm mkdir dirname sleep comm sort mktemp hostname date; do
    ln -sf "$(command -v "$tool")" "$NOJQ_BIN/$tool"
done
ln -sf "$STUB_BIN/curl" "$NOJQ_BIN/curl"

CLEAN_BODY='{"fully_recovered": true, "mailboxes": []}'
WEDGED_BODY='{"fully_recovered": true, "mailboxes": [{"provider":"claude","channel_id":"c1","relay_stall_state":"stalled"}]}'
UNRECOVERED_BODY='{"fully_recovered": false, "mailboxes": []}'

failures=0
fail_test() {
    echo "FAIL: $1" >&2
    failures=$((failures + 1))
}
pass_test() { echo "ok: $1"; }
check_rc() { # want, got, label
    if [ "$2" -eq "$1" ]; then pass_test "$3"; else fail_test "$3 (want rc=$1, got rc=$2)"; fi
}

# Fresh per-test world: empty runtime state, empty evidence, chosen bodies.
setup_case() {
    ADK_REL="$TMP_ROOT/release"
    rm -rf "$ADK_REL"
    mkdir -p "$ADK_REL/logs" "$ADK_REL/runtime"
    POST_DEPLOY_SMOKE_EVIDENCE="$ADK_REL/logs/evidence.log"
    : > "$POST_DEPLOY_SMOKE_EVIDENCE"
    POST_DEPLOY_SMOKE_TMP_DIR="$TMP_ROOT/smoke-tmp"
    rm -rf "$POST_DEPLOY_SMOKE_TMP_DIR"
    mkdir -p "$POST_DEPLOY_SMOKE_TMP_DIR"
    POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$TMP_ROOT/health-detail.json"
    POST_DEPLOY_SMOKE_STAMP="test-stamp"
    POST_DEPLOY_SMOKE_FAILURES=()
    POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS=0
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=0
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
    POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS=3
    POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE="$ADK_REL/runtime/post_deploy_smoke_wedge_skips.json"
    POST_DEPLOY_SMOKE_WEDGE_GATE_FAILED=""
    POST_DEPLOY_SMOKE_WEDGE_GATED_BODY=""
    POST_DEPLOY_SMOKE_WEDGE_GATE_JQ_OK=""
    POST_DEPLOY_SMOKE_WEDGE_GATE_STATE_OK=""
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_BODY=""
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_REASON=""
    printf 'jq-1.7.1-apple\n' > "$STUB_STATE/jq.version"
    : > "$STUB_STATE/jq.version.calls"
    printf 'fail\n' > "$STUB_STATE/curl.mode"
    # The production functions arrive through eval, so the linter cannot see
    # that they consume these globals; exporting them states the contract.
    export POST_DEPLOY_SMOKE_STAMP POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS
    export POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS
    export POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE
    export POST_DEPLOY_SMOKE_WEDGE_GATE_FAILED POST_DEPLOY_SMOKE_WEDGE_GATED_BODY
    export POST_DEPLOY_SMOKE_WEDGE_GATE_JQ_OK POST_DEPLOY_SMOKE_WEDGE_GATE_STATE_OK
    export POST_DEPLOY_SMOKE_WEDGE_RECOVERY_BODY POST_DEPLOY_SMOKE_WEDGE_RECOVERY_REASON
    export POST_DEPLOY_SMOKE_EVIDENCE POST_DEPLOY_SMOKE_TMP_DIR
    export POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY ADK_REL
}
stored_count() {
    if [ -f "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE" ]; then
        "$REAL_JQ" -r '.consecutive_skips' "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE" 2>/dev/null || printf 'unreadable'
    else
        printf 'absent'
    fi
}
# Runs the WHOLE gated check in this shell — not a subshell — so the sticky
# gate flag and POST_DEPLOY_SMOKE_FAILURES it publishes stay observable. The
# rc lands in RUN_CHECK_RC; the operator notes go to the evidence file anyway.
RUN_CHECK_RC=0
run_check() {
    RUN_CHECK_RC=0
    _post_deploy_smoke_check_wedges > /dev/null || RUN_CHECK_RC=$?
}

# ── S1': one skip is counted, not swallowed ─────────────────────────────────
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check; check_rc 0 "$RUN_CHECK_RC" "single late skip keeps the smoke green"
if [ "$(stored_count)" = "1" ]; then
    pass_test "single late skip recorded consecutive_skips=1"
else
    fail_test "single late skip recorded consecutive_skips=$(stored_count), want 1"
fi

# ── S1': consecutive skips accumulate across deploys and reach the limit ─────
# The decisive test for reset placement. A reset on any skip path — the r1
# defect — makes this streak restart every deploy and never reach the limit.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check; check_rc 0 "$RUN_CHECK_RC" "repeated deploy 1/3 stays green"
run_check; check_rc 0 "$RUN_CHECK_RC" "repeated deploy 2/3 stays green"
run_check
check_rc 1 "$RUN_CHECK_RC" "repeated deploy 3/3 reaches the consecutive-skip limit"
if printf '%s\n' "${POST_DEPLOY_SMOKE_FAILURES[@]:-}" | grep -q 'skipped on 3 consecutive deploys'; then
    pass_test "limit breach recorded as a smoke FAILURE"
else
    fail_test "limit breach did not record a smoke FAILURE"
fi

# ── S1': the limit propagates out of _run_post_deploy_functional_smoke ───────
# #5244's first fix direction: the runner must not print "passed" over this.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
_post_deploy_smoke_wedge_state_write '{"consecutive_skips": 2}' "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
smoke_rc=0
_run_post_deploy_functional_smoke || smoke_rc=$?
check_rc 1 "$smoke_rc" "consecutive-skip limit fails _run_post_deploy_functional_smoke"

# ── Reset happens only on an evidence-based terminal verdict ─────────────────
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check; check_rc 0 "$RUN_CHECK_RC" "markers-absent verdict is a pass"
if [ "$(stored_count)" = "0" ]; then
    pass_test "markers-absent verdict resets the skip streak"
else
    fail_test "markers-absent verdict left consecutive_skips=$(stored_count), want 0"
fi

# A clean first deploy must not invent a skip.
setup_case
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check
if [ "$(stored_count)" = "absent" ]; then
    pass_test "clean verdict records no skip"
else
    fail_test "clean verdict recorded consecutive_skips=$(stored_count), want no state file"
fi

# Markers that clear during settle are also an evidence-based verdict.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
_post_deploy_smoke_wedge_state_write '{"consecutive_skips": 2}' "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
printf 'body\n' > "$STUB_STATE/curl.mode"
printf '%s\n' "$CLEAN_BODY" > "$STUB_STATE/curl.body"
run_check; check_rc 0 "$RUN_CHECK_RC" "markers cleared after settle is a pass"
if [ "$(stored_count)" = "0" ]; then
    pass_test "markers-cleared verdict resets the skip streak"
else
    fail_test "markers-cleared verdict left consecutive_skips=$(stored_count), want 0"
fi

# A persistent marker is still a failure, and still an evidence-based verdict.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'body\n' > "$STUB_STATE/curl.mode"
printf '%s\n' "$WEDGED_BODY" > "$STUB_STATE/curl.body"
run_check; check_rc 1 "$RUN_CHECK_RC" "persistent marker fails the wedge check"

# ── S2: bounded wait for startup recovery ───────────────────────────────────
setup_case
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=4
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
printf '%s\n' "$UNRECOVERED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'body\n' > "$STUB_STATE/curl.mode"
printf '%s\n' "$CLEAN_BODY" > "$STUB_STATE/curl.body"
run_check; check_rc 0 "$RUN_CHECK_RC" "recovery that lands inside the deadline reaches a verdict"
if [ "$(stored_count)" = "absent" ]; then
    pass_test "waited-out recovery records no skip"
else
    fail_test "waited-out recovery recorded consecutive_skips=$(stored_count), want none"
fi

setup_case
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=2
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
printf '%s\n' "$UNRECOVERED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'body\n' > "$STUB_STATE/curl.mode"
printf '%s\n' "$UNRECOVERED_BODY" > "$STUB_STATE/curl.body"
run_check; check_rc 0 "$RUN_CHECK_RC" "recovery deadline expiry is a skip, not a pass"
if [ "$(stored_count)" = "1" ]; then
    pass_test "recovery deadline expiry is counted as a skip"
else
    fail_test "recovery deadline expiry recorded consecutive_skips=$(stored_count), want 1"
fi

# ── The single gate: jq ─────────────────────────────────────────────────────
setup_case
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
nojq_rc=0
PATH="$NOJQ_BIN" _post_deploy_smoke_check_wedges || nojq_rc=$?
check_rc 1 "$nojq_rc" "absent jq fails the check instead of yielding a clean verdict"
if [ "$(stored_count)" = "absent" ]; then
    pass_test "absent jq is an accounting failure, not a skip"
else
    fail_test "absent jq recorded consecutive_skips=$(stored_count), want no skip"
fi

for bad_version in jq-1.6 jq-1.8pre jq-1.7rc1 jq-1 not-jq ''; do
    setup_case
    printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    printf '%s\n' "$bad_version" > "$STUB_STATE/jq.version"
    run_check; check_rc 1 "$RUN_CHECK_RC" "jq version '${bad_version:-<empty>}' fails closed"
    if [ -s "$STUB_STATE/jq.version.calls" ]; then
        pass_test "jq --version was actually consulted for '${bad_version:-<empty>}'"
    else
        fail_test "jq --version was never called for '${bad_version:-<empty>}'"
    fi
done

for good_version in jq-1.7 jq-1.7.1 jq-1.7.1-apple jq-2.0; do
    setup_case
    printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    printf '%s\n' "$good_version" > "$STUB_STATE/jq.version"
    run_check; check_rc 0 "$RUN_CHECK_RC" "jq version '$good_version' is accepted"
done

# ── The single gate: health/detail schema ───────────────────────────────────
# Every one of these shapes makes `.mailboxes[]?` emit nothing, which reads as
# "no wedge markers" — a clean verdict for a body that was never understood.
while IFS= read -r shape; do
    setup_case
    printf '%s\n' "$shape" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    run_check; check_rc 1 "$RUN_CHECK_RC" "top-level shape $shape is unevaluable"
done <<'SHAPES'
{"fully_recovered": true}
{"fully_recovered": true, "mailboxes": null}
{"fully_recovered": true, "mailboxes": "none"}
{"fully_recovered": true, "mailboxes": 0}
{"fully_recovered": true, "mailboxes": {}}
[null]
["mailboxes"]
"a string body"
17
SHAPES

# The resample body is gated too. Before this PR a schema-broken resample
# produced zero markers, which `comm -12` read as "markers cleared".
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'body\n' > "$STUB_STATE/curl.mode"
printf '%s\n' '{"fully_recovered": true, "mailboxes": "none"}' > "$STUB_STATE/curl.body"
run_check; check_rc 1 "$RUN_CHECK_RC" "schema-broken settle resample is unevaluable, not cleared"

# ── The single gate: skip-state I/O ─────────────────────────────────────────
setup_case
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
ln -sf /dev/null "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
run_check; check_rc 1 "$RUN_CHECK_RC" "symlinked skip state is unevaluable"

setup_case
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
chmod 500 "$ADK_REL/runtime"
run_check; readonly_rc="$RUN_CHECK_RC"
chmod 700 "$ADK_REL/runtime"
check_rc 1 "$readonly_rc" "unwritable skip-state directory is unevaluable"

# A write that fails when the skip is being recorded fails the smoke on the
# FIRST occurrence: losing the increment is losing the skip, and an uncounted
# skip can never reach the limit. Driven past the gate deliberately — the
# scenario is a write that fails after the destination was judged usable.
setup_case
POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE="$ADK_REL/runtime/nested/state.json"
mkdir -p "$ADK_REL/runtime/nested"
chmod 500 "$ADK_REL/runtime/nested"
write_rc=0
_post_deploy_smoke_wedge_skip "settle resample unavailable" > /dev/null 2>&1 || write_rc=$?
chmod 700 "$ADK_REL/runtime/nested"
check_rc 1 "$write_rc" "a failed skip-state write fails the smoke on the first occurrence"
if printf '%s\n' "${POST_DEPLOY_SMOKE_FAILURES[@]:-}" | grep -q 'skip .* would be lost'; then
    pass_test "the lost skip increment is recorded as a smoke FAILURE"
else
    fail_test "the lost skip increment was not recorded as a smoke FAILURE"
fi

# ── Corrupt skip state: recover once, fail on the streak ────────────────────
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'not json at all\n' > "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
run_check; check_rc 0 "$RUN_CHECK_RC" "one unreadable skip state recovers from 0"
if grep -q 'skip state unreadable, recovering from 0' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    pass_test "the recovery from a corrupt skip state is noted"
else
    fail_test "the recovery from a corrupt skip state was silent"
fi
if [ "$(cat "${POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE}.corrupt" 2>/dev/null || printf 'absent')" = "1" ]; then
    pass_test "the corrupt streak is tallied in the sibling file"
else
    fail_test "the corrupt streak was not tallied in the sibling file"
fi
# Second consecutive deploy with an unreadable state: the streak survives in
# the sibling, so the limit is reachable even when the count file never is.
printf 'not json at all\n' > "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
run_check; check_rc 1 "$RUN_CHECK_RC" "a second consecutive unreadable skip state fails the smoke"

# ── Migration ratchet, not an eternal invariant ─────────────────────────────
# 7 = the 8 wedge skip branches on main minus the two first-sample recovery
# branches, which S2 merged into one bounded wait, plus nothing new. A genuine
# refactor may change this number; change it deliberately and say why.
skip_sites=$(grep -c '_post_deploy_smoke_wedge_skip "' "$DEPLOY_SH" || true)
if [ "$skip_sites" -eq 7 ]; then
    pass_test "all wedge skip branches go through the accounting helper (7 sites)"
else
    fail_test "wedge skip helper call sites = $skip_sites, ratchet expects 7"
fi

SUITE_COMPLETED=1
if [ "$failures" -eq 0 ]; then
    echo "wedge skip accounting + single gate tests passed"
else
    echo "$failures assertion(s) failed" >&2
    exit 1
fi
