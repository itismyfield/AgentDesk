#!/usr/bin/env bash
# Regression test for #5189, part 1: EVERY deploy run must end its transcript on a
# terminal marker, and that marker must be the LAST thing the run prints.
#
# What actually happened (from deploy-release.93281.log): a cluster peer refused
# promotion at the restart persistence gate and ended its log with NO terminal
# marker at all, because the `DEPLOY FAILED` echo was gated behind the
# detached-helper branch. A peer leg is neither a detached child nor report-channel
# bound, so nothing was printed and no log-based verdict had anything to match.
#
# The gate's refusal was CORRECT ("the in-flight delivery frontier is not durable").
# The defect is that the refusal never reached the report. §1 pins the marker onto
# every non-zero exit. §2 pins its POSITION: the script hands the operator a polling
# command built on `grep -qm1`, so a success marker printed before the cluster stage
# is read as the verdict for a deploy that has judged no peer at all.
#
# Later parts of this stack judge the peers themselves; this file is the contract
# they report through.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-cluster-verdict-test.XXXXXX")
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
eval "$(extract_function _emit_terminal_deploy_marker)"
eval "$(extract_function _report_peer_verdict_failure)"
eval "$(extract_function _wait_for_peer_deploy_verdict)"

failures=0
fail_test() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

# --- 1. every failing run must leave a terminal marker --------------------
# The peer's log ended with no marker precisely because this echo was gated on
# the detached-helper branch. A peer leg is neither detached-child nor
# report-channel bound.
DEPLOY_DETACHED_CHILD=0
REPORT_CHANNEL_ID=""
export DEPLOY_DETACHED_CHILD REPORT_CHANNEL_ID
marker_out="$(_emit_terminal_deploy_marker 1)"
case "$marker_out" in
    *"DEPLOY FAILED (exit=1)"*) : ;;
    *) fail_test "a non-zero exit must print a terminal DEPLOY FAILED marker even outside the detached helper; got '$marker_out'" ;;
esac
marker_out="$(_emit_terminal_deploy_marker 0)"
if [ -n "$marker_out" ]; then
    fail_test "a successful exit must not print a failure marker; got '$marker_out'"
fi
# The emitter is only half the contract — the EXIT path has to CALL it. Asserting the
# function in isolation leaves "delete the call site" undetected, and that restores
# the original defect exactly: a marker nothing invokes is a silent non-zero exit.
cleanup_body="$(extract_function _cleanup_on_exit)"
if [ -z "$cleanup_body" ]; then
    fail_test "could not extract _cleanup_on_exit from $DEPLOY_SH"
fi
case "$cleanup_body" in
    *_emit_terminal_deploy_marker*) : ;;
    *) fail_test "_cleanup_on_exit must emit the terminal marker; a marker function nothing calls leaves every non-zero exit silent" ;;
esac

# --- 2. the terminal marker must be the LAST thing a run prints ----------
# The script hands the operator a polling command built on `grep -qm1`, which stops
# at the FIRST match. While the success marker was printed BEFORE the cluster stage,
# that command locked onto it and reported success for a deploy that had not judged a
# single peer — #5189's own defect on a second path. The window is seconds today and
# grows to the 10-25 minutes a peer leg takes once peers deploy in the ssh foreground.
marker_line="$(grep -n '^echo "═══ Deploy Complete ═══"$' "$DEPLOY_SH" | head -1 | cut -d: -f1)"
cluster_line="$(grep -n '^    _deploy_to_all_peers "\$@"$' "$DEPLOY_SH" | head -1 | cut -d: -f1)"
if [ -z "$marker_line" ] || [ -z "$cluster_line" ]; then
    fail_test "could not locate the terminal marker echo and the cluster-deploy call in $DEPLOY_SH"
elif [ "$marker_line" -lt "$cluster_line" ]; then
    fail_test "the success marker (line $marker_line) is printed BEFORE the cluster stage (line $cluster_line) — the advised poll will report it as the verdict while peers are still being judged"
fi

# The behavioural half: run the polling command this script actually prints, against
# a log that is still GROWING, and require it to wait for the cluster verdict.
run_bounded() {
    # (deadline_secs, out_path, command-string) -> rc, 124 on deadline.
    local deadline="$1" out="$2" cmd="$3"
    ( eval "$cmd" ) > "$out" 2>&1 &
    local pid=$! waited=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$deadline" ]; then
            kill -9 "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            return 124
        fi
        sleep 1
        waited=$((waited + 1))
    done
    wait "$pid" 2>/dev/null || return $?
    return 0
}

poll_stmt="$(grep -F 'until [ -f ' "$DEPLOY_SH" | head -1)"
if [ -z "$poll_stmt" ]; then
    fail_test "could not find the one-shot wait command the script prints for the operator"
else
    log_path="$TMP_ROOT/helper-growing.log"
    # shellcheck disable=SC2034  # log_path is expanded by the extracted echo statement
    poll_cmd="$(eval "$poll_stmt" | sed -E 's/^[[:space:]]+//')"
    # Pre-cluster output including a per-peer ✓ line, which really does quote the
    # marker (PEER_DEPLOY_VERDICT carries it). An unanchored match takes it as the verdict.
    {
        printf '%s\n' '✓ Post-deploy functional smoke passed'
        printf '%s\n' '═══ Cluster Deploy → Peers ═══'
        printf '%s\n' '  ✓ mac-air — ═══ Deploy Complete ═══; repo_head 4ee96e55e'
    } > "$log_path"
    (
        sleep 4
        {
            printf '%s\n' '✗ Cluster deploy: 1/2 peer(s) did not prove promotion: mac-mini'
            printf '%s\n' '═══ DEPLOY FAILED (exit=1) ═══'
        } >> "$log_path"
    ) &
    writer_pid=$!
    poll_rc=0
    run_bounded 40 "$TMP_ROOT/poll.out" "$poll_cmd" || poll_rc=$?
    wait "$writer_pid" 2>/dev/null || true
    if [ "$poll_rc" -eq 124 ]; then
        fail_test "the advised polling command never terminated on a log that reached a terminal marker"
    elif ! grep -q '═══ DEPLOY FAILED (exit=1) ═══' "$TMP_ROOT/poll.out"; then
        fail_test "an operator polling exactly as instructed must be handed the cluster verdict; got: $(cat "$TMP_ROOT/poll.out")"
    fi
    if grep -q 'repo_head 4ee96e55e' "$TMP_ROOT/poll.out"; then
        fail_test "the polling command matched a line that merely QUOTES the terminal marker — it must match terminal lines only"
    fi
fi

# --- 3. peer completion requires an observed three-axis verdict ------------
# Stub the peer probe so these checks cannot reach SSH or a live release API.
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_TIMEOUT_SECS=30
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS=1
# shellcheck disable=SC2329  # Invoked indirectly by the production wait function.
_probe_peer_deploy_state() {
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        success '═══ Deploy Complete ═══' stale-head read true 'ok=true, status=healthy'
}

peer_verdict_rc=0
peer_verdict_out="$(_wait_for_peer_deploy_verdict \
    peer-stub /stub/deploy.log /stub/release-source.json 8791 target-head 2>&1)" \
    || peer_verdict_rc=$?
if [ "$peer_verdict_rc" -eq 0 ]; then
    fail_test "a terminal marker and healthy API must still be red when repo_head differs from the deploy target"
elif ! grep -q 'repo head does not match the deploy target' <<<"$peer_verdict_out"; then
    fail_test "a repo_head mismatch must identify the failing verdict axis; got: $peer_verdict_out"
fi

# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_PEER_VERDICT_TIMEOUT_SECS=0
# shellcheck disable=SC2329  # Invoked indirectly by the production wait function.
_probe_peer_deploy_state() {
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        missing 'no terminal marker' unavailable 'manifest unavailable' false 'request failed: stub'
}

peer_verdict_rc=0
peer_verdict_out="$(_wait_for_peer_deploy_verdict \
    peer-stub /stub/deploy.log /stub/release-source.json 8791 target-head 2>&1)" \
    || peer_verdict_rc=$?
if [ "$peer_verdict_rc" -eq 0 ]; then
    fail_test "a peer verdict timeout must be red"
elif ! grep -q 'timed out after 0s' <<<"$peer_verdict_out"; then
    fail_test "a timeout must be reported as the verdict failure reason; got: $peer_verdict_out"
elif ! grep -q 'terminal marker: missing' <<<"$peer_verdict_out" \
    || ! grep -q 'repo head: expected=target-head observed=unavailable' <<<"$peer_verdict_out" \
    || ! grep -q 'health: ok=false' <<<"$peer_verdict_out"; then
    fail_test "a timeout must report marker, repo head, and health observations; got: $peer_verdict_out"
fi

# --- 3c. peer leg rc propagation: _deploy_to_one_peer must use verdict rc ----
# If _wait_for_peer_deploy_verdict fails, _deploy_to_one_peer's rc must propagate it.
# Stub the verdict function and ssh/rsync to verify rc is not masked by unconditional
# success-returning statements (e.g. echo that should be before not after the verdict call).

# First, the structural check (text exists in the function).
peer_deploy_body="$(extract_function _deploy_to_one_peer)"
case "$peer_deploy_body" in
    *_wait_for_peer_deploy_verdict*) : ;;
    *) fail_test "_deploy_to_one_peer must use the observed peer verdict after SSH launches the deploy" ;;
esac
case "$peer_deploy_body" in
    *'deploy completed'*) fail_test "_deploy_to_one_peer must not describe SSH launch success as deploy completion" ;;
    *) : ;;
esac

# Now test rc propagation: load the function, stub its dependencies, and verify
# that when _wait_for_peer_deploy_verdict fails (rc=1), _deploy_to_one_peer also fails.
eval "$(extract_function _deploy_to_one_peer)"

# Stub git to match actual invocation: git -C "$REPO" rev-parse HEAD
# $1="-C", $2=$REPO path, $3="rev-parse", $4="HEAD"
git() {
    if [ "$3" = "rev-parse" ] && [ "$4" = "HEAD" ]; then
        echo "abc1234567890def"
    else
        return 0
    fi
}

# Stub ssh to handle different commands and return appropriate output
ssh() {
    # Parse the command to determine what output to return
    local cmd="${*: -1}"
    if [[ "$cmd" == *"AGENTDESK_ROOT_DIR"* ]]; then
        # Return peer's ADK_REL and port
        printf '%s\n' "/stub/.adk/release"
        printf '%s\n' "8791"
    else
        return 0
    fi
}

# Stub rsync (invoked only when routines directory exists on local machine)
rsync() {
    return 0
}

# Stub _deploy_peer_env_prelude to return empty string
_deploy_peer_env_prelude() {
    echo ""
}

# Stub _wait_for_peer_deploy_verdict to FAIL (return 1)
_wait_for_peer_deploy_verdict() {
    return 1
}

# Stub global variables required by _deploy_to_one_peer
export REPO="/stub/repo"
export ADK_REL="/stub/.adk/release"
export DEPLOY_SSH_CONNECT_TIMEOUT=10

# Test: _deploy_to_one_peer with failing verdict should return non-zero
# Verify rc propagates from verdict call, not from an earlier failure path.
peer_deploy_rc=0
peer_deploy_out=$(_deploy_to_one_peer "test-peer" 2>&1) || peer_deploy_rc=$?
if [ "$peer_deploy_rc" -eq 0 ]; then
    fail_test "_deploy_to_one_peer must fail (rc≠0) when _wait_for_peer_deploy_verdict fails; got rc=$peer_deploy_rc"
elif grep -q 'deploy verified' <<<"$peer_deploy_out"; then
    fail_test "_deploy_to_one_peer failure must not claim verified success; got: $peer_deploy_out"
fi

if grep -q 'Cluster Deploy Complete (all peers healthy)' "$DEPLOY_SH"; then
    fail_test "the cluster verdict must not claim all peers healthy without naming the verified verdict"
fi

if [ "$failures" -ne 0 ]; then
    printf '%s\n' "test_cluster_deploy_peer_verdict_5189: $failures assertion(s) failed" >&2
    exit 1
fi

printf '%s\n' "test_cluster_deploy_peer_verdict_5189: all assertions passed"
