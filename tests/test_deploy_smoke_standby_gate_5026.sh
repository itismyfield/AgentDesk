#!/usr/bin/env bash
# Regression test for #5026: the post-deploy relay round-trip must actually run
# on a healthy (non-standby) node.
#
# `jq -e` sets exit status 1 whenever the last output value is `false` or
# `null`, and `false` is exactly the healthy non-standby value of
# `.cluster_standby`. The previous `jq -er` standby gate therefore reported
# "could not prove node is non-standby" on every normal node even though it had
# extracted the flag correctly. Combined with the deliberate skip on
# `cluster_standby=true`, no input value could reach the round-trip at all: the
# relay check was unreachable on every deploy and silently caught nothing.
#
# The defect is "a gate that quietly never ran", so this suite asserts the
# positive: on a non-standby node the round-trip path is actually entered.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_SH="$REPO_ROOT/scripts/deploy-release.sh"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-smoke-standby-test.XXXXXX")
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
eval "$(extract_function _post_deploy_smoke_resolve_cluster_standby)"
eval "$(extract_function _post_deploy_smoke_check_relay_round_trip)"

POST_DEPLOY_SMOKE_EVIDENCE="$TMP_ROOT/evidence.log"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
ADK_REL="$TMP_ROOT/release"
REPO="$REPO_ROOT"
POST_DEPLOY_SMOKE_RELAY_CELL="claude/adk-cc"
POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY="$TMP_ROOT/health-detail.json"
POST_DEPLOY_SMOKE_SESSIONS_BODY="$TMP_ROOT/sessions.json"
POST_DEPLOY_SMOKE_HEALTH_BODY=""
# The production functions loaded through eval consume these test globals, so
# the linter cannot see the uses; exporting them states the contract. An array
# cannot be exported, hence the targeted directive below.
export POST_DEPLOY_SMOKE_EVIDENCE ADK_REL REPO POST_DEPLOY_SMOKE_RELAY_CELL
export POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY POST_DEPLOY_SMOKE_SESSIONS_BODY
export POST_DEPLOY_SMOKE_HEALTH_BODY
# shellcheck disable=SC2034  # consumed by the eval'd _post_deploy_smoke_fail
POST_DEPLOY_SMOKE_FAILURES=()
mkdir -p "$ADK_REL/config"
printf '{}\n' > "$POST_DEPLOY_SMOKE_HEALTH_DETAIL_BODY"
printf '{}\n' > "$POST_DEPLOY_SMOKE_SESSIONS_BODY"

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

health_body_with() {
    local body="$1" path="$TMP_ROOT/health.json"
    printf '%s\n' "$body" > "$path"
    printf '%s' "$path"
}

# --- 1. `false` is a legitimate VALUE, not a resolution failure -------------
# This is the exact regression: `jq -e` conflates the boolean `false` with
# "no output", so a perfectly readable non-standby node looked unreadable.
rc=0
value=$(_post_deploy_smoke_resolve_cluster_standby \
    "$(health_body_with '{"cluster_standby": false}')") || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "cluster_standby=false must resolve successfully, got rc=$rc (jq -e exit-status regression)"
elif [ "$value" != "false" ]; then
    fail_test "cluster_standby=false must resolve to 'false', got '$value'"
fi

# --- 2. `true` still resolves (standby nodes skip by design) ----------------
rc=0
value=$(_post_deploy_smoke_resolve_cluster_standby \
    "$(health_body_with '{"cluster_standby": true}')") || rc=$?
if [ "$rc" -ne 0 ]; then
    fail_test "cluster_standby=true must resolve successfully, got rc=$rc"
elif [ "$value" != "true" ]; then
    fail_test "cluster_standby=true must resolve to 'true', got '$value'"
fi

# --- 3. genuinely unreadable input is the ONLY failure mode -----------------
rc=0
_post_deploy_smoke_resolve_cluster_standby \
    "$(health_body_with '{"cluster_standby": "false"}')" >/dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "a non-boolean cluster_standby must not resolve"
fi

rc=0
_post_deploy_smoke_resolve_cluster_standby \
    "$(health_body_with '{}')" >/dev/null 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "a missing cluster_standby must not resolve"
fi

rc=0
_post_deploy_smoke_resolve_cluster_standby "$TMP_ROOT/does-not-exist.json" >/dev/null || rc=$?
if [ "$rc" -eq 0 ]; then
    fail_test "an absent health body must not resolve"
fi

# --- 4. the round-trip is actually ENTERED on a non-standby node ------------
# The downstream round-trip needs a live deploy environment and is expected to
# fail here; only the gate decision is under test, so the call runs in a
# subshell and just the emitted breadcrumbs are asserted.
POST_DEPLOY_SMOKE_HEALTH_BODY="$(health_body_with '{"cluster_standby": false}')"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
( _post_deploy_smoke_check_relay_round_trip ) > "$TMP_ROOT/roundtrip.out" 2>&1 || true

if ! grep -q 'relay E-1=round-trip proceeding' "$TMP_ROOT/roundtrip.out"; then
    fail_test "non-standby node never entered the relay round-trip; the standby gate rejected it"
fi
if grep -q 'NOT VERIFIED' "$TMP_ROOT/roundtrip.out"; then
    fail_test "non-standby node reported the standby gate as unverifiable"
fi

# --- 5. a standby node still skips without claiming a failure --------------
POST_DEPLOY_SMOKE_HEALTH_BODY="$(health_body_with '{"cluster_standby": true}')"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
( _post_deploy_smoke_check_relay_round_trip ) > "$TMP_ROOT/standby.out" 2>&1 || true

if ! grep -q 'relay E-1=skipped cluster_standby=true' "$TMP_ROOT/standby.out"; then
    fail_test "standby node must record the deliberate skip"
fi
if grep -q 'relay E-1=round-trip proceeding' "$TMP_ROOT/standby.out"; then
    fail_test "standby node must not enter the round-trip"
fi

# --- 6. unreadable standby state is reported as NOT VERIFIED, not a failure -
POST_DEPLOY_SMOKE_HEALTH_BODY="$(health_body_with '{}')"
: > "$POST_DEPLOY_SMOKE_EVIDENCE"
( _post_deploy_smoke_check_relay_round_trip ) > "$TMP_ROOT/unreadable.out" 2>&1 || true

if ! grep -q 'relay E-1 NOT VERIFIED' "$TMP_ROOT/unreadable.out"; then
    fail_test "an unreadable standby flag must be reported as NOT VERIFIED (coverage gap), distinct from a relay failure"
fi

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_deploy_smoke_standby_gate_5026: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_deploy_smoke_standby_gate_5026: all assertions passed"
