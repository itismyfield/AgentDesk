#!/usr/bin/env bash
# Pinned deploy target (AGENTDESK_DEPLOY_TARGET_SHA): leader identity/ancestry/CI gates and
# peer ff-only-to-target pre-sync, against throwaway git repos with ssh and gh stubbed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-deploy-target-sha-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

# Hermetic git: no operator config, hooks or signing can leak into the fixtures.
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid
unset AGENTDESK_DEPLOY_SKIP_FRESHNESS AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS \
    AGENTDESK_DEPLOY_BINARY AGENTDESK_DEPLOY_ALLOW_NON_MAIN AGENTDESK_DEPLOY_ALLOW_DIRTY \
    AGENTDESK_DEPLOY_TARGET_SHA AGENTDESK_PEER_REPO_DIR

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

# The top-level validation block, from its assignment to the closing `fi`.
extract_target_validation() {
    awk '
        /^DEPLOY_TARGET_SHA="\$\{AGENTDESK_DEPLOY_TARGET_SHA:-\}"$/ { printing = 1 }
        printing { print }
        printing && /^fi$/ { exit }
    ' "$DEPLOY_SH"
}

for fn in _verify_deploy_target_sha _verify_deploy_target_ci_green _check_repo_source_identity \
    _check_repo_remote_freshness _deploy_peer_env_prelude _deploy_to_one_peer; do
    body="$(extract_function "$fn")"
    if [ -z "$body" ]; then
        printf 'FAIL: could not extract %s from %s\n' "$fn" "$DEPLOY_SH" >&2
        exit 1
    fi
    eval "$body"
done
validation_block="$(extract_target_validation)"
if [ -z "$validation_block" ]; then
    printf 'FAIL: could not extract the AGENTDESK_DEPLOY_TARGET_SHA validation block from %s\n' "$DEPLOY_SH" >&2
    exit 1
fi

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

# --- fixtures: origin main A -> B -> C; leader and peer are clones -----------
ORIGIN="$TMP_ROOT/origin.git"
SEED="$TMP_ROOT/seed"
LEADER="$TMP_ROOT/leader"
PEER="$TMP_ROOT/peer"
git init --quiet --bare -b main "$ORIGIN"
git init --quiet -b main "$SEED"
for name in A B C; do
    printf '%s\n' "$name" >"$SEED/$name"
    git -C "$SEED" add "$name"
    git -C "$SEED" commit --quiet -m "$name"
done
SHA_C="$(git -C "$SEED" rev-parse HEAD)"
SHA_B="$(git -C "$SEED" rev-parse HEAD~1)"
SHA_A="$(git -C "$SEED" rev-parse HEAD~2)"
git -C "$SEED" push --quiet "$ORIGIN" main
git clone --quiet "$ORIGIN" "$LEADER"
git clone --quiet "$ORIGIN" "$PEER"
# A commit that exists on the leader but was never merged to origin/main.
git -C "$LEADER" checkout --quiet -b side "$SHA_B"
printf 'side\n' >"$LEADER/side"
git -C "$LEADER" add side
git -C "$LEADER" commit --quiet -m side
SHA_SIDE="$(git -C "$LEADER" rev-parse HEAD)"
git -C "$LEADER" checkout --quiet main

# shellcheck disable=SC2034  # Read by the production functions loaded through eval.
REPO="$LEADER"
# shellcheck disable=SC2034  # Read by the production functions loaded through eval.
DEPLOY_SSH_CONNECT_TIMEOUT=1
# shellcheck disable=SC2034  # Read by the production functions loaded through eval.
DEPLOY_PEER_INVOCATION=0

# gh stub: accepts only the exact CI Main success query for DEPLOY_TARGET_SHA and answers it
# from GH_STUB_CONCLUSIONS (all runs of that commit); GH_STUB_FAIL=1 simulates a gh failure.
GH_CALLS_FILE="$TMP_ROOT/gh-calls"
GH_STUB_CONCLUSIONS="success"
GH_STUB_FAIL=0
# shellcheck disable=SC2329  # Invoked by the production function loaded through eval.
gh() {
    printf '%s\n' "$*" >>"$GH_CALLS_FILE"
    local expected=(run list --workflow ci-main.yml --commit "$DEPLOY_TARGET_SHA"
        --status success --limit 1 --json conclusion --jq '.[].conclusion')
    if [ "$*" != "${expected[*]}" ]; then
        echo "gh stub: unexpected argv: $*" >&2
        return 1
    fi
    [ "$GH_STUB_FAIL" != "1" ] || { echo "gh: not authenticated" >&2; return 1; }
    ! grep -qx success <<<"$GH_STUB_CONCLUSIONS" || echo success
}

leader_at() {
    git -C "$LEADER" checkout --quiet main
    git -C "$LEADER" reset --quiet --hard "$1"
}

# (label, expect rc 0|nonzero, expected output fragment, command...)
run_gate() {
    local label="$1" expect="$2" needle="$3"
    shift 3
    local rc=0 out
    out="$( ( "$@" ) 2>&1 )" || rc=$?
    if [ "$expect" = "0" ] && [ "$rc" -ne 0 ]; then
        fail_test "$label: expected pass, got rc=$rc: $out"
    elif [ "$expect" != "0" ] && [ "$rc" -eq 0 ]; then
        fail_test "$label: expected refusal, got pass: $out"
    elif [ -n "$needle" ] && ! grep -qF -- "$needle" <<<"$out"; then
        fail_test "$label: output lacks '$needle': $out"
    fi
}

# --- 1. malformed target values are refused up front ------------------------
for bad in "${SHA_B:0:12}" "$(printf '%s' "$SHA_B" | tr 'a-f' 'A-F')" "${SHA_B}0" "main" "g${SHA_B:1}"; do
    rc=0
    out="$(AGENTDESK_DEPLOY_TARGET_SHA="$bad" bash -c "$validation_block" 2>&1)" || rc=$?
    if [ "$rc" -ne 2 ] || ! grep -qF 'must be a full 40-character' <<<"$out"; then
        fail_test "malformed target '$bad' must exit 2 with a format error; got rc=$rc: $out"
    fi
done
for good in "" "$SHA_B"; do
    rc=0
    out="$(AGENTDESK_DEPLOY_TARGET_SHA="$good" bash -c "$validation_block" 2>&1)" || rc=$?
    [ "$rc" -eq 0 ] || fail_test "target '$good' must pass format validation; got rc=$rc: $out"
done

# --- 2. local gates with a pinned target ------------------------------------
leader_at "$SHA_B"
DEPLOY_TARGET_SHA="$SHA_B"
run_gate "source identity: HEAD == target, ancestor of origin/main" 0 "Deploy target pinned: $SHA_B" \
    _check_repo_source_identity
run_gate "remote freshness: HEAD == target behind origin/main" 0 "Deploy target pinned: $SHA_B" \
    _check_repo_remote_freshness

grep -qF -- "--commit $SHA_B" "$GH_CALLS_FILE" \
    || fail_test "the CI Main query must name the pinned target commit; got: $(cat "$GH_CALLS_FILE" 2>/dev/null)"

# CI Main must have succeeded on the target; cancelled/failed/absent/unqueryable is refused.
GH_STUB_CONCLUSIONS="$(printf 'cancelled\nsuccess')"
run_gate "remote freshness: target with a successful re-run among cancelled ones" 0 "CI Main: success" \
    _check_repo_remote_freshness
for conclusions in "cancelled" "failure" "" "$(printf 'cancelled\nfailure')"; do
    GH_STUB_CONCLUSIONS="$conclusions"
    run_gate "remote freshness: target CI Main '${conclusions//$'\n'/,}' is not green" 1 "has no successful CI Main run" \
        _check_repo_remote_freshness
done
GH_STUB_CONCLUSIONS="success"
GH_STUB_FAIL=1
run_gate "remote freshness: CI Main status cannot be queried" 1 "could not query CI Main" \
    _check_repo_remote_freshness
GH_STUB_FAIL=0
GH_STUB_CONCLUSIONS="failure"
rm -f "$GH_CALLS_FILE"
DEPLOY_PEER_INVOCATION=1
run_gate "remote freshness on a peer: the leader already vetted CI" 0 "Deploy target pinned: $SHA_B" \
    _check_repo_remote_freshness
# shellcheck disable=SC2034  # Read by the production functions loaded through eval.
DEPLOY_PEER_INVOCATION=0
[ ! -s "$GH_CALLS_FILE" ] || fail_test "a peer invocation must not re-query CI Main"
GH_STUB_CONCLUSIONS="success"

DEPLOY_TARGET_SHA="$SHA_A"
run_gate "source identity: HEAD is not the target" 1 "does not match AGENTDESK_DEPLOY_TARGET_SHA" \
    _check_repo_source_identity
run_gate "remote freshness: HEAD is not the target" 1 "does not match AGENTDESK_DEPLOY_TARGET_SHA" \
    _check_repo_remote_freshness

leader_at "$SHA_SIDE"
DEPLOY_TARGET_SHA="$SHA_SIDE"
run_gate "source identity: target is not an ancestor of origin/main" 1 "is not an ancestor of origin/main" \
    _check_repo_source_identity
run_gate "remote freshness: target is not an ancestor of origin/main" 1 "is not an ancestor of origin/main" \
    _check_repo_remote_freshness

# --- 2b. freshness escape hatches keep the pin; only the CI query is skipped --
rc=0
out="$(AGENTDESK_DEPLOY_TARGET_SHA="$SHA_B" AGENTDESK_DEPLOY_BINARY=/tmp/prebuilt bash -c "$validation_block" 2>&1)" || rc=$?
if [ "$rc" -ne 2 ] || ! grep -qF 'cannot be combined with AGENTDESK_DEPLOY_BINARY' <<<"$out"; then
    fail_test "a pinned target with a prebuilt binary must exit 2; got rc=$rc: $out"
fi
rc=0
out="$(AGENTDESK_DEPLOY_BINARY=/tmp/prebuilt bash -c "$validation_block" 2>&1)" || rc=$?
[ "$rc" -eq 0 ] || fail_test "a prebuilt binary without a target must pass validation; got rc=$rc: $out"

leader_at "$SHA_C"
DEPLOY_TARGET_SHA="$SHA_B"
for hatch in AGENTDESK_DEPLOY_SKIP_FRESHNESS AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS; do
    export "$hatch=1"
    run_gate "$hatch: HEAD is not the target" 1 "does not match AGENTDESK_DEPLOY_TARGET_SHA" \
        _check_repo_source_identity
    unset "$hatch"
done
leader_at "$SHA_SIDE"
DEPLOY_TARGET_SHA="$SHA_SIDE"
export AGENTDESK_DEPLOY_SKIP_FRESHNESS=1
run_gate "SKIP_FRESHNESS: target is not an ancestor of origin/main" 1 "is not an ancestor of origin/main" \
    _check_repo_source_identity
leader_at "$SHA_B"
DEPLOY_TARGET_SHA="$SHA_B"
GH_STUB_CONCLUSIONS="failure"
rm -f "$GH_CALLS_FILE"
run_gate "SKIP_FRESHNESS: pinned HEAD passes without CI" 0 "Deploy target pinned: $SHA_B" \
    _check_repo_source_identity
run_gate "SKIP_FRESHNESS: remote freshness is skipped" 0 "" _check_repo_remote_freshness
unset AGENTDESK_DEPLOY_SKIP_FRESHNESS
[ ! -s "$GH_CALLS_FILE" ] || fail_test "a freshness escape hatch skips only the CI Main query; gh was called"
GH_STUB_CONCLUSIONS="success"

# --- 3. no target: the origin/main tip rule is unchanged ---------------------
DEPLOY_TARGET_SHA=""
leader_at "$SHA_B"
run_gate "default source identity: HEAD behind origin/main" 1 "does not match origin/main" \
    _check_repo_source_identity
run_gate "default remote freshness: HEAD behind origin/main" 1 "is behind origin/main by 1 commit(s)" \
    _check_repo_remote_freshness
leader_at "$SHA_C"
run_gate "default source identity: HEAD == origin/main" 0 "" _check_repo_source_identity
run_gate "default remote freshness: HEAD == origin/main" 0 "" _check_repo_remote_freshness
rm -f "$GH_CALLS_FILE"
leader_at "$SHA_B"
run_gate "default remote freshness never queries CI" 1 "" _check_repo_remote_freshness
[ ! -s "$GH_CALLS_FILE" ] || fail_test "an unset target must not query CI Main"
# Legacy escape hatches and branch rules behave as before when no target is set.
export AGENTDESK_DEPLOY_SKIP_FRESHNESS=1
run_gate "default SKIP_FRESHNESS: stale HEAD passes source identity" 0 "" _check_repo_source_identity
run_gate "default SKIP_FRESHNESS: stale HEAD passes remote freshness" 0 "" _check_repo_remote_freshness
unset AGENTDESK_DEPLOY_SKIP_FRESHNESS
export AGENTDESK_DEPLOY_BINARY=/tmp/prebuilt
run_gate "default prebuilt binary: stale HEAD passes source identity" 0 "" _check_repo_source_identity
unset AGENTDESK_DEPLOY_BINARY
git -C "$LEADER" checkout --quiet side
run_gate "default: non-main branch is refused" 1 "Refusing release deploy from non-main branch: side" \
    _check_repo_source_identity
export AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1
run_gate "default ALLOW_NON_MAIN: non-main branch passes" 0 "" _check_repo_source_identity
unset AGENTDESK_DEPLOY_ALLOW_NON_MAIN
git -C "$LEADER" checkout --quiet main

# --- 4. the target reaches peers ---------------------------------------------
DEPLOY_PEER_ENV_CHECK="$(AGENTDESK_DEPLOY_TARGET_SHA="$SHA_B" _deploy_peer_env_prelude)"
case "$DEPLOY_PEER_ENV_CHECK" in
    *"AGENTDESK_DEPLOY_TARGET_SHA=$SHA_B"*) : ;;
    *) fail_test "peer env prelude must forward AGENTDESK_DEPLOY_TARGET_SHA; got '$DEPLOY_PEER_ENV_CHECK'" ;;
esac
DEPLOY_PEER_ENV_CHECK="$(_deploy_peer_env_prelude)"
case "$DEPLOY_PEER_ENV_CHECK" in
    *AGENTDESK_DEPLOY_TARGET_SHA*) fail_test "an unset target must not appear in the peer prelude; got '$DEPLOY_PEER_ENV_CHECK'" ;;
esac
if ! grep -qF "export AGENTDESK_DEPLOY_TARGET_SHA=" "$DEPLOY_SH"; then
    fail_test "the detached helper must export AGENTDESK_DEPLOY_TARGET_SHA so the relaunched deploy keeps the pin"
fi

# --- 5. peer pre-sync ---------------------------------------------------------
# ssh stub: the first call (pre-sync) runs locally against the peer clone; every
# later call fails so _deploy_to_one_peer stops before any deploy is launched.
SSH_CALLS_FILE="$TMP_ROOT/ssh-calls"
PRESYNC_CMD_FILE="$TMP_ROOT/presync-cmd"
# shellcheck disable=SC2329  # Invoked by the production function loaded through eval.
ssh() {
    local calls remote
    calls="$(cat "$SSH_CALLS_FILE" 2>/dev/null || echo 0)"
    echo $((calls + 1)) >"$SSH_CALLS_FILE"
    [ "$calls" -eq 0 ] || return 1
    remote="${*: -1}"
    remote="${remote#bash -lc }"
    eval "printf '%s' $remote" >"$PRESYNC_CMD_FILE"
    bash -c "$(cat "$PRESYNC_CMD_FILE")"
}
export AGENTDESK_PEER_REPO_DIR="$PEER"

peer_at() {
    git -C "$PEER" checkout --quiet main
    git -C "$PEER" reset --quiet --hard "$1"
}

# (label, expect presync ok|refused, output fragment)
run_peer() {
    local label="$1" expect="$2" needle="$3" out rc=0
    rm -f "$SSH_CALLS_FILE" "$PRESYNC_CMD_FILE"
    out="$(_deploy_to_one_peer peer-stub 2>&1)" || rc=$?
    [ "$rc" -ne 0 ] || fail_test "$label: the ssh stub fails after pre-sync, so the peer leg must not succeed: $out"
    if [ "$expect" = "ok" ] && grep -qF "Pre-sync failed" <<<"$out"; then
        fail_test "$label: expected the pre-sync to pass: $out"
    elif [ "$expect" = "refused" ] && ! grep -qF "Pre-sync failed" <<<"$out"; then
        fail_test "$label: expected the pre-sync to be refused: $out"
    fi
    if [ -n "$needle" ] && ! grep -qF -- "$needle" <<<"$out"; then
        fail_test "$label: output lacks '$needle': $out"
    fi
}

leader_at "$SHA_B"
DEPLOY_TARGET_SHA="$SHA_B"
peer_at "$SHA_A"
run_peer "peer behind the target fast-forwards to it" ok ""
[ "$(git -C "$PEER" rev-parse HEAD)" = "$SHA_B" ] \
    || fail_test "peer must stop at the target $SHA_B, not the origin/main tip; got $(git -C "$PEER" rev-parse HEAD)"

peer_at "$SHA_B"
run_peer "peer already at the target" ok ""
[ "$(git -C "$PEER" rev-parse HEAD)" = "$SHA_B" ] || fail_test "a peer at the target must stay there"

peer_at "$SHA_C"
run_peer "peer main already ahead of the target" refused "refusing to rewind"
[ "$(git -C "$PEER" rev-parse HEAD)" = "$SHA_C" ] \
    || fail_test "a peer ahead of the target must never be rewound; got $(git -C "$PEER" rev-parse HEAD)"

leader_at "$SHA_C"
peer_at "$SHA_A"
rm -f "$SSH_CALLS_FILE"
rc=0
out="$(_deploy_to_one_peer peer-stub 2>&1)" || rc=$?
if [ "$rc" -eq 0 ] || ! grep -qF "is not AGENTDESK_DEPLOY_TARGET_SHA" <<<"$out"; then
    fail_test "a leader HEAD that is not the target must refuse the peer leg; got rc=$rc: $out"
fi
[ ! -s "$SSH_CALLS_FILE" ] || fail_test "a leader/target mismatch must be refused before any ssh call"

# No target: the legacy pre-sync command, byte for byte, still advancing to the tip.
# shellcheck disable=SC2034  # Read by the production functions loaded through eval.
DEPLOY_TARGET_SHA=""
leader_at "$SHA_C"
peer_at "$SHA_A"
run_peer "default pre-sync advances to origin/main" ok ""
legacy_cmd="set -e
cd $(printf '%q' "$PEER")
git fetch --quiet origin main
git checkout --quiet main
git merge --quiet --ff-only origin/main"
[ "$(cat "$PRESYNC_CMD_FILE")" = "$legacy_cmd" ] \
    || fail_test "an unset target must send the legacy pre-sync command unchanged; got: $(cat "$PRESYNC_CMD_FILE")"
[ "$(git -C "$PEER" rev-parse HEAD)" = "$SHA_C" ] || fail_test "default pre-sync must fast-forward the peer to origin/main"

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_deploy_target_sha_6200: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_deploy_target_sha_6200: all assertions passed"
