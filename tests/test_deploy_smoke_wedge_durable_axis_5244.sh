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
# Two of these assertions exist because the FIX reproduced that same shape:
#   - the durable axis put the authority channel id through jq arithmetic, which
#     mangles Discord snowflakes, so the detector could never resolve a real
#     record. Every fixture below therefore uses a real 19-digit snowflake; a
#     3-digit id is exact as a double and cannot see the defect.
#   - the skip counter was reset before settle/resample could decide, so every
#     late skip branch reset the streak and set it back to 1 forever. The
#     repeated-deploy assertion drives one of those branches N times; a
#     call-count ratchet cannot see that.
#
# Predicate coverage note: only ONE durable predicate exists, `in_memory >
# durable`. The rejected companion ("in-flight present and no delivery record")
# is asserted-against by the IN-FLIGHT idle-channel negative below; delivery
# records are created only after a Delivered outcome, so an ordinary first turn
# sits in exactly that state for minutes and must not be flagged.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-smoke-wedge-5244.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

# Live Discord authority channel ids, measured on the release node. Snowflakes
# are ~1.5e18 and jq numbers are IEEE-754 doubles, so any jq arithmetic on these
# loses the low digits (measured with jq-1.7.1-apple: `floor|tostring` returned
# 1479671298497183700 for the first id below).
OWNER_SNOWFLAKE=1479671298497183835
MAILBOX_SNOWFLAKE=1509350490461180105
OTHER_SNOWFLAKE=1528150941151264878

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
eval "$(extract_function _post_deploy_smoke_wedge_durable_unevaluable)"
eval "$(extract_function _post_deploy_smoke_wedge_durable_axis_markers_from_file)"
eval "$(extract_function _post_deploy_smoke_wedge_all_markers_from_file)"
eval "$(extract_function _post_deploy_smoke_fully_recovered_from_file)"
eval "$(extract_function _post_deploy_smoke_wedge_await_recovery)"
eval "$(extract_function _post_deploy_smoke_wedge_unevaluable_summary)"
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
POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE="$POST_DEPLOY_SMOKE_TMP_DIR/wedge_durable_unevaluable.log"
# The production functions loaded through eval consume these test globals, so
# the linter cannot see the uses; exporting them states the contract. An array
# cannot be exported, hence the targeted directive below.
export ADK_REL REL_PORT ADK_DEFAULT_LOOPBACK POST_DEPLOY_SMOKE_STAMP
export POST_DEPLOY_SMOKE_EVIDENCE POST_DEPLOY_SMOKE_TMP_DIR
export POST_DEPLOY_SMOKE_WEDGE_SETTLE_SECS POST_DEPLOY_SMOKE_WEDGE_RECOVERY_WAIT_SECS
export POST_DEPLOY_SMOKE_WEDGE_RECOVERY_POLL_SECS POST_DEPLOY_SMOKE_WEDGE_MAX_CONSECUTIVE_SKIPS
export POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE POST_DEPLOY_SMOKE_WEDGE_SKIP_CORRUPT_TALLY
export POST_DEPLOY_SMOKE_WEDGE_SKIP_MAX_CORRUPTIONS POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR
export POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE
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
    # _post_deploy_smoke_check_wedges points the unevaluable log at the smoke
    # run's own tmp dir, and _run_post_deploy_functional_smoke below makes (and
    # removes) its own; re-pin both to this suite's dir every time.
    POST_DEPLOY_SMOKE_TMP_DIR="$TMP_ROOT/smoke-tmp"
    POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE="$POST_DEPLOY_SMOKE_TMP_DIR/wedge_durable_unevaluable.log"
    mkdir -p "$POST_DEPLOY_SMOKE_TMP_DIR"
    rm -rf "${POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE:?}" "${POST_DEPLOY_SMOKE_WEDGE_SKIP_CORRUPT_TALLY:?}"
    rm -rf "${POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR:?}"
    POST_DEPLOY_SMOKE_FAILURES=()
    : > "$POST_DEPLOY_SMOKE_EVIDENCE"
    : > "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE"
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
# $3 (`inflight`) defaults to false; the idle-channel negative sets it TRUE,
# because the rejected predicate (a) is gated on it and a false-only fixture
# could not tell whether (a) was reinstated.
health_detail_with_mailbox() {
    local owner_channel="$1" in_memory="$2"
    local inflight="${3:-false}" mailbox_channel="${4:-$MAILBOX_SNOWFLAKE}"
    cat > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" <<EOF
{
  "fully_recovered": true,
  "mailboxes": [
    {
      "provider": "claude",
      "channel_id": ${mailbox_channel},
      "watcher_attached": true,
      "inflight_state_present": ${inflight},
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

write_raw_record() {
    local owner_channel="$1" body="$2"
    mkdir -p "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude"
    printf '%s\n' "$body" > "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude/${owner_channel}.json"
}

durable_axis_markers() {
    local owner_channel="$1" in_memory="$2" frontier_json="$3" inflight="${4:-false}"
    health_detail_with_mailbox "$owner_channel" "$in_memory" "$inflight"
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

# --- T3. reaching an evidence-based verdict RESETS the streak ---------------
reset_state
seed_counter 2
health_detail_with_mailbox "$OWNER_SNOWFLAKE" 0
rc=0
_post_deploy_smoke_check_wedges > /dev/null 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T3: a clean evidence-based verdict must return 0, got rc=$rc"
fi
if [ "$(read_counter)" != "0" ]; then
    fail_test "T3: reaching a verdict must reset the streak to 0, got '$(read_counter)'"
fi

# --- T3b. a LATE skip branch must still reach the threshold across deploys --
# The reset used to sit right after the first marker sample, so every branch
# after it reset the streak and then bumped it back to 1 — forever. Simulate
# consecutive deploys through the "settle resample unavailable" branch (the
# resample curl cannot connect to REL_PORT=0) and require the threshold to fire.
# A call-count ratchet over the source cannot detect this; only running the
# check repeatedly can.
reset_state
health_detail_with_mailbox "$OWNER_SNOWFLAKE" 500
write_delivery_record "$OWNER_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
late_skip_threshold_hit=0
observed_counts=""
for deploy in 1 2 3 4; do
    POST_DEPLOY_SMOKE_FAILURES=()
    rc=0
    _post_deploy_smoke_check_wedges > /dev/null 2>&1 || rc=$?
    observed_counts="${observed_counts}${observed_counts:+,}deploy${deploy}=rc${rc}/count$(read_counter)"
    if [ "$rc" -ne 0 ]; then
        late_skip_threshold_hit=1
        break
    fi
done
if [ "$late_skip_threshold_hit" -ne 1 ]; then
    fail_test "T3b: repeated deploys through a late skip branch never reached the threshold ($observed_counts)"
fi

# --- T4. durable axis positive: in_memory 500 > durable 100 -> ONE marker ---
reset_state
markers=$(durable_axis_markers "$OWNER_SNOWFLAKE" 500 '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}')
if [ "$(count_lines "$markers")" != "1" ]; then
    fail_test "T4: in_memory 500 > durable 100 must emit exactly one marker, got: '$markers'"
fi
case "$markers" in
    *durable-axis*) : ;;
    *) fail_test "T4: the emitted marker must identify the durable axis, got: '$markers'" ;;
esac
# The record file is addressed by the authority id. If any jq arithmetic touches
# it, the id in the marker is not the id in the fixture and the record path was
# never resolvable in the first place.
case "$markers" in
    *"owner_channel=${OWNER_SNOWFLAKE}"*) : ;;
    *) fail_test "T4: the marker must carry the exact authority snowflake ${OWNER_SNOWFLAKE}, got: '$markers'" ;;
esac
if [ -s "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE" ]; then
    fail_test "T4: a fully resolvable mailbox must not be logged as unevaluable: $(cat "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE")"
fi

# --- T5. idle IN-FLIGHT channel negative: no record -> NO marker ------------
# This is the assertion that keeps the rejected predicate (a) out. It only has
# that force with inflight_state_present=true, which is (a)'s own gate.
reset_state
markers=$(durable_axis_markers "$OWNER_SNOWFLAKE" 500 none true)
if [ -n "$markers" ]; then
    fail_test "T5: an in-flight channel with no delivery record must emit no marker, got: '$markers'"
fi
if [ -s "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE" ]; then
    fail_test "T5: an absent delivery record is an ordinary state, not an unevaluable one"
fi

# --- T6. live-measured shape: durable 1156590 > in_memory 43079 -> none -----
# The raw two-sided comparison in the issue body false-positived on 6 of 6 live
# mailboxes with exactly this shape; only the one direction is flagged.
reset_state
markers=$(durable_axis_markers "$OWNER_SNOWFLAKE" 43079 '{"range":[0,1156590],"generation_mtime_ns":1,"attempts":1}')
if [ -n "$markers" ]; then
    fail_test "T6: durable ahead of in_memory must NOT be flagged, got: '$markers'"
fi

# --- T7. in_memory 0, durable 38093 -> no marker ----------------------------
reset_state
markers=$(durable_axis_markers "$OWNER_SNOWFLAKE" 0 '{"range":[0,38093],"generation_mtime_ns":1,"attempts":1}')
if [ -n "$markers" ]; then
    fail_test "T7: in_memory 0 vs durable 38093 must NOT be flagged, got: '$markers'"
fi

# A null/missing delivered_frontier is a VALID record (`Option` with
# `#[serde(default)]`). jq orders null below every number, so an unguarded
# comparison makes `0 > null` true — an unknown frontier must not read as zero.
# It is also NOT a schema violation: no marker AND no unevaluable entry.
for offset in 0 500; do
    reset_state
    markers=$(durable_axis_markers "$OWNER_SNOWFLAKE" "$offset" 'null')
    if [ -n "$markers" ]; then
        fail_test "T7: a null delivered_frontier must be unknown, not zero (offset=$offset), got: '$markers'"
    fi
    if [ -s "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE" ]; then
        fail_test "T7: a null delivered_frontier is a valid record, not an unevaluable one (offset=$offset)"
    fi
    # `{}` as the WHOLE record: `delivered_frontier` is `Option` +
    # `#[serde(default)]`, so the field being absent is a valid record. (A
    # present-but-empty `{"delivered_frontier":{}}` is a different thing —
    # `DeliveredCommit.range` is mandatory — and is covered by T16.)
    reset_state
    health_detail_with_mailbox "$OWNER_SNOWFLAKE" "$offset"
    write_raw_record "$OWNER_SNOWFLAKE" '{}'
    markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
    if [ -n "$markers" ]; then
        fail_test "T7: an absent delivered_frontier must be unknown (offset=$offset), got: '$markers'"
    fi
    if [ -s "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE" ]; then
        fail_test "T7: an absent delivered_frontier is a valid record, not an unevaluable one (offset=$offset)"
    fi
done

# --- T15. a NULL authority falls back to the delivery channel ---------------
# Production resolves the offset authority with a fallback (tmux.rs:250-253,
# idle_recap.rs:391). Half the live mailboxes (3 of 6) have a null authority, so
# leaving them unevaluated silently drops half the coverage.
reset_state
health_detail_with_mailbox null 500
write_delivery_record "$MAILBOX_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ "$(count_lines "$markers")" != "1" ]; then
    fail_test "T15: a null authority must fall back to the delivery channel record, got: '$markers'"
fi
case "$markers" in
    *"owner_channel=${MAILBOX_SNOWFLAKE}"*) : ;;
    *) fail_test "T15: the fallback must address the delivery channel ${MAILBOX_SNOWFLAKE}, got: '$markers'" ;;
esac
# The record written under the OTHER snowflake must not be the one that matched.
reset_state
health_detail_with_mailbox null 500
write_delivery_record "$OTHER_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T15: the fallback must not match an unrelated channel's record, got: '$markers'"
fi

# A ZERO authority is NOT a fallback: opt_channel_id (inflight/model.rs:28-36)
# maps 0 to None. It is unevaluable, and unevaluable is never silent.
reset_state
health_detail_with_mailbox 0 500
write_delivery_record "$MAILBOX_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T15: a zero authority must not be retargeted to the delivery channel, got: '$markers'"
fi
if ! grep -q 'zero' "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE"; then
    fail_test "T15: a zero authority must be logged as unevaluable, log: '$(cat "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE")'"
fi

# --- T16. schema violations are reported, never read as clean ---------------
# Each of these used to pass with no marker, no error, and — because the reset
# sat before the settle branch — a counter reset on top.
schema_case() {
    local label="$1" body="$2"
    reset_state
    health_detail_with_mailbox "$OWNER_SNOWFLAKE" 500
    write_raw_record "$OWNER_SNOWFLAKE" "$body"
    markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
    if [ -n "$markers" ]; then
        fail_test "T16/$label: a malformed record must not emit a wedge marker, got: '$markers'"
    fi
    if [ ! -s "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE" ]; then
        fail_test "T16/$label: a malformed record must be recorded as unevaluable"
    fi
}
schema_case "not-json" 'this is not json'
schema_case "range-is-string" '{"delivered_frontier":{"range":"0-100","generation_mtime_ns":1,"attempts":1}}'
schema_case "range-too-short" '{"delivered_frontier":{"range":[7],"generation_mtime_ns":1,"attempts":1}}'
schema_case "range-end-not-number" '{"delivered_frontier":{"range":[0,"100"],"generation_mtime_ns":1,"attempts":1}}'
schema_case "frontier-not-object" '{"delivered_frontier":42}'
schema_case "frontier-without-range" '{"delivered_frontier":{"generation_mtime_ns":1,"attempts":1}}'

# A record path that exists but is not a regular file is NOT the same thing as
# no record at all: `[ ! -f ]` alone would collapse the two into one silence.
reset_state
health_detail_with_mailbox "$OWNER_SNOWFLAKE" 500
mkdir -p "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude/${OWNER_SNOWFLAKE}.json"
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T16/record-is-dir: a non-regular record path must not emit a marker, got: '$markers'"
fi
if ! grep -q 'not a regular file' "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE"; then
    fail_test "T16/record-is-dir: a non-regular record path must be unevaluable, not read as 'no record'"
fi

# A missing last_relay_offset is the health-side equivalent.
reset_state
cat > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" <<EOF
{
  "fully_recovered": true,
  "mailboxes": [
    {
      "provider": "claude",
      "channel_id": ${MAILBOX_SNOWFLAKE},
      "watcher_attached": true,
      "inflight_state_present": false,
      "relay_stall_state": "healthy",
      "relay_owner_kind": "watcher",
      "relay_health": {
        "watcher_owner_channel_id": ${OWNER_SNOWFLAKE},
        "desynced": false,
        "stale_thread_proof": false,
        "watcher_attached_stale": false
      }
    }
  ]
}
EOF
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T16/no-offset: a missing last_relay_offset must not emit a marker, got: '$markers'"
fi
if ! grep -q 'last_relay_offset' "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE"; then
    fail_test "T16/no-offset: a missing last_relay_offset must be recorded as unevaluable"
fi

# --- T17. an unevaluable reading blocks the CLEAN verdict -------------------
# Otherwise the durable axis contributes nothing while still reporting a pass —
# the exact shape #5244 is about.
reset_state
seed_counter 1
health_detail_with_mailbox "$OWNER_SNOWFLAKE" 500
write_raw_record "$OWNER_SNOWFLAKE" 'this is not json'
rc=0
_post_deploy_smoke_check_wedges > /dev/null 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T17: one unevaluable reading is advisory on its own, got rc=$rc"
fi
if [ "$(read_counter)" != "2" ]; then
    fail_test "T17: an unevaluable reading must count as a skip, not reset the streak, got '$(read_counter)'"
fi

# --- T8. migration ratchet: every known wedge skip branch uses the helper ---
# NOT a permanent invariant. 10 means "the eight wedge branches known at #5244,
# plus the two that route an unevaluable durable reading away from a clean
# verdict, all go through the accounting helper". Consolidating branches (or
# adding one) legitimately changes this number — update it deliberately and say
# in the commit why, rather than treating a diff here as a regression.
skip_sites=$(grep -cE '_post_deploy_smoke_wedge_skip[ "\\]' "$DEPLOY_SH" || true)
if [ "$skip_sites" != "10" ]; then
    fail_test "T8: expected 10 accounted wedge skip sites, found $skip_sites (intentional? update this ratchet)"
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

# --- T18. a non-regular state path must FAIL the write, not fake success ----
# `mv f d/` where `d` is a directory returns 0 and moves the file INSIDE it, so
# the authoritative path never changes and the streak stays pinned at 1.
reset_state
mkdir -p "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
rc=0
_post_deploy_smoke_wedge_skip "settle resample unavailable" > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T18: a directory at the state path must fail the skip write, got rc=$rc"
fi
reset_state
ln -s /dev/null "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"
rc=0
_post_deploy_smoke_wedge_skip "settle resample unavailable" > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T18: a symlinked state path must fail the skip write, got rc=$rc"
fi
rm -f "$POST_DEPLOY_SMOKE_WEDGE_SKIP_STATE"

# --- T19. an unevaluable reading also blocks the POST-SETTLE clean verdict --
# T17 covers the "no marker in the first sample" verdict. This one covers
# "marker cleared across the settle resample", which is only reachable with a
# resample that succeeds — hence the curl stub.
two_mailbox_body() {
    local out="$1" in_memory_a="$2"
    cat > "$out" <<EOF
{
  "fully_recovered": true,
  "mailboxes": [
    {
      "provider": "claude", "channel_id": ${MAILBOX_SNOWFLAKE},
      "watcher_attached": true, "inflight_state_present": false,
      "relay_stall_state": "healthy", "relay_owner_kind": "watcher",
      "relay_health": {
        "watcher_owner_channel_id": ${OWNER_SNOWFLAKE},
        "last_relay_offset": ${in_memory_a},
        "desynced": false, "stale_thread_proof": false, "watcher_attached_stale": false
      }
    },
    {
      "provider": "claude", "channel_id": ${OTHER_SNOWFLAKE},
      "watcher_attached": true, "inflight_state_present": false,
      "relay_stall_state": "healthy", "relay_owner_kind": "watcher",
      "relay_health": {
        "watcher_owner_channel_id": ${OTHER_SNOWFLAKE},
        "last_relay_offset": 10,
        "desynced": false, "stale_thread_proof": false, "watcher_attached_stale": false
      }
    }
  ]
}
EOF
}

reset_state
seed_counter 1
# Mailbox A is a real marker in the first sample and clears in the resample;
# mailbox B is unreadable in both.
two_mailbox_body "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" 500
two_mailbox_body "$TMP_ROOT/resample-body.json" 50
write_delivery_record "$OWNER_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
write_raw_record "$OTHER_SNOWFLAKE" 'not json'
rc=0
(
    # Stands in for the settle resample fetch; only the -o destination matters.
    curl() {
        local out=""
        while [ "$#" -gt 0 ]; do
            case "$1" in
                -o) out="$2"; shift 2 ;;
                *) shift ;;
            esac
        done
        [ -n "$out" ] || return 1
        cp "$TMP_ROOT/resample-body.json" "$out"
    }
    _post_deploy_smoke_check_wedges
) > /dev/null 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T19: one unevaluable reading after settle is advisory on its own, got rc=$rc"
fi
if [ "$(read_counter)" != "2" ]; then
    fail_test "T19: 'markers cleared' with an unevaluable mailbox must skip, not reset, got '$(read_counter)'"
fi

# Same shape with NO unevaluable mailbox must reach the cleared verdict and reset.
reset_state
seed_counter 1
two_mailbox_body "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" 500
two_mailbox_body "$TMP_ROOT/resample-body.json" 50
write_delivery_record "$OWNER_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
write_delivery_record "$OTHER_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
rc=0
(
    curl() {
        local out=""
        while [ "$#" -gt 0 ]; do
            case "$1" in
                -o) out="$2"; shift 2 ;;
                *) shift ;;
            esac
        done
        [ -n "$out" ] || return 1
        cp "$TMP_ROOT/resample-body.json" "$out"
    }
    _post_deploy_smoke_check_wedges
) > /dev/null 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T19: a marker that clears across settle must return 0, got rc=$rc"
fi
if [ "$(read_counter)" != "0" ]; then
    fail_test "T19: the cleared verdict must reset the streak, got '$(read_counter)'"
fi

# --- T14. the marker string is sensitive to BOTH offsets --------------------
# The settle resample intersects marker strings with `comm -12`. If the marker
# dropped the offsets, an advancing relay would produce a byte-identical string
# across both samples and be reported as a persistent wedge.
reset_state
marker_a=$(durable_axis_markers "$OWNER_SNOWFLAKE" 500 '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}')
reset_state
marker_b=$(durable_axis_markers "$OWNER_SNOWFLAKE" 501 '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}')
reset_state
marker_c=$(durable_axis_markers "$OWNER_SNOWFLAKE" 500 '{"range":[0,101],"generation_mtime_ns":1,"attempts":1}')
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

# --- T20. the TOP-LEVEL health/detail schema is checked, not assumed --------
# T16 covers a malformed delivery RECORD. These are one level up: on every body
# below both marker passes iterate nothing through their `[]?` and report no
# marker, which the caller used to read as an evaluated clean verdict and reset
# the streak on. "No output" is not "evaluated, nothing found" — that IS #5244.
top_level_case() {
    local label="$1" body="$2"
    reset_state
    printf '%s\n' "$body" > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
    # A record that WOULD prove a wedge if any mailbox were readable, so a pass
    # here cannot be explained by "there was nothing to find".
    write_delivery_record "$OWNER_SNOWFLAKE" '{"range":[0,100],"generation_mtime_ns":1,"attempts":1}'
    markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
    if [ -n "$markers" ]; then
        fail_test "T20/$label: a malformed top-level body must not emit a wedge marker, got: '$markers'"
    fi
    if [ ! -s "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE" ]; then
        fail_test "T20/$label: a malformed top-level body must be recorded as unevaluable"
    fi
}
top_level_case "mailboxes-absent" '{"fully_recovered":true,"degraded_reasons":[]}'
top_level_case "mailboxes-null" '{"fully_recovered":true,"mailboxes":null}'
top_level_case "mailboxes-string" '{"fully_recovered":true,"mailboxes":"none"}'
top_level_case "mailboxes-number" '{"fully_recovered":true,"mailboxes":0}'
top_level_case "mailboxes-object" '{"fully_recovered":true,"mailboxes":{"a":null}}'
top_level_case "mailbox-entry-null" '{"fully_recovered":true,"mailboxes":[null]}'
top_level_case "degraded-reasons-string" \
    '{"fully_recovered":true,"degraded_reasons":"relay wedge","mailboxes":[]}'
# A non-null, non-object mailbox ENTRY (string/number/array) is not in the list
# above because it does not reach this classification: the in-memory pass
# indexes it first and jq errors, so the whole extraction returns nonzero and
# the caller raises a hard FAIL. Only `null` is absorbed silently, hence the
# entry case above. Measured: `"mailboxes":["claude"]` gives extraction rc=1.
reset_state
printf '%s\n' '{"fully_recovered":true,"mailboxes":["claude"]}' \
    > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
rc=0
_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY" \
    > /dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "T20/entry-string: an unindexable mailbox entry must not extract cleanly"
fi

# An EMPTY mailbox list in a well-formed body is not a schema violation: a node
# with no active relay channel is ordinary, and classifying it unevaluable
# would drive every such deploy to the skip threshold.
reset_state
printf '%s\n' '{"fully_recovered":true,"degraded_reasons":[],"mailboxes":[]}' \
    > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T20/empty-ok: an empty mailbox list must not emit a marker, got: '$markers'"
fi
if [ -s "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE" ]; then
    fail_test "T20/empty-ok: an empty mailbox list must stay EVALUABLE, log: '$(cat "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE")'"
fi

# ... and end-to-end it blocks the clean verdict, exactly like T17.
reset_state
seed_counter 1
printf '%s\n' '{"fully_recovered":true,"mailboxes":null}' \
    > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
rc=0
_post_deploy_smoke_check_wedges > /dev/null 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "T20: one unevaluable top-level body is advisory on its own, got rc=$rc"
fi
if [ "$(read_counter)" != "2" ]; then
    fail_test "T20: a malformed top-level body must count as a skip, not reset, got '$(read_counter)'"
fi

# --- T21. a SYMLINKED delivery record is unevaluable, never authority -------
# The relay writes records by rename, never as a link. `-e`/`-f` follow links,
# so this is the read-side twin of the state writer's destination guard (T18).
reset_state
health_detail_with_mailbox "$OWNER_SNOWFLAKE" 500
mkdir -p "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude"
printf '{"delivered_frontier":{"range":[0,100],"generation_mtime_ns":1,"attempts":1}}\n' \
    > "$TMP_ROOT/planted-record.json"
ln -s "$TMP_ROOT/planted-record.json" \
    "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude/${OWNER_SNOWFLAKE}.json"
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T21: a symlinked record must not be read as the authority's own, got: '$markers'"
fi
if ! grep -q 'symlink' "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE"; then
    fail_test "T21: a symlinked record path must be recorded as unevaluable"
fi

# A DANGLING link is the worse half: `-e` is false, so without the check it
# lands in the ordinary "no record yet" branch and reads as clean.
reset_state
health_detail_with_mailbox "$OWNER_SNOWFLAKE" 500
mkdir -p "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude"
ln -s "$TMP_ROOT/there-is-no-such-record.json" \
    "$POST_DEPLOY_SMOKE_DELIVERY_RECORDS_DIR/claude/${OWNER_SNOWFLAKE}.json"
markers=$(_post_deploy_smoke_wedge_all_markers_from_file "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY")
if [ -n "$markers" ]; then
    fail_test "T21/dangling: a dangling record symlink must not emit a marker, got: '$markers'"
fi
if ! grep -q 'symlink' "$POST_DEPLOY_SMOKE_WEDGE_UNEVALUABLE"; then
    fail_test "T21/dangling: a dangling record symlink must be unevaluable, not 'no record yet'"
fi

# --- T22. jq older than 1.7 is an ACCOUNTING failure, not a silent read -----
# Preserving a number's source literal is a jq 1.7 feature. Up to 1.6 the
# IEEE-754 conversion happens at parse time, so `.id | tostring` ALONE mangles
# a snowflake, the mangled id addresses a record path that cannot exist, and
# the durable axis takes the ordinary "no record yet" branch — a clean verdict
# on a relay it never read. `--all-nodes` runs this script on peers whose jq is
# an independent install, so the version is not knowable from the deploy node.
REAL_JQ="$(command -v jq)"
stub_jq_version() {
    local dir="$1" reported="$2"
    mkdir -p "$dir"
    cat > "$dir/jq" <<EOF
#!/bin/sh
if [ "\$1" = "--version" ]; then
    printf '%s\n' "$reported"
    exit 0
fi
exec "$REAL_JQ" "\$@"
EOF
    chmod +x "$dir/jq"
}

for reported in "jq-1.6" "jq-1.5" "jq-0.9" "jq-1" "jq-" "not-a-version" ""; do
    reset_state
    stub_jq_version "$TMP_ROOT/jqstub" "$reported"
    rc=0
    (
        PATH="$TMP_ROOT/jqstub:$PATH"
        _post_deploy_smoke_wedge_skip "startup recovery state unavailable"
    ) > /dev/null 2>&1 || rc=$?
    if [ "$rc" -eq 0 ]; then
        fail_test "T22: jq version '$reported' must fail the check, not degrade to a silent skip"
    fi
done

# The gate must not reject the versions that ARE safe, or it fails every
# deploy instead — a fail-closed gate still has to let the good case through.
for reported in "jq-1.7" "jq-1.7.1" "jq-1.7.1-apple" "jq-1.8.0" "jq-2.0"; do
    reset_state
    stub_jq_version "$TMP_ROOT/jqstub" "$reported"
    rc=0
    (
        PATH="$TMP_ROOT/jqstub:$PATH"
        _post_deploy_smoke_wedge_skip "startup recovery state unavailable"
    ) > /dev/null 2>&1 || rc=$?
    if [ "$rc" -ne 0 ]; then
        fail_test "T22: jq version '$reported' must be accepted, got rc=$rc"
    fi
done
rm -rf "$TMP_ROOT/jqstub"

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_deploy_smoke_wedge_durable_axis_5244: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_deploy_smoke_wedge_durable_axis_5244: all assertions passed"
