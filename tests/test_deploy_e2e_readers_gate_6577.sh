#!/usr/bin/env bash
# The deploy and rollback health gates accept a reader-count alarm only on the
# post-deploy smoke E2E channels, resolved from config; every other alarm blocks.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"

TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-e2e-readers-gate.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

FAILURES=0
pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; FAILURES=$((FAILURES + 1)); }

body() {
    printf '{"db":true,"dashboard":true,"server_up":true,"status":"degraded","ok":false,"fully_recovered":true,"degraded_reasons":%s}' "$1"
}

# The exact call both the new-binary and the rollback gate make, one attempt,
# with the HTTP fetch and launchd kick replaced.
gate_ready() {
    local health="$1"
    (
        curl() { printf '%s' "$health"; }
        _kickstart_launchd_job_if_needed() { return 0; }
        wait_for_http_service_health test-label 1 1 0 1 1 1 1
    ) >/dev/null 2>&1
}
expect_ready() {
    if gate_ready "$2"; then pass "$1"; else fail "$1 — the deploy would be refused"; fi
}
expect_blocked() {
    if gate_ready "$2"; then fail "$1 — the gate let it through"; else pass "$1"; fi
}

CONFIG="$TMP_ROOT/agentdesk.yaml"
cat > "$CONFIG" <<'YAML'
agents:
  - id: adk-claude-tui-e2e
    channels:
      claude: {id: "111"}
  - id: adk-codex-tui-e2e
    channels:
      codex: {id: "222"}
  - id: some-production-agent
    channels:
      claude: {id: "333"}
YAML

echo "§1 the smoke cells resolve to their channels through the smoke's own resolver"

ids=$(_deploy_e2e_smoke_channel_ids "$REPO_ROOT" "$CONFIG" claude-tui codex-tui)
if [ "$ids" = "111|222" ]; then pass "both smoke cells resolve ($ids)"; else fail "expected '111|222', got '$ids'"; fi

echo "§2 a smoke E2E channel's reader-count alarm does not fail a deploy or a rollback"

DEPLOY_E2E_SMOKE_CHANNEL_IDS="$ids"
INCIDENT="$(body '["tui_o:too_many_readers:111","tui_o:released:7"]')"
expect_ready "the measured payload: E2E reader alarm plus a released channel" "$INCIDENT"
expect_ready "the Codex smoke channel's reader alarm" "$(body '["tui_o:too_many_readers:222"]')"

echo "§3 any other channel, or a look-alike id, still blocks"

for reason in tui_o:too_many_readers:333 tui_o:too_many_readers:1110 \
    tui_o:too_many_readers:11 tui_o:too_many_readers:; do
    expect_blocked "$reason blocks" "$(body "[\"$reason\",\"tui_o:released:7\"]")"
done

echo "§4 another alarm on a smoke E2E channel still blocks"

for kind in binding_pending rotation_stalled halted blocked spool_full; do
    expect_blocked "tui_o:$kind:111 blocks" "$(body "[\"tui_o:$kind:111\"]")"
done
expect_blocked "an accepted reader alarm does not carry a halted one with it" \
    "$(body '["tui_o:too_many_readers:111","tui_o:halted:111"]')"
named=$(_health_json_deploy_blocking_reasons \
    "$(body '["tui_o:too_many_readers:111","tui_o:too_many_readers:333"]')" \
    "$(_health_json_deploy_nonblocking_ere_for_body "$INCIDENT" 1 1)")
if [ "$named" = "tui_o:too_many_readers:333" ]; then
    pass "the timeout diagnostic names only the other channel ($named)"
else
    fail "expected 'tui_o:too_many_readers:333', got '$named'"
fi

echo "§5 a config that does not resolve accepts nothing"

printf 'agents: [unterminated\n' > "$TMP_ROOT/broken.yaml"
printf 'agents:\n  - id: adk-claude-tui-e2e\n    channels:\n      claude: {id: "111"}\n' \
    > "$TMP_ROOT/claude-only.yaml"
for case in "missing:$TMP_ROOT/absent.yaml" "malformed:$TMP_ROOT/broken.yaml"; do
    DEPLOY_E2E_SMOKE_CHANNEL_IDS=$(_deploy_e2e_smoke_channel_ids "$REPO_ROOT" "${case#*:}" claude-tui codex-tui)
    if [ -z "$DEPLOY_E2E_SMOKE_CHANNEL_IDS" ]; then
        pass "${case%%:*} config resolves no channel"
    else
        fail "${case%%:*} config resolved '$DEPLOY_E2E_SMOKE_CHANNEL_IDS'"
    fi
    expect_blocked "${case%%:*} config keeps the reader alarm blocking" "$INCIDENT"
done
DEPLOY_E2E_SMOKE_CHANNEL_IDS=$(_deploy_e2e_smoke_channel_ids "$REPO_ROOT" "$TMP_ROOT/claude-only.yaml" claude-tui codex-tui)
expect_ready "a resolved cell is accepted when its sibling is unconfigured" "$INCIDENT"
expect_blocked "the unconfigured cell's channel is not" "$(body '["tui_o:too_many_readers:222"]')"
DEPLOY_E2E_SMOKE_CHANNEL_IDS='.*'
expect_blocked "a value that is not a channel id list accepts nothing" "$INCIDENT"

echo "§6 only a deploy verdict accepts it, and jq and the fallback agree"

DEPLOY_E2E_SMOKE_CHANNEL_IDS="$ids"
if health_json_is_ready "$INCIDENT" 1 1 1 0 >/dev/null 2>&1; then
    fail "a non-deploy caller accepted the reader alarm"
else
    pass "a non-deploy caller still refuses it"
fi
if ! command -v jq >/dev/null 2>&1; then
    fail "jq is absent, so the comparison would prove nothing"
else
    for shape in '["tui_o:too_many_readers:111","tui_o:released:7"]' \
        '["tui_o:too_many_readers:333"]' '["tui_o:binding_pending:111"]'; do
        b="$(body "$shape")"
        with_jq=0; health_json_is_ready "$b" 1 1 1 1 >/dev/null 2>&1 || with_jq=1
        without_jq=0
        (
            _health_json_has_jq() { return 1; }
            health_json_is_ready "$b" 1 1 1 1 >/dev/null 2>&1
        ) || without_jq=1
        if [ "$with_jq" = "$without_jq" ]; then
            pass "jq and fallback agree on $shape"
        else
            fail "jq said $with_jq but the fallback said $without_jq for $shape"
        fi
    done
fi

if [ "$FAILURES" -gt 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all E2E reader-alarm gate checks passed"
