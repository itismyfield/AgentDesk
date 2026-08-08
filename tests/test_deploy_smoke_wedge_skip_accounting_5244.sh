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
DEPLOY_SH="${DEPLOY_SH_OVERRIDE:-$REPO_ROOT/scripts/deploy-release.sh}"
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
    _post_deploy_smoke_wedge_usable_int \
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

# NOTE: production _post_deploy_smoke_fail prints lines beginning "FAIL:" as
# ordinary fixture output. Assertion failures here are marked ASSERT-FAIL.

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
    sequence_partial_fail)
        calls=$(cat "$STUB_STATE/curl.calls" 2>/dev/null || printf 0)
        calls=$((calls + 1))
        printf '%s\n' "$calls" > "$STUB_STATE/curl.calls"
        [ -f "$STUB_STATE/curl.body.$calls" ] || exit 22
        cat "$STUB_STATE/curl.body.$calls" > "$dest"
        [ "$calls" -ne 2 ] || exit 22
        ;;
    swap_state_dir)
        cat "$STUB_STATE/curl.body" > "$dest"
        rm -f "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
        ln -s "$SYMLINK_TARGET_DIR" "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
        ;;
    *)     exit 22 ;;
esac
STUB
# jq stub: reports whatever version the test asks for and logs that the gate
# actually consulted it, then delegates every real filter to the real jq —
# unless jq.garbage is set, in which case every filter exits 0 with output that
# has nothing to do with the input. That is the shape a version check cannot
# see: an honest --version in front of a parser that answers nonsense.
cat > "$STUB_BIN/jq" <<STUB
#!/usr/bin/env bash
if [ "\$1" = "--version" ]; then
    printf 'x\n' >> "\$STUB_STATE/jq.version.calls"
    cat "\$STUB_STATE/jq.version"
    exit 0
fi
if [ -f "\$STUB_STATE/jq.garbage" ]; then
    printf 'garbage-but-rc-zero\n'
    exit 0
fi
if [ -f "\$STUB_STATE/jq.markersuppress" ]; then
    for arg in "\$@"; do
        case "\$arg" in *degraded_reason=*) exit 0 ;; esac
    done
fi
# jq.boolgarble: honest everywhere including the gate's self-test, then garbage
# from the SECOND fully_recovered read onwards. That filter is the only one
# carrying this error message, so this reaches _post_deploy_smoke_fully_
# recovered_from_file and nothing else.
for arg in "\$@"; do
    case "\$arg" in
        *"is not boolean"*)
            n=\$(cat "\$STUB_STATE/jq.boolgarble.calls" 2>/dev/null || printf 0)
            n=\$((n + 1))
            printf '%s\n' "\$n" > "\$STUB_STATE/jq.boolgarble.calls"
            if [ -f "\$STUB_STATE/jq.boolgarble" ] && [ "\$n" -ge 2 ]; then
                printf 'garbage-but-rc-zero\n'
                exit 0
            fi
            ;;
    esac
done
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

# Fixtures carry the whole gated contract — mailboxes array, degraded_reasons
# array, boolean fully_recovered — because that is what the server emits and
# what the gate now demands.
CLEAN_BODY='{"fully_recovered": true, "mailboxes": [], "degraded_reasons": []}'
WEDGED_BODY='{"fully_recovered": true, "degraded_reasons": [], "mailboxes": [{"provider":"claude","channel_id":"c1","relay_stall_state":"tmux_alive_relay_dead","relay_health":{"desynced":true}}]}'
OWNERLESS_BODY='{"fully_recovered": true, "degraded_reasons": [], "mailboxes": [{"provider":"claude","channel_id":"c1","relay_stall_state":"healthy","inflight_state_present":true,"watcher_attached":true,"relay_health":{"desynced":false,"stale_thread_proof":false,"watcher_attached_stale":false}}]}'
UNRECOVERED_BODY='{"fully_recovered": false, "mailboxes": [], "degraded_reasons": []}'

failures=0
fail_test() {
    echo "ASSERT-FAIL: $1" >&2
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
    rm -f "$STUB_STATE/jq.garbage" "$STUB_STATE/jq.boolgarble" "$STUB_STATE/jq.markersuppress"
    printf '0\n' > "$STUB_STATE/jq.boolgarble.calls"
    printf '0\n' > "$STUB_STATE/curl.calls"
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
    export STUB_STATE
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

# relay_stall_state is classified by an explicit allowlist. Drive the complete
# check so each normal state must reach markers=absent and each abnormal state
# must persist its own marker after resampling. Other marker fields are false,
# which prevents those clauses from hiding a missing stall-state comparison.
while read -r stall expected_rc; do
    setup_case
    body=$(printf '%s\n' \
        "{\"fully_recovered\":true,\"degraded_reasons\":[],\"mailboxes\":[{\"provider\":\"claude\",\"channel_id\":\"c1\",\"relay_stall_state\":\"$stall\",\"inflight_state_present\":false,\"watcher_attached\":true,\"relay_health\":{\"desynced\":false,\"stale_thread_proof\":false,\"watcher_attached_stale\":false}}]}")
    printf '%s\n' "$body" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    printf 'body\n' > "$STUB_STATE/curl.mode"
    printf '%s\n' "$body" > "$STUB_STATE/curl.body"
    run_check; check_rc "$expected_rc" "$RUN_CHECK_RC" "relay stall state $stall marker verdict"
    if [ "$expected_rc" -eq 0 ]; then
        if grep -q 'relay wedge markers=absent' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
            pass_test "relay stall state $stall emits no marker"
        else
            fail_test "relay stall state $stall did not reach markers=absent"
        fi
    elif grep -q "relay wedge marker persisted.*stall=$stall" "$POST_DEPLOY_SMOKE_EVIDENCE"; then
        pass_test "relay stall state $stall emits a marker"
    else
        fail_test "relay stall state $stall did not persist its marker"
    fi
done <<'STALL_STATES'
healthy 0
active_foreground_stream 0
explicit_background_work 0
orphan_pending_token 1
queue_blocked 1
tmux_alive_relay_dead 1
STALL_STATES

setup_case
printf '%s\n' "$OWNERLESS_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check; check_rc 0 "$RUN_CHECK_RC" "missing relay_owner_kind is not ownerless wedge evidence"
if [ "$(stored_count)" = "absent" ]; then
    pass_test "wire-level owner absence reaches a clean marker verdict"
else
    fail_test "wire-level owner absence changed skip accounting"
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

# A failed curl may still replace its -o file. The same recovery path is reused,
# so path identity would bless those new bytes with the previous poll's gate.
# Content identity must refuse them and account the unevaluable wait as a skip.
setup_case
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=2
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
printf '%s\n' "$UNRECOVERED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'sequence_partial_fail\n' > "$STUB_STATE/curl.mode"
printf '%s\n' "$UNRECOVERED_BODY" > "$STUB_STATE/curl.body.1"
printf '%s\n' "$CLEAN_BODY" > "$STUB_STATE/curl.body.2"
run_check; check_rc 0 "$RUN_CHECK_RC" "an un-gated replacement body cannot become a clean verdict"
if [ "$(stored_count)" = "1" ]; then
    pass_test "a failed fetch that replaced a gated path is accounted as a skip"
else
    fail_test "a failed fetch that replaced a gated path left consecutive_skips=$(stored_count), want 1"
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
# The gate must assert the WHOLE contract its consumers read, not one field of
# it. A gate narrower than its consumers is the same defect one field over, so
# the shapes below are organised as a 1:1 contrast with the consumption sites:
# each row breaks exactly one consumed field and nothing else, and every row
# must be unevaluable.
#
#   consumer : field                              broken by rows tagged
#   -------------------------------------------   ---------------------
#   _markers_from_file : .mailboxes[]?             [mailboxes]
#   _markers_from_file : .degraded_reasons[]?      [degraded_reasons]
#   _fully_recovered_from_file : .fully_recovered  [fully_recovered]
#
# If a consumer is added that reads a fifth thing, this table gains a row and
# the gate gains a clause. Drift between the two columns IS the defect.
while IFS= read -r shape; do
    setup_case
    printf '%s\n' "$shape" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    run_check; check_rc 1 "$RUN_CHECK_RC" "top-level shape $shape is unevaluable"
done <<'SHAPES'
{"fully_recovered": true, "degraded_reasons": []}
{"fully_recovered": true, "degraded_reasons": [], "mailboxes": null}
{"fully_recovered": true, "degraded_reasons": [], "mailboxes": "none"}
{"fully_recovered": true, "degraded_reasons": [], "mailboxes": 0}
{"fully_recovered": true, "degraded_reasons": [], "mailboxes": {}}
{"fully_recovered": true, "mailboxes": []}
{"fully_recovered": true, "mailboxes": [], "degraded_reasons": null}
{"fully_recovered": true, "mailboxes": [], "degraded_reasons": "relay wedge"}
{"fully_recovered": true, "mailboxes": [], "degraded_reasons": {}}
{"mailboxes": [], "degraded_reasons": []}
{"fully_recovered": null, "mailboxes": [], "degraded_reasons": []}
{"fully_recovered": "true", "mailboxes": [], "degraded_reasons": []}
{"fully_recovered": 1, "mailboxes": [], "degraded_reasons": []}
[null]
["mailboxes"]
"a string body"
17
SHAPES

# The resample body is gated too, on every clause. Before this PR a
# schema-broken resample produced zero markers, which `comm -12` read as
# "markers cleared".
for broken_resample in \
    '{"fully_recovered": true, "degraded_reasons": [], "mailboxes": "none"}' \
    '{"fully_recovered": true, "mailboxes": [], "degraded_reasons": "none"}' \
    '{"fully_recovered": "yes", "mailboxes": [], "degraded_reasons": []}'
do
    setup_case
    printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    printf 'body\n' > "$STUB_STATE/curl.mode"
    printf '%s\n' "$broken_resample" > "$STUB_STATE/curl.body"
    run_check
    check_rc 1 "$RUN_CHECK_RC" "schema-broken settle resample $broken_resample is unevaluable, not cleared"
done

# A recovery POLL body that breaks the contract is the case leg B reproduced as
# a green skip: the wait could not read fully_recovered, gave up at the
# deadline, and called that "recovery state unavailable". It is a gate trip.
for broken_poll in \
    '{"mailboxes": [], "degraded_reasons": []}' \
    '{"fully_recovered": "yes", "mailboxes": [], "degraded_reasons": []}' \
    '{"fully_recovered": true, "mailboxes": [], "degraded_reasons": "wedge"}'
do
    setup_case
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=2
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
    printf '%s\n' "$UNRECOVERED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    printf 'body\n' > "$STUB_STATE/curl.mode"
    printf '%s\n' "$broken_poll" > "$STUB_STATE/curl.body"
    run_check
    check_rc 1 "$RUN_CHECK_RC" "schema-broken recovery poll $broken_poll is a gate trip, not a skip"
done

# ── The single gate: jq that exits 0 and lies ───────────────────────────────
# The version gate proves a claim about jq, not a behaviour. Only the gate's
# semantic self-test separates a working parser from one that returns success
# with unrelated output — and that output otherwise reads as "not recovered",
# i.e. a skip for a body nobody parsed.
setup_case
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
: > "$STUB_STATE/jq.garbage"
run_check; garbage_rc="$RUN_CHECK_RC"
rm -f "$STUB_STATE/jq.garbage"
check_rc 1 "$garbage_rc" "a jq that exits 0 with garbage output is unevaluable"
if [ "$(stored_count)" = "absent" ]; then
    pass_test "a lying jq is an accounting failure, not a skip"
else
    fail_test "a lying jq recorded consecutive_skips=$(stored_count), want no skip"
fi

# The self-test cannot cover a jq that answers honestly once and then garbles a
# real body, so the reader checks its own output too. Isolating that guard takes
# the settle path: the recovery read is honest (so the wait completes and the
# markers are real), the settle read is garbage. Without the reader's true/false
# check that garbage merely fails `= "false"`, the check walks on to compare
# markers, finds them cleared, and RESETS the streak — a clean verdict derived
# from a recovery state nobody read. With it, the deploy skips, and from a
# streak of 2 that skip is the third, which is red.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'body\n' > "$STUB_STATE/curl.mode"
printf '%s\n' "$CLEAN_BODY" > "$STUB_STATE/curl.body"
_post_deploy_smoke_wedge_state_write '{"consecutive_skips": 2}' "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
: > "$STUB_STATE/jq.boolgarble"
run_check; boolgarble_rc="$RUN_CHECK_RC"
rm -f "$STUB_STATE/jq.boolgarble"
check_rc 1 "$boolgarble_rc" "a garbled settle recovery state cannot become a cleared verdict"
if [ "$(stored_count)" = "3" ]; then
    pass_test "a garbled settle recovery state is skipped, not reset"
else
    fail_test "a garbled settle recovery state left consecutive_skips=$(stored_count), want 3"
fi

# A gate that trips DURING the recovery wait is the case only the exit
# assertion can catch: the wait gives up, the caller records a skip, and the
# body of the check therefore returns 0. Nothing but the assertion in
# _post_deploy_smoke_check_wedges turns that back into a failure.
setup_case
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=4
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
printf '%s\n' "$UNRECOVERED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'body\n' > "$STUB_STATE/curl.mode"
printf '%s\n' '{"fully_recovered": true, "mailboxes": 7}' > "$STUB_STATE/curl.body"
run_check; check_rc 1 "$RUN_CHECK_RC" "a gate tripped inside the recovery wait cannot end as a skip"

# The sole jq reader refuses a body the gate has not just cleared. This is the
# coupling that makes every gate call site load-bearing: a future site that
# fetches a body and forgets to gate it degrades to a visible refusal instead
# of silently parsing an unvalidated body.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$TMP_ROOT/ungated.json"
POST_DEPLOY_SMOKE_WEDGE_GATED_BODY="$TMP_ROOT/some-other-body.json"
ungated_rc=0
_post_deploy_smoke_wedge_markers_from_file "$TMP_ROOT/ungated.json" > /dev/null 2>&1 || ungated_rc=$?
check_rc 1 "$ungated_rc" "an ungated body is refused by the sole jq reader"

# ── The single gate: knobs bash can actually compare ────────────────────────
# A digit-only wait outside bash's integer range passes a shape check and then
# makes every `-ge` in the wait loop error out. The loop reads that error as
# "deadline not reached", so the bounded wait stops being bounded. The stub
# curl counter proves the gate stops it BEFORE the first poll rather than
# merely surviving it.
for huge_knob in 9223372036854775808 99999999999999999999 999999999999999999999999999999999999; do
    setup_case
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS="$huge_knob"
    POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
    printf '%s\n' "$UNRECOVERED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    printf 'body\n' > "$STUB_STATE/curl.mode"
    printf '%s\n' "$CLEAN_BODY" > "$STUB_STATE/curl.body"
    printf '0\n' > "$STUB_STATE/curl.calls"
    run_check
    check_rc 1 "$RUN_CHECK_RC" "out-of-range recovery wait '$huge_knob' is unevaluable"
    if [ "$(cat "$STUB_STATE/curl.calls")" = "0" ]; then
        pass_test "out-of-range wait '$huge_knob' never entered the poll loop"
    else
        fail_test "out-of-range wait '$huge_knob' polled $(cat "$STUB_STATE/curl.calls") time(s)"
    fi
done
# 2^63-1 is comparable but wraps to a negative on the +1 the counter performs,
# so the gate rejects it too; 2^63-2 is the largest knob that survives both.
setup_case
POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS=9223372036854775807
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check; check_rc 1 "$RUN_CHECK_RC" "a skip limit that wraps when incremented is unevaluable"
setup_case
POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS=9223372036854775806
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check; check_rc 0 "$RUN_CHECK_RC" "the largest non-wrapping skip limit is accepted"

# Leading zeroes are decimal operator spelling, not an octal knob.
setup_case
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=010
POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS=010
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
run_check; check_rc 0 "$RUN_CHECK_RC" "leading-zero integer knobs are normalized as decimal"
if [ "$POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS" = 10 ] \
  && [ "$POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS" = 10 ]; then
    pass_test "leading-zero wedge knobs are stored in decimal form"
else
    fail_test "leading-zero wedge knobs were not normalized"
fi

# ── A stored count outside bash's range is corruption, not zero ─────────────
# Schema-valid and believable-looking, but `count + 1` wraps to a negative that
# compares below every threshold — a state that had blown past the limit would
# come back reading as brand new. It must land in the corrupt tally instead, so
# a second consecutive occurrence turns the smoke red.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf '{"consecutive_skips": 9223372036854775807}\n' > "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
run_check; check_rc 0 "$RUN_CHECK_RC" "an out-of-range stored count recovers from 0 once"
if [ "$(cat "${POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE}.corrupt" 2>/dev/null || printf 'absent')" = "1" ]; then
    pass_test "an out-of-range stored count is tallied as corruption"
else
    fail_test "an out-of-range stored count was not tallied as corruption"
fi
if [ "$(stored_count)" = "1" ]; then
    pass_test "an out-of-range stored count does not wrap into the counter"
else
    fail_test "an out-of-range stored count left consecutive_skips=$(stored_count), want 1"
fi
printf '{"consecutive_skips": 99999999999999999999}\n' > "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
run_check; check_rc 1 "$RUN_CHECK_RC" "a second out-of-range stored count fails the smoke"

# Once the configured limit has been reached, later skips remain red. Saturate
# at the limit instead of writing the next integer and reclassifying it as
# corruption on the following deploy.
setup_case
POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS=9223372036854775806
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
_post_deploy_smoke_wedge_state_write \
    '{"consecutive_skips": 9223372036854775806}' "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
run_check; check_rc 1 "$RUN_CHECK_RC" "a skip at the maximum limit remains red"
run_check; check_rc 1 "$RUN_CHECK_RC" "the next skip after the maximum limit remains red"
if [ "$(stored_count)" = 9223372036854775806 ] \
  && [ ! -e "${POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE}.corrupt" ]; then
    pass_test "the reached limit stays monotonic without corruption recovery"
else
    fail_test "the reached limit escaped through corruption recovery"
fi

# ── The single gate: skip-state I/O ─────────────────────────────────────────
setup_case
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf '{"consecutive_skips": 0}\n' > "$ADK_REL/runtime/symlink-target.json"
ln -sf "$ADK_REL/runtime/symlink-target.json" "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
run_check; check_rc 1 "$RUN_CHECK_RC" "symlinked skip state is unevaluable"

# ── Predictable scratch paths are not writable handles ──────────────────────
# `.probe.$$` and `.tmp.$$` are guessable. A symlink planted at either is
# written THROUGH by a plain redirection: the whole checker returns 0 while an
# unrelated file is truncated, and in the `.tmp` case the atomic replace then
# renames the symlink onto the state path, quietly undoing the gate's own
# regular-file verdict. Exclusive creation is what makes the gate's claim true.
setup_case
printf '%s\n' "$CLEAN_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'victim-contents-must-survive\n' > "$ADK_REL/runtime/victim"
ln -sf "$ADK_REL/runtime/victim" "${POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE}.probe.$$"
run_check
if [ "$(cat "$ADK_REL/runtime/victim")" = "victim-contents-must-survive" ]; then
    pass_test "the gate's writability probe does not truncate a planted symlink target"
else
    fail_test "the gate's writability probe truncated the planted symlink target"
fi

setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf 'victim-contents-must-survive\n' > "$ADK_REL/runtime/victim"
ln -sf "$ADK_REL/runtime/victim" "${POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE}.tmp.$$"
run_check
if [ "$(cat "$ADK_REL/runtime/victim")" = "victim-contents-must-survive" ]; then
    pass_test "the atomic replace does not write through a planted temp symlink"
else
    fail_test "the atomic replace wrote the skip state through a planted temp symlink"
fi
if [ ! -L "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE" ]; then
    pass_test "the skip state is still a regular file after the atomic replace"
else
    fail_test "the atomic replace turned the skip state into a symlink"
fi
if [ "$(stored_count)" = "1" ]; then
    pass_test "the skip was still recorded while refusing the planted temp symlink"
else
    fail_test "the planted temp symlink cost the skip increment (count=$(stored_count))"
fi

# BSD mv follows a destination symlink to a directory unless -h is used. Swap
# that symlink after the gate and verify the atomic replacement neither follows
# it nor overwrites the attacker's same-basename sibling.
setup_case
printf '%s\n' "$WEDGED_BODY" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
SYMLINK_TARGET_DIR="$ADK_REL/runtime/attacker-dir"
mkdir -p "$SYMLINK_TARGET_DIR"
victim="$SYMLINK_TARGET_DIR/$(basename "${POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE}.tmp.$$")"
printf 'victim-contents-must-survive\n' > "$victim"
export SYMLINK_TARGET_DIR
printf 'swap_state_dir\n' > "$STUB_STATE/curl.mode"
printf '%s\n' "$WEDGED_BODY" > "$STUB_STATE/curl.body"
run_check; check_rc 1 "$RUN_CHECK_RC" "a persistent marker still fails after a destination symlink swap"
if [ ! -L "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE" ] \
  && [ "$(cat "$victim")" = "victim-contents-must-survive" ]; then
    pass_test "the atomic replace does not follow a directory symlink destination"
else
    fail_test "the atomic replace followed a directory symlink destination"
fi

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
# 7 = 8 - 2 + 1. main had 8 wedge skip branches; S2 replaced the two
# first-sample recovery branches with one bounded wait (-2) and that merged
# wait still ends in a skip of its own when the deadline expires (+1). Counting
# call sites proves every skip goes through the accounting helper, not that
# every branch that ought to skip does. A genuine refactor may change this
# number; change it deliberately and say why.
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
