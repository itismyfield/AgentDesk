#!/usr/bin/env bash
# A local pre-restart reader snapshot applies to deploy and rollback health,
# while new reader alarms and every other blocking reason retain their policy.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_deploy_policy_generation.sh"

TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-preexisting-readers-gate.XXXXXX")
cleanup() {
    python3 - "$TMP_ROOT" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
for entry in sorted(root.rglob("*"), key=lambda item: len(item.parts), reverse=True):
    entry.rmdir() if entry.is_dir() else entry.unlink()
root.rmdir()
PY
}
trap cleanup EXIT

FAILURES=0
pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; FAILURES=$((FAILURES + 1)); }
expect_equal() {
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 — expected '$3', got '$2'"; fi
}
body() {
    printf '{"db":true,"dashboard":true,"server_up":true,"status":"degraded","ok":false,"fully_recovered":true,"degraded_reasons":%s}' "$1"
}

# Load only function definitions; running or sourcing deploy-release would
# invoke deployment operations instead of the fixture paths under test.
extract_function() {
    awk -v start="^${1}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$REPO_ROOT/scripts/deploy-release.sh"
}
for function_name in _capture_deploy_preexisting_reader_alarms \
    _rollback_release_binary _wait_for_peer_deploy_verdict; do
    definition=$(extract_function "$function_name")
    [ -n "$definition" ] || { echo "missing function: $function_name" >&2; exit 1; }
    eval "$definition"
done

export REL_PORT=1
CURL_LOG="$TMP_ROOT/curl.log"
CURL_FIXTURE_BODY=""
CURL_FIXTURE_RC=0
curl() {
    printf '%s\n' "$*" >> "$CURL_LOG"
    printf '%s' "$CURL_FIXTURE_BODY"
    return "$CURL_FIXTURE_RC"
}
_kickstart_launchd_job_if_needed() { return 0; }
capture_preexisting() {
    CURL_FIXTURE_BODY="$1"
    CURL_FIXTURE_RC="${2:-0}"
    : > "$CURL_LOG"
    _capture_deploy_preexisting_reader_alarms > "$TMP_ROOT/capture.log"
}
gate_ready() {
    local health="$1"
    (
        curl() { printf '%s' "$health"; }
        wait_for_http_service_health fixture-label 1 1 0 1 1 1 1
    ) >/dev/null 2>&1
}
expect_ready() {
    if gate_ready "$2"; then pass "$1"; else fail "$1 — the deploy would be refused"; fi
}
expect_blocked() {
    if gate_ready "$2"; then fail "$1 — the gate let it through"; else pass "$1"; fi
}

DEPLOY_E2E_SMOKE_CHANNEL_IDS=""
SNAPSHOT_BODY="$(body '["tui_o:too_many_readers:111"]')"
NEW_BODY="$(body '["tui_o:too_many_readers:333"]')"
capture_preexisting "$SNAPSHOT_BODY"
expect_ready "FIRST: the snapshot reader alarm is nonblocking" "$SNAPSHOT_BODY"
expect_blocked "SECOND: a new post-snapshot reader alarm blocks" "$NEW_BODY"
expect_equal "the snapshot stores only its observed channel" "$DEPLOY_PREEXISTING_READER_CHANNEL_IDS" 111
expect_equal "the snapshot makes exactly one local health fetch" "$(wc -l < "$CURL_LOG" | tr -d ' ')" 1
if [[ "$(cat "$CURL_LOG")" == *"http://${ADK_DEFAULT_LOOPBACK}:1/api/health"* ]]; then
    pass "the snapshot fetches the local release endpoint"
else
    fail "the snapshot did not fetch the local release endpoint"
fi
if [ "$(cat "$TMP_ROOT/capture.log")" = '▸ Pre-existing reader-count alarms (non-blocking after deploy): 111' ]; then
    pass "the snapshot logs its exemptions on one line"
else
    fail "the snapshot log did not name exactly its captured channel"
fi
if python3 - "$REPO_ROOT/scripts/deploy-release.sh" <<'PY'
from pathlib import Path
import sys

lines = Path(sys.argv[1]).read_text().splitlines()
capture = [i for i, line in enumerate(lines) if line == "_capture_deploy_preexisting_reader_alarms"]
drain = next(i for i, line in enumerate(lines) if line.startswith("if ! request_restart_drain_mode_or_fail "))
stop = next(i for i, line in enumerate(lines) if line == 'echo "▸ Stopping release..."')
raise SystemExit(0 if len(capture) == 1 and capture[0] < drain < stop else 1)
PY
then
    pass "the real deploy snapshots once before drain and release stop"
else
    fail "the snapshot declaration has no single pre-drain, pre-stop call"
fi

echo "§1 failed reads and malformed bodies retain blocking policy"
capture_preexisting "$SNAPSHOT_BODY" 7
expect_equal "a failed fetch clears a stale snapshot" "$DEPLOY_PREEXISTING_READER_CHANNEL_IDS" ""
expect_blocked "failed snapshot fetch leaves its old channel blocking" "$SNAPSHOT_BODY"
expect_blocked "failed snapshot fetch leaves every other channel blocking" "$NEW_BODY"
expect_equal "failed fetch logs no exemptions" "$(cat "$TMP_ROOT/capture.log")" \
    '▸ Pre-existing reader-count alarms (non-blocking after deploy): none'
for malformed in '' '{' "$SNAPSHOT_BODY garbage" "$SNAPSHOT_BODY $SNAPSHOT_BODY" \
    '[]' '{}' '{"degraded_reasons":"tui_o:too_many_readers:111"}' \
    '{"degraded_reasons":{"reason":"tui_o:too_many_readers:111"}}' \
    '{"degraded_reasons":["tui_o:too_many_readers:111",null]}'; do
    DEPLOY_PREEXISTING_READER_CHANNEL_IDS=111
    capture_preexisting "$malformed"
    expect_equal "malformed snapshot clears the list: $malformed" "$DEPLOY_PREEXISTING_READER_CHANNEL_IDS" ""
    expect_blocked "malformed snapshot cannot exempt a reader alarm" "$SNAPSHOT_BODY"
done

echo "§2 only exact numeric reader ids enter the snapshot"
capture_preexisting "$(body '["tui_o:too_many_readers:111","tui_o:too_many_readers:222","tui_o:too_many_readers:111","tui_o:too_many_readers:","tui_o:too_many_readers:.*","tui_o:too_many_readers:333x","tui_o:too_many_readers: 444","tui_o:too_many_readers:555:extra","provider:claude:too_many_readers:666","tui_o:halted:777"]')"
expect_equal "numeric ids are deduplicated and non-reader or invalid ids are discarded" \
    "$DEPLOY_PREEXISTING_READER_CHANNEL_IDS" '111|222'
expect_ready "both observed numeric reader channels are accepted" \
    "$(body '["tui_o:too_many_readers:111","tui_o:too_many_readers:222"]')"
for reason in tui_o:too_many_readers:1110 tui_o:too_many_readers:11 \
    tui_o:too_many_readers:333x tui_o:too_many_readers: tui_o:halted:111 \
    tui_o:binding_pending:111 provider:claude:too_many_readers:111 \
    provider:claude:halted:111; do
    expect_blocked "$reason retains its blocking classification" "$(body "[\"$reason\"]")"
done
expect_ready "provider reconciliation retains its nonblocking classification" \
    "$(body '["provider:claude:reconcile_in_progress"]')"
expect_ready "provider queue depth retains its deploy classification" \
    "$(body '["provider:claude:pending_queue_depth:111"]')"

echo "§3 smoke and snapshot lists merge without empty or unsafe regex alternatives"
DEPLOY_E2E_SMOKE_CHANNEL_IDS='222|444'
DEPLOY_PREEXISTING_READER_CHANNEL_IDS='111|222'
expect_ready "smoke and preexisting channel sets both apply" \
    "$(body '["tui_o:too_many_readers:111","tui_o:too_many_readers:222","tui_o:too_many_readers:444"]')"
expect_blocked "the union does not accept an unlisted channel" "$NEW_BODY"
for invalid in '.*' '|111' '111|' '111||222' '111|.*'; do
    DEPLOY_E2E_SMOKE_CHANNEL_IDS="$invalid"
    DEPLOY_PREEXISTING_READER_CHANNEL_IDS="$invalid"
    expect_blocked "invalid id lists cannot broaden exemptions: $invalid" "$SNAPSHOT_BODY"
done
DEPLOY_E2E_SMOKE_CHANNEL_IDS='.*'
DEPLOY_PREEXISTING_READER_CHANNEL_IDS=111
expect_ready "an invalid smoke list does not discard the valid snapshot" "$SNAPSHOT_BODY"
DEPLOY_E2E_SMOKE_CHANNEL_IDS=222
DEPLOY_PREEXISTING_READER_CHANNEL_IDS='.*'
expect_ready "an invalid snapshot list does not discard the valid smoke list" \
    "$(body '["tui_o:too_many_readers:222"]')"
expect_blocked "an invalid snapshot list does not accept its old channel" "$SNAPSHOT_BODY"
DEPLOY_E2E_SMOKE_CHANNEL_IDS=""
DEPLOY_PREEXISTING_READER_CHANNEL_IDS=""
ere=$(_health_json_deploy_nonblocking_ere_for_body "$SNAPSHOT_BODY" 1 1)
if [[ "$ere" == *too_many_readers* || "$ere" == *'()'* ]]; then
    fail "empty id lists introduced a reader alternative: $ere"
else
    pass "empty id lists introduce no reader or empty regex alternative"
fi
expect_blocked "an empty merged list keeps every reader alarm blocking" "$SNAPSHOT_BODY"

echo "§4 deploy scope and jq/fallback agreement remain intact"
DEPLOY_PREEXISTING_READER_CHANNEL_IDS=111
if health_json_is_ready "$SNAPSHOT_BODY" 1 1 1 0 >/dev/null 2>&1; then
    fail "a non-deploy caller accepted a snapshot reader alarm"
else
    pass "a non-deploy caller keeps the reader alarm blocking"
fi
if ! command -v jq >/dev/null 2>&1; then
    fail "jq is absent, so the comparison would prove nothing"
else
    for shape in '["tui_o:too_many_readers:111","tui_o:released:7"]' \
        '["tui_o:too_many_readers:333"]' '["provider:claude:halted:111"]'; do
        b="$(body "$shape")"
        with_jq=0; health_json_is_ready "$b" 1 1 1 1 >/dev/null 2>&1 || with_jq=1
        without_jq=0
        (
            _health_json_has_jq() { return 1; }
            health_json_is_ready "$b" 1 1 1 1 >/dev/null 2>&1
        ) || without_jq=1
        expect_equal "jq and fallback agree on $shape" "$with_jq" "$without_jq"
    done
fi

echo "§5 the rollback function actually uses the captured local policy"
# Execute the production rollback health path with every stop, swap, guard,
# launchd and tmux operation stubbed; all paths point at plain fixture files.
printf 'fixture previous binary\n' > "$TMP_ROOT/previous-binary"
printf 'fixture current binary\n' > "$TMP_ROOT/current-binary"
rollback_with_body() {
    (
        export REL_BINARY="$TMP_ROOT/current-binary"
        export REL_BINARY_BACKUP="$TMP_ROOT/previous-binary"
        export PLIST_REL=fixture-release
        export ADK_REL="$TMP_ROOT"
        export DEPLOY_HEALTH_RETRIES=1
        export DEPLOY_HEALTH_DELAY_SECS=0
        export AGENTDESK_RELEASE_TMUX_SESSION=fixture-release
        CURL_FIXTURE_BODY="$1"
        CURL_FIXTURE_RC=0
        _rollback_would_brick_on_migration() { return 1; }
        _rollback_would_revert_o_writer() { return 1; }
        _rollback_would_revert_o_ledger() { return 1; }
        _launchd_domain() { printf 'fixture/domain'; }
        _wait_release_stopped() { return 0; }
        launchctl() { return 0; }
        tmux() { return 0; }
        chflags() { return 0; }
        xattr() { return 0; }
        mv() { return 0; }
        start_release_tmux_fallback() { return 0; }
        _rollback_release_binary
    ) > "$TMP_ROOT/rollback.log" 2>&1
}
DEPLOY_E2E_SMOKE_CHANNEL_IDS=""
capture_preexisting "$SNAPSHOT_BODY"
: > "$CURL_LOG"
rollback_with_body "$SNAPSHOT_BODY"
if grep -q 'Rollback succeeded' "$TMP_ROOT/rollback.log"; then
    pass "the executed rollback health gate accepts the snapshot channel"
else
    fail "the executed rollback rejected the snapshot channel: $(cat "$TMP_ROOT/rollback.log")"
fi
expect_equal "rollback traverses the real health fetch exactly once" "$(wc -l < "$CURL_LOG" | tr -d ' ')" 1
rollback_with_body "$NEW_BODY"
if grep -q 'Rollback restart did not reach healthy state' "$TMP_ROOT/rollback.log" \
    && ! grep -q 'Rollback succeeded' "$TMP_ROOT/rollback.log"; then
    pass "the executed rollback health gate still rejects a new channel"
else
    fail "the executed rollback accepted a new channel: $(cat "$TMP_ROOT/rollback.log")"
fi

echo "§6 a transported peer body cannot inherit the local snapshot"
export DEPLOY_PEER_VERDICT_TIMEOUT_SECS=0
export DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS=1
PEER_HEALTH_BODY="$SNAPSHOT_BODY"
_probe_peer_deploy_state() {
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        success fixture-marker target-head fixture-repo false fixture-health "$PEER_HEALTH_BODY"
}
_report_peer_verdict_failure() { printf '%s\n' "$@"; }
if _wait_for_peer_deploy_verdict fixture-peer fixture-log fixture-manifest 1 target-head \
    > "$TMP_ROOT/peer.log" 2>&1; then
    fail "the peer incorrectly inherited the local snapshot"
elif grep -q 'deploy-blocking: tui_o:too_many_readers:111' "$TMP_ROOT/peer.log"; then
    pass "the peer refuses and names a locally preexisting reader alarm"
else
    fail "peer refusal did not name its blocking reader alarm: $(cat "$TMP_ROOT/peer.log")"
fi
expect_equal "the peer verdict preserves the local snapshot afterward" "$DEPLOY_PREEXISTING_READER_CHANNEL_IDS" 111
export DEPLOY_E2E_SMOKE_CHANNEL_IDS=222
PEER_HEALTH_BODY="$(body '["tui_o:too_many_readers:222"]')"
if _wait_for_peer_deploy_verdict fixture-peer fixture-log fixture-manifest 1 target-head \
    > "$TMP_ROOT/peer.log" 2>&1; then
    pass "the peer retains its existing smoke reader exemption"
else
    fail "the peer lost its smoke exemption: $(cat "$TMP_ROOT/peer.log")"
fi

echo "§7 a peer keeps the Herdr worker channel claude-tui moved off exempt"
# 444 is the Herdr worker channel, 111 claude-tui's tmux channel, 222 Codex; exemptions come from the real resolver.
printf 'agents:\n  - {id: adk-claude-tui-e2e, channels: {claude: {id: "444"}}}\n  - {id: adk-claude-tui-2-e2e, channels: {claude: {id: "111"}}}\n  - {id: adk-codex-tui-e2e, channels: {codex: {id: "222"}}}\n' > "$TMP_ROOT/both.yaml"
printf 'agents:\n  - {id: adk-claude-tui-e2e, channels: {claude: {id: "444"}}}\n  - {id: adk-codex-tui-e2e, channels: {codex: {id: "222"}}}\n' > "$TMP_ROOT/old-only.yaml"
peer_verdict() {
    export DEPLOY_E2E_SMOKE_CHANNEL_IDS
    DEPLOY_E2E_SMOKE_CHANNEL_IDS=$(_deploy_e2e_smoke_channel_ids "$REPO_ROOT" "$TMP_ROOT/$1" "$2" codex-tui)
    PEER_HEALTH_BODY="$(body "[\"$3\"]")"
    _wait_for_peer_deploy_verdict fixture-peer fixture-log fixture-manifest 1 target-head > "$TMP_ROOT/peer.log" 2>&1
}
for config in both.yaml old-only.yaml; do
    if peer_verdict "$config" claude-tui tui_o:too_many_readers:444; then
        pass "$config: the peer verdict accepts the Herdr worker channel's reader alarm"
    else
        fail "$config: the peer verdict refused 444: $(cat "$TMP_ROOT/peer.log")"
    fi
    for reason in tui_o:too_many_readers:333 tui_o:halted:444 tui_o:binding_pending:444; do
        if peer_verdict "$config" claude-tui "$reason"; then
            fail "$config: the peer verdict accepted $reason"
        else
            pass "$config: the peer verdict still refuses $reason"
        fi
    done
done
if peer_verdict both.yaml codex-tui tui_o:too_many_readers:444; then
    fail "a Codex-only smoke exempted the Herdr worker channel on the peer"
else
    pass "a Codex-only smoke leaves the Herdr worker channel blocking on the peer"
fi

if [ "$FAILURES" -gt 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all preexisting reader-alarm gate checks passed"
