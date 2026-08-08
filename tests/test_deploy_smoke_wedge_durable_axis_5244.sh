#!/usr/bin/env bash
# Regression test for #5244: the post-deploy relay wedge check
#   ① counted every "give up" branch as a PASS,
#   ② had no marker that looks at the DURABLE delivery axis at all, and
#   ③ gave up permanently the moment `fully_recovered` was false.
#
# The defects share one shape — a check that reports success without ever
# reaching a verdict — so these assertions are mostly about what the check does
# when it CANNOT decide, not about a wedge it detects.
#
# Predicate coverage note: only ONE durable predicate exists, `in_memory >
# durable`. The rejected companion ("in-flight present and no delivery record")
# is asserted-against by the idle-channel negative below; delivery records are
# created only after a Delivered outcome, so an ordinary first turn sits in that
# state for minutes and must not be flagged.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-smoke-wedge-5244.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

# Exercise the production functions without executing the deploy script.
eval "$(extract_function _post_deploy_smoke_note)"
eval "$(extract_function _post_deploy_smoke_fail)"
eval "$(extract_function _post_deploy_smoke_wedge_skip_state_read)"
eval "$(extract_function _post_deploy_smoke_wedge_skip_state_write)"
eval "$(extract_function _post_deploy_smoke_wedge_skip_corrupt_tally)"
eval "$(extract_function _post_deploy_smoke_wedge_skip_state_reset)"
eval "$(extract_function _post_deploy_smoke_wedge_skip)"
eval "$(extract_function _post_deploy_smoke_wedge_markers_from_file)"
eval "$(extract_function _post_deploy_smoke_wedge_durable_axis_markers_from_file)"
eval "$(extract_function _post_deploy_smoke_wedge_all_markers_from_file)"
eval "$(extract_function _post_deploy_smoke_fully_recovered_from_file)"
eval "$(extract_function _post_deploy_smoke_wedge_await_recovery)"
eval "$(extract_function _post_deploy_smoke_check_wedges)"
eval "$(extract_function _run_post_deploy_functional_smoke)"

ADK_REL="$TMP_ROOT/release"
REL_PORT="0"
ADK_DEFAULT_LOOPBACK="127.0.0.1"
POST_DEPLOY_SMOKE_STAMP="19700101T000000Z"
POST_DEPLOY_SMOKE_EVIDENCE="$TMP_ROOT/evidence.log"
POST_DEPLOY_SMOKE_TMP_DIR="$TMP_ROOT/smoke-tmp"
POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS=0
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS=0
POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS=1
POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS=3
POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE="$ADK_REL/runtime/post_deploy_smoke_wedge_skips.json"
POST_DEPLOY_SMOKE_WEDGE_SKIP_CORRUPT_TALLY="$ADK_REL/runtime/post_deploy_smoke_wedge_skips.corrupt"
POST_DEPLOY_SMOKE_WEDGE_SKIP_MAX_CORRUPTIONS=2
POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR="$ADK_REL/runtime/discord_delivery_records"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$TMP_ROOT/health-detail.json"
# The production functions loaded through eval consume these test globals, so
# the linter cannot see the uses; exporting them states the contract. An array
# cannot be exported, hence the targeted directive below.
export ADK_REL REL_PORT ADK_DEFAULT_LOOPBACK POST_DEPLOY_SMOKE_STAMP
export POST_DEPLOY_SMOKE_EVIDENCE POST_DEPLOY_SMOKE_TMP_DIR
export POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS
export POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS
export POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE POST_DEPLOY_SMOKE_WEDGE_SKIP_CORRUPT_TALLY
export POST_DEPLOY_SMOKE_WEDGE_SKIP_MAX_CORRUPTIONS POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR
export POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY
# shellcheck disable=SC2034  # consumed by the eval'd _post_deploy_smoke_fail
POST_DEPLOY_SMOKE_FAILURES=()
mkdir -p "$ADK_REL/runtime" "$ADK_REL/logs" "$POST_DEPLOY_SMOKE_TMP_DIR"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

reset_state() {
    rm -f "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE" "$POST_DEPLOY_SMOKE_WEDGE_SKIP_CORRUPT_TALLY"
    rm -rf "${POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR:?}"
    POST_DEPLOY_SMOKE_FAILURES=()
    : > "$POST_DEPLOY_SMOKE_EVIDENCE"
}

seed_counter() {
    printf '{"consecutive_skips":%s,"last_reason":"seed","updated":"seed"}\n' "$1" \
        > "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
}

read_counter() {
    if [ ! -f "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE" ]; then
        printf 'missing'
        return 0
    fi
    jq -r '.consecutive_skips' "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
}

# A mailbox that is healthy on every PRE-EXISTING marker, so any marker the
# assertions below see can only have come from the new durable axis.
health_detail_with_mailbox() {
    local owner_channel="$1" in_memory="$2"
    cat > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" <<EOF
{
  "fully_recovered": true,
  "mailboxes": [
    {
      "provider": "claude",
      "channel_id": 111,
      "watcher_attached": true,
      "inflight_state_present": false,
      "relay_stall_state": "healthy",
      "relay_owner_kind": "watcher",
      "relay_health": {
        "watcher_owner_channel_id": ${owner_channel},
        "last_relay_offset": ${in_memory},
        "desynced": false,
        "stale_thread_proof": false,
        "watcher_attached_stale": false
      }
    }
  ]
}
EOF
}

write_delivery_record() {
    local owner_channel="$1" frontier_json="$2"
    mkdir -p "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude"
    printf '{"delivered_frontier":%s}\n' "$frontier_json" \
        > "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude/${owner_channel}.json"
}

durable_axis_markers() {
    local owner_channel="$1" in_memory="$2" frontier_json="$3"
    health_detail_with_mailbox "$owner_channel" "$in_memory"
    if [ "$frontier_json" = "none" ]; then
        rm -rf "${POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR:?}"
    else
        write_delivery_record "$owner_channel" "$frontier_json"
    fi
    _post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
}

count_lines() {
    local text="$1"
    if [ -z "$text" ]; then
        printf '0'
        return 0
    fi
    printf '%s\n' "$text" | wc -l | tr -d ' '
}

# --- T1. one skip is recorded, and stays advisory ---------------------------
reset_state
rc=0
_post_deploy_smoke_wedge_skip "startup recovery state unavailable" > /dev/null || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T1: a single skip must return 0 (advisory), got rc=$rc"
fi
if [ "$(read_counter)" != "1" ]; then
    fail_test "T1: the first skip must persist consecutive_skips=1, got '$(read_counter)'"
fi

# --- T2. the consecutive-skip threshold reaches the RUNNER ------------------
# This is the #5244 ① defect proper: the old code recorded the skip and then
# returned 0, so nothing downstream ever fired. Assert all three levels.
reset_state
rc=0
_post_deploy_smoke_wedge_skip "settle resample unavailable" > /dev/null || rc=$?
[ "$rc" -eq 0 ] || fail_test "T2: skip 1/3 must return 0, got rc=$rc"
rc=0
_post_deploy_smoke_wedge_skip "settle resample unavailable" > /dev/null || rc=$?
[ "$rc" -eq 0 ] || fail_test "T2: skip 2/3 must return 0, got rc=$rc"
rc=0
_post_deploy_smoke_wedge_skip "settle resample unavailable" > /dev/null || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T2: skip 3/3 reached the threshold and must return nonzero"
fi
if [ "${#POST_DEPLOY_SMOKE_FAILURES[@]}" -eq 0 ]; then
    fail_test "T2: reaching the threshold must record a smoke finding"
fi

# The wedge check itself must propagate that, and so must the runner. `{}` has
# no boolean fully_recovered, which is skip site #1.
reset_state
seed_counter 2
printf '{}\n' > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
rc=0
_post_deploy_smoke_check_wedges > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T2: _post_deploy_smoke_check_wedges must return nonzero at the skip threshold"
fi

reset_state
seed_counter 2
printf '{}\n' > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
# The other three checks are out of scope here; stub them so the runner's
# wiring (`failed=1` on a nonzero wedge check) is what is under test.
_post_deploy_smoke_probe_apis() {
    POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$TMP_ROOT/health-detail.json"
    return 0
}
_post_deploy_smoke_check_fail_closed_warn_rate() { return 0; }
_post_deploy_smoke_check_relay_round_trip() { return 0; }
rc=0
_run_post_deploy_functional_smoke > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T2: _run_post_deploy_functional_smoke must return nonzero when the wedge check does"
fi
POST_DEPLOY_SMOKE_TMP_DIR="$TMP_ROOT/smoke-tmp"
mkdir -p "$POST_DEPLOY_SMOKE_TMP_DIR"

# --- T3. reaching an evidence-based verdict RESETS the streak ---------------
reset_state
seed_counter 2
health_detail_with_mailbox 777 0
rc=0
_post_deploy_smoke_check_wedges > /dev/null 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T3: a clean evidence-based verdict must return 0, got rc=$rc"
fi
if [ "$(read_counter)" != "0" ]; then
    fail_test "T3: reaching a verdict must reset the streak to 0, got '$(read_counter)'"
fi

# --- T4. durable axis positive: in_memory 500 > durable 100 -> ONE marker ---
reset_state
markers=$(durable_axis_markers 777 500 '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}')
if [ "$(count_lines "$markers")" != "1" ]; then
    fail_test "T4: in_memory 500 > durable 100 must emit exactly one marker, got: '$markers'"
fi
case "$markers" in
    *durable-axis*) : ;;
    *) fail_test "T4: the emitted marker must identify the durable axis, got: '$markers'" ;;
esac

# --- T5. idle channel negative: no in-flight, no record -> NO marker --------
# This is the assertion that keeps the rejected predicate (a) out.
reset_state
markers=$(durable_axis_markers 777 500 none)
if [ -n "$markers" ]; then
    fail_test "T5: an idle channel with no delivery record must emit no marker, got: '$markers'"
fi

# --- T6. live-measured shape: durable 1156590 > in_memory 43079 -> none -----
# The raw two-sided comparison in the issue body false-positived on 6 of 6 live
# mailboxes with exactly this shape; only the one direction is flagged.
reset_state
markers=$(durable_axis_markers 777 43079 '{"range":[0,1156590],"generation_mtime_ns":1,"attempts":1}')
if [ -n "$markers" ]; then
    fail_test "T6: durable ahead of in_memory must NOT be flagged, got: '$markers'"
fi

# --- T7. in_memory 0, durable 38093 -> no marker ----------------------------
reset_state
markers=$(durable_axis_markers 777 0 '{"range":[0,38093],"generation_mtime_ns":1,"attempts":1}')
if [ -n "$markers" ]; then
    fail_test "T7: in_memory 0 vs durable 38093 must NOT be flagged, got: '$markers'"
fi

# A null/missing delivered_frontier is a VALID record (`Option` with
# `#[serde(default)]`). jq orders null below every number, so an unguarded
# comparison makes `0 > null` true — an unknown frontier must not read as zero.
reset_state
markers=$(durable_axis_markers 777 0 'null')
if [ -n "$markers" ]; then
    fail_test "T7: a null delivered_frontier must be unknown, not zero, got: '$markers'"
fi
reset_state
markers=$(durable_axis_markers 777 500 'null')
if [ -n "$markers" ]; then
    fail_test "T7: a null delivered_frontier must be unknown even with a live offset, got: '$markers'"
fi

# A mailbox with no offset-authority channel cannot be resolved to a record
# file, so it is left unevaluated — stated in the non-guarantee list.
reset_state
health_detail_with_mailbox null 500
mkdir -p "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude"
printf '{"delivered_frontier":{"range":[0,100],"generation_mtime_ns":1,"attempts":1}}\n' \
    > "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude/null.json"
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T7: a null watcher_owner_channel_id must be left unevaluated, got: '$markers'"
fi

# --- T8. migration ratchet: every known wedge skip branch uses the helper ---
# NOT a permanent invariant. 8 means "the eight wedge branches known at #5244
# were all converted to the accounting helper". Consolidating branches (or
# adding one) legitimately changes this number — update it deliberately and say
# in the commit why, rather than treating a diff here as a regression.
skip_sites=$(grep -c '_post_deploy_smoke_wedge_skip "' "$DEPLOY_SH" || true)
if [ "$skip_sites" != "8" ]; then
    fail_test "T8: expected 8 accounted wedge skip sites, found $skip_sites (intentional? update this ratchet)"
fi

# --- T9. state corruption: recover once, fail on a STREAK -------------------
reset_state
printf 'not json at all\n' > "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
rc=0
_post_deploy_smoke_wedge_skip "settle resample body empty" > /dev/null 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T9: a single unreadable state file must recover to 0, got rc=$rc"
fi
if ! grep -q 'recovering the counter to 0' "$POST_DEPLOY_SMOKE_EVIDENCE"; then
    fail_test "T9: the one-shot recovery must be recorded in the evidence log"
fi
if [ "$(read_counter)" != "1" ]; then
    fail_test "T9: recovery must still record this deploy's skip, got '$(read_counter)'"
fi
# Corrupt it again: a permanently broken state file would otherwise restart the
# streak at 0 on every deploy and never reach the threshold.
printf 'not json at all\n' > "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
rc=0
_post_deploy_smoke_wedge_skip "settle resample body empty" > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T9: a second consecutive unreadable state file must return nonzero"
fi

# --- T10. no jq is an ACCOUNTING failure, not one more skip -----------------
reset_state
mkdir -p "$TMP_ROOT/nobin"
rc=0
(
    # shellcheck disable=SC2123  # an unusable PATH IS the condition under test
    PATH="$TMP_ROOT/nobin"
    _post_deploy_smoke_wedge_skip "startup recovery state unavailable"
) > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T10: an unusable jq must fail the check, not degrade to another silent skip"
fi

# --- T14. the marker string is sensitive to BOTH offsets --------------------
# The settle resample intersects marker strings with `comm -12`. If the marker
# dropped the offsets, an advancing relay would produce a byte-identical string
# across both samples and be reported as a persistent wedge.
reset_state
marker_a=$(durable_axis_markers 777 500 '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}')
reset_state
marker_b=$(durable_axis_markers 777 501 '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}')
reset_state
marker_c=$(durable_axis_markers 777 500 '{"range":[0,101],"generation_mtime_ns":1,"attempts":1}')
for pair in "500:100:$marker_a" "501:100:$marker_b" "500:101:$marker_c"; do
    in_mem="${pair%%:*}"
    rest="${pair#*:}"
    dur="${rest%%:*}"
    text="${rest#*:}"
    case "$text" in
        *"in_memory=${in_mem}"*) : ;;
        *) fail_test "T14: the marker must carry in_memory=${in_mem}, got: '$text'" ;;
    esac
    case "$text" in
        *"durable=${dur}"*) : ;;
        *) fail_test "T14: the marker must carry durable=${dur}, got: '$text'" ;;
    esac
done
if [ "$marker_a" = "$marker_b" ]; then
    fail_test "T14: changing the in-memory offset must change the marker string"
fi
if [ "$marker_a" = "$marker_c" ]; then
    fail_test "T14: changing the durable offset must change the marker string"
fi

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_deploy_smoke_wedge_durable_axis_5244: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_deploy_smoke_wedge_durable_axis_5244: all assertions passed"
