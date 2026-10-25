#!/usr/bin/env bash
# Named degraded exemptions are deploy-only, full-reason matches.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"
# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/lib/deploy_allow_degraded.sh"
unset AGENTDESK_DEPLOY_ALLOW_DEGRADED
export DEPLOY_E2E_SMOKE_CHANNEL_IDS=""
export DEPLOY_PREEXISTING_READER_CHANNEL_IDS=""
FAILURES=0
CHECKS=0
pass() { CHECKS=$((CHECKS + 1)); echo "  ✓ $1"; }
fail() { CHECKS=$((CHECKS + 1)); FAILURES=$((FAILURES + 1)); echo "  ✗ $1" >&2; }
equal() { if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: expected '$3', got '$2'"; fi; }
body() {
    printf '{"server_up":true,"db":true,"dashboard":true,"status":"degraded","degraded_reasons":%s}' "$1"
}
ready() { health_json_is_ready "$1" 1 1 1 "${2:-1}"; }
expect_ready() { if ready "$2" >/dev/null 2>&1; then pass "$1"; else fail "$1"; fi; }
expect_blocked() { if ready "$2" "${3:-1}" >/dev/null 2>&1; then fail "$1"; else pass "$1"; fi; }
INCIDENT=$(body '["tui_o:schema_blocked:1479671301387059200"]')
MIXED=$(body '["tui_o:schema_blocked:1479671301387059200","tui_o:halted:7"]')
BASE=$(_health_json_deploy_nonblocking_ere_for_body "$INCIDENT" 1 1)
export AGENTDESK_DEPLOY_ALLOW_DEGRADED='tui_o:schema_blocked:1479671301387059200'
expect_ready "FIRST: the named degraded cause alone passes" "$INCIDENT"
[ "$FAILURES" = 0 ] || exit 1
if ! declare -F _deploy_allow_degraded_validate >/dev/null; then
    echo "allow-degraded helper is missing; $FAILURES failure(s)" >&2
    exit 1
fi

for mode in jq fallback; do
    if [ "$mode" = fallback ]; then
        _health_json_has_jq() { return 1; }
    elif ! command -v jq >/dev/null; then
        fail "jq must be installed to test both readers"
        continue
    fi
    expect_ready "$mode: the named degraded cause passes" "$INCIDENT"
    expect_blocked "$mode: an unnamed cause mixed in still blocks" "$MIXED"
    expect_blocked "$mode: a non-deploy caller still blocks" "$INCIDENT" 0
    expect_blocked "$mode: a partial reason match cannot waive a suffix" \
        "$(body '["tui_o:schema_blocked:14796713013870592000"]')"
    expect_ready "$mode: an existing nonblocking cause can mix with the named one" \
        "$(body '["tui_o:schema_blocked:1479671301387059200","relay_verdict_unknown_codex_c1"]')"
    for axis in server_up db dashboard; do
        expect_blocked "$mode: $axis=false still blocks" "${INCIDENT/\"$axis\":true/\"$axis\":false}"
    done
    expect_blocked "$mode: unhealthy still blocks" "${INCIDENT/\"degraded\"/\"unhealthy\"}"
    AGENTDESK_DEPLOY_ALLOW_DEGRADED='.*'
    expect_blocked "$mode: a broad pattern still cannot waive db=false" "${INCIDENT/\"db\":true/\"db\":false}"
    expect_blocked "$mode: a broad pattern cannot waive an unhealthy status" "${INCIDENT/\"degraded\"/\"unhealthy\"}"
    expect_blocked "$mode: a broad pattern cannot waive an empty list" "$(body '[]')"
    expect_blocked "$mode: a broad pattern cannot waive an empty reason" "$(body '[""]')"
    for pattern in 'a)|x' 'a)|.*' '[(]|foo' 'foo|foobar' '.*'; do
        AGENTDESK_DEPLOY_ALLOW_DEGRADED="$pattern"
        ere=$(_health_json_deploy_nonblocking_ere_for_body "$(body '["ab","a)","foo.bar","foobar"]')" 1 1)
        case "$pattern" in
            'a)|x')
                if [[ ab =~ $ere ]]; then fail "$mode: literal right-parenthesis escaped the outer anchor"; else pass "$mode: literal right-parenthesis preserves the anchor"; fi
                expect_ready "$mode: a literal right-parenthesis is still a valid ERE" "$(body '["a)"]')"
                ;;
            'foo|foobar') expect_ready "$mode: alternation uses a full reason match" "$(body '["foobar"]')" ;;
            '.*')
                if [[ unobserved =~ $ere ]]; then fail "$mode: broad pattern leaked beyond observed reasons"; else pass "$mode: broad pattern only inserts observed literal reasons"; fi
                if [[ fooXbar =~ $ere ]]; then fail "$mode: reason metacharacters were not escaped"; else pass "$mode: observed reasons are ERE-escaped"; fi
                ;;
            *)
                if [[ unobserved =~ $ere ]]; then fail "$mode: pattern escaped the accepted set: $pattern"; else pass "$mode: the accepted set remains anchored: $pattern"; fi
                ;;
        esac
    done
    AGENTDESK_DEPLOY_ALLOW_DEGRADED='tui_o:schema_blocked:1479671301387059200'
done
unset -f _health_json_has_jq
_health_json_has_jq() { command -v jq >/dev/null 2>&1; }

for pattern in '' 'foo,bar' '[' ')|(' $'foo\nbar'; do
    AGENTDESK_DEPLOY_ALLOW_DEGRADED="$pattern"
    if output=$(_deploy_allow_degraded_validate 2>&1); then
        fail "invalid pattern was accepted: $pattern"
    elif [[ "$output" == *AGENTDESK_DEPLOY_ALLOW_DEGRADED* ]]; then
        pass "invalid pattern is rejected with a variable-specific message"
    else
        fail "invalid pattern lacked an explanation"
    fi
    rc=0
    output=$("$BASH" "$REPO_ROOT/scripts/deploy-release.sh" 2>&1) || rc=$?
    equal "real deploy rejects invalid input before detach" "$rc" 2
    if [[ "$output" == *'[detach]'* ]]; then fail "invalid input spawned a detached deploy"; else pass "invalid input did not spawn a detached deploy"; fi
    expect_blocked "invalid patterns fail closed in a deploy verdict" "$INCIDENT"
    equal "a non-deploy pattern is byte-identical even with invalid input" \
        "$(_health_json_deploy_nonblocking_ere 1 0)" \
        "$(unset AGENTDESK_DEPLOY_ALLOW_DEGRADED; _health_json_deploy_nonblocking_ere 1 0)"
done
for flag in --skip-health --skip-review; do
    unset AGENTDESK_DEPLOY_ALLOW_DEGRADED
    if output=$(_deploy_allow_degraded_validate "$flag" 2>&1); then
        fail "$flag was silently ignored"
    elif [[ "$output" == *AGENTDESK_DEPLOY_ALLOW_DEGRADED* ]]; then
        pass "$flag is rejected with the replacement variable"
    else
        fail "$flag lacked the replacement guidance"
    fi
    rc=0
    output=$("$BASH" "$REPO_ROOT/scripts/deploy-release.sh" "$flag" 2>&1) || rc=$?
    equal "real deploy rejects $flag before detach" "$rc" 2
done
unset AGENTDESK_DEPLOY_ALLOW_DEGRADED
equal "unset environment preserves exact baseline pattern bytes" \
    "$(_health_json_deploy_nonblocking_ere_for_body "$INCIDENT" 1 1)" "$BASE"
expect_blocked "unset environment keeps the incident blocked" "$INCIDENT"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-defaults-bundle.XXXXXX")
trap 'python3 -c "import shutil, sys; shutil.rmtree(sys.argv[1])" "$TMP_ROOT"' EXIT
cp "$REPO_ROOT/scripts/_defaults.sh" "$TMP_ROOT/_defaults.sh"
equal "bundled defaults without the helper preserve an unset deploy pattern" \
    "$(/bin/bash -euc '. "$1"; _health_json_deploy_nonblocking_ere 1 1' fixture "$TMP_ROOT/_defaults.sh")" "$BASE"
equal "bundled defaults ignore an invalid override for non-deploy callers" \
    "$(AGENTDESK_DEPLOY_ALLOW_DEGRADED='[' /bin/bash -euc '. "$1"; _health_json_deploy_nonblocking_ere 1 0' fixture "$TMP_ROOT/_defaults.sh")" \
    "$(_health_json_deploy_nonblocking_ere 1 0)"
if AGENTDESK_DEPLOY_ALLOW_DEGRADED='.*' /bin/bash -euc \
    '. "$1"; _health_json_deploy_nonblocking_ere 1 1' fixture "$TMP_ROOT/_defaults.sh" >/dev/null 2>&1; then
    fail "a missing helper allowed a requested deploy waiver"
else
    pass "a missing helper fails closed only when a deploy requests a waiver"
fi

extract_function() {
    awk -v start="^${1}[(][)] [{]$" '
        $0 ~ start { printing=1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$REPO_ROOT/scripts/deploy-release.sh"
}
eval "$(extract_function _deploy_peer_env_prelude)"
export AGENTDESK_DEPLOY_ALLOW_DEGRADED='a)|.* $literal `quoted` "space"'
prelude=$(_deploy_peer_env_prelude)
equal "peer prelude preserves ERE and shell metacharacters through printf %q" \
    "$(unset AGENTDESK_DEPLOY_ALLOW_DEGRADED; eval "$prelude"; printf '%s' "$AGENTDESK_DEPLOY_ALLOW_DEGRADED")" \
    "$AGENTDESK_DEPLOY_ALLOW_DEGRADED"
export_line=$(sed -n '/^.*printf.*export AGENTDESK_DEPLOY_ALLOW_DEGRADED=/p' "$REPO_ROOT/scripts/deploy-release.sh")
equal "detached helper preserves the same quoted pattern" \
    "$(eval "printf '%s' \"$export_line\"")" \
    "export AGENTDESK_DEPLOY_ALLOW_DEGRADED=$(printf '%q' "$AGENTDESK_DEPLOY_ALLOW_DEGRADED")"
unset AGENTDESK_DEPLOY_ALLOW_DEGRADED
equal "detached helper does not invent an empty variable when unset" "$(eval "printf '%s' \"$export_line\"")" ''

# Exercise the exact rollback condition with a health waiter stub, without
# executing the rollback's filesystem, launchd, or process operations.
rollback_condition=$(extract_function _rollback_release_binary | sed -n '/^    if .*wait_for_http_service_health /p')
export AGENTDESK_DEPLOY_ALLOW_DEGRADED='.*'
wait_for_http_service_health() { [ "${AGENTDESK_DEPLOY_ALLOW_DEGRADED+x}" != x ]; }
export plist=fixture rel_port=1 DEPLOY_HEALTH_RETRIES=1 DEPLOY_HEALTH_DELAY_SECS=0
if eval "$rollback_condition pass 'rollback health excludes the new override'; else fail 'rollback health inherited the override'; fi"; then :; fi
equal "rollback scope restores the caller environment" "$AGENTDESK_DEPLOY_ALLOW_DEGRADED" '.*'

wait_for_http_service_health() {
    export WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON="$LOG_BODY"
    return "$LOG_RC"
}
export AGENTDESK_DEPLOY_ALLOW_DEGRADED='tui_o:schema_blocked:[0-9]+'
for LOG_RC in 0 1; do
    LOG_BODY="$MIXED"
    rc=0
    output=$(_deploy_wait_with_allow_degraded fixture 1 1 0 1 1 1 1) || rc=$?
    equal "health wrapper preserves rc=$LOG_RC" "$rc" "$LOG_RC"
    equal "health wrapper logs one exact waiver line on rc=$LOG_RC" "$output" \
        '▸ [allow-degraded] pattern=<tui_o:schema_blocked:[0-9]+> waived=<tui_o:schema_blocked:1479671301387059200>'
done
LOG_BODY=$(body '["relay_verdict_unknown_codex_c1"]') LOG_RC=0
equal "a pattern matching no additional causes logs none" \
    "$(_deploy_wait_with_allow_degraded fixture 1 1 0 1 1 1 1)" \
    '▸ [allow-degraded] pattern=<tui_o:schema_blocked:[0-9]+> waived=<none>'
AGENTDESK_DEPLOY_ALLOW_DEGRADED='.*'
equal "existing nonblocking causes are not reported as newly waived" \
    "$(_deploy_wait_with_allow_degraded fixture 1 1 0 1 1 1 1)" \
    '▸ [allow-degraded] pattern=<.*> waived=<none>'
unset AGENTDESK_DEPLOY_ALLOW_DEGRADED
equal "no override produces no extra log" "$(_deploy_wait_with_allow_degraded fixture 1 1 0 1 1 1 1)" ''

if python3 - "$REPO_ROOT/scripts/deploy-release.sh" <<'PY'
from pathlib import Path
import sys
text = Path(sys.argv[1]).read_text()
assert text.index('_deploy_allow_degraded_validate "$@"') < text.index('ADK_REL=')
assert text.index('_deploy_allow_degraded_validate "$@"') < text.index('nohup "$0"')
assert 'if _deploy_wait_with_allow_degraded "$PLIST_REL" "$REL_PORT"' in text
assert '--skip-review|--skip-health)' not in text
PY
then pass "real deploy validates early and uses the logging health wrapper"; else fail "real deploy wiring is missing"; fi
echo "$CHECKS checks, $FAILURES failure(s)"
[ "$FAILURES" = 0 ]
