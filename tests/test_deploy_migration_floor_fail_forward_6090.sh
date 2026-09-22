#!/usr/bin/env bash
# Regression test for #6090: a deploy that aborts after Postgres may have advanced,
# but before the staged binary is promoted, must not leave the node on a binary
# sqlx refuses to boot (mac-mini crash-looped this way on migrations 113/116/120/122).
#
# The recovery must not overreach either. The restart-durability gate refuses to
# stop a runtime whose in-flight delivery frontier is not proven durable; recovering
# by stopping that runtime anyway would discard exactly what the refusal protected.
# So promote forward only when nothing is serving, and otherwise preserve the one
# binary that can boot and leave the runtime alone.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-migration-floor-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

FAILURES=0
fail() { echo "  ✗ $1" >&2; FAILURES=$((FAILURES + 1)); }
pass() { echo "  ✓ $1"; }

extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/_defaults.sh"

for fn in _recover_or_preserve_past_migration_floor _preserve_staged_binary_for_recovery \
    _release_runtime_is_serving _migration_floor_may_advance; do
    body="$(extract_function "$fn")"
    if [ -z "$body" ]; then
        fail "$fn is not defined in $DEPLOY_SH"
        echo "$FAILURES failure(s)" >&2
        exit 1
    fi
    eval "$body"
done

# Emulates curl closely enough that -f/--fail and the exit code both matter: a
# probe that only accepts rc 0 would look correct here and ship a live-runtime bug.
curl() {
    local fail_on_http=0 a
    for a in "$@"; do
        case "$a" in --fail) fail_on_http=1 ;; --*) ;; -*f*) fail_on_http=1 ;; esac
    done
    [ "${STUB_CURL_RC:-0}" = 0 ] || return "${STUB_CURL_RC:-0}"
    printf '%s' "${STUB_HTTP_CODE:-200}"
    if [ "$fail_on_http" = 1 ] && [ "${STUB_HTTP_CODE:-200}" -ge 400 ]; then
        return 22
    fi
    return 0
}
launchctl() { echo "launchctl $*" >>"$TMP_ROOT/calls"; return 0; }
chflags() { echo "chflags $*" >>"$TMP_ROOT/calls"; return 0; }
tmux() { echo "tmux $*" >>"$TMP_ROOT/calls"; return 0; }
xattr() { return 0; }
_launchd_domain() { echo "gui/501"; }
start_release_tmux_fallback() { echo "tmux-fallback" >>"$TMP_ROOT/calls"; return 0; }
wait_for_http_service_health() { echo "health $*" >>"$TMP_ROOT/calls"; return 0; }
mv() {
    local dst="${!#}"
    if [ "${STUB_MV_FAIL:-0}" = 1 ]; then
        case "$dst" in */agentdesk) return 1 ;; esac
    fi
    command mv "$@"
}

# shellcheck disable=SC2034  # Read by the production function loaded through eval.
PLIST_REL="com.agentdesk.test"
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_HEALTH_RETRIES=1
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_HEALTH_DELAY_SECS=1
ADK_REL="$TMP_ROOT/rel"
REL_PORT="18791"
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
OLD_PID=""
mkdir -p "$ADK_REL/bin"
REL_BINARY="$ADK_REL/bin/agentdesk"
REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
RECOVERY="$ADK_REL/bin/agentdesk.migration-floor-recovery"

reset_node() {
    rm -f "$REL_BINARY" "$REL_BINARY_BACKUP" "$RECOVERY" "$ADK_REL/bin/agentdesk.deploy.test"
    printf 'OLD-UNBOOTABLE' >"$REL_BINARY"
    STAGED_BINARY="$ADK_REL/bin/agentdesk.deploy.test"
    printf 'STAGED-NEW' >"$STAGED_BINARY"
    : >"$TMP_ROOT/calls"
    STUB_MV_FAIL=0
}

echo "§1 nothing serving: promote the staged binary so launchd stops crash-looping"

STUB_CURL_RC=7
STUB_HTTP_CODE=000
reset_node
_recover_or_preserve_past_migration_floor >"$TMP_ROOT/out" 2>&1 || true

if [ "$(cat "$REL_BINARY")" = "STAGED-NEW" ]; then
    pass "the staged binary is now live"
else
    fail "live binary is still '$(cat "$REL_BINARY")' — the node stays bricked"
fi
if [ -z "${STAGED_BINARY:-}" ]; then
    pass "STAGED_BINARY was cleared so the EXIT cleanup cannot delete the live binary"
else
    fail "STAGED_BINARY still points at '$STAGED_BINARY' after promotion"
fi
if grep -q "bootout gui/501/com.agentdesk.test" "$TMP_ROOT/calls"; then
    pass "the crash-looping job was booted out before the swap"
else
    fail "no bootout issued before the swap"
fi
if grep -q "^tmux kill-session" "$TMP_ROOT/calls"; then
    pass "the manual tmux fallback was killed, so no old process can answer the health check"
else
    fail "the tmux fallback was left alive — health could pass on the OLD executable"
fi
if grep -q "^launchctl bootstrap " "$TMP_ROOT/calls"; then
    pass "the service was restarted after the swap"
else
    fail "the service was never restarted"
fi
if [ ! -e "$REL_BINARY_BACKUP" ]; then
    pass "the unbootable binary was not recorded as last-known-good"
else
    fail ".prev was written with a binary that cannot boot"
fi

echo "§2 a serving runtime is left alone — the durability refusal is not undone"

STUB_CURL_RC=0
STUB_HTTP_CODE=200
reset_node
_recover_or_preserve_past_migration_floor >>"$TMP_ROOT/out" 2>&1 || true

if [ "$(cat "$REL_BINARY")" = "OLD-UNBOOTABLE" ]; then
    pass "the live binary was not swapped under a serving runtime"
else
    fail "the binary was swapped while a runtime was still serving"
fi
if ! grep -q "bootout" "$TMP_ROOT/calls"; then
    pass "the serving runtime was not stopped, so its in-flight frontier survives"
else
    fail "a serving runtime was booted out — this is the loss the durability gate refused to risk"
fi
if [ -e "$RECOVERY" ] && [ "$(cat "$RECOVERY")" = "STAGED-NEW" ]; then
    pass "the migration-capable binary was preserved for the redeploy"
else
    fail "the only bootable binary was not preserved"
fi
if [ -z "${STAGED_BINARY:-}" ]; then
    pass "the preserved binary is out of the staging cleanup's reach"
else
    fail "cleanup would still delete the preserved binary at '$STAGED_BINARY'"
fi

echo "§3 a failed promote must not leave the node with no bootable binary"

STUB_CURL_RC=7
STUB_HTTP_CODE=000
reset_node
STUB_MV_FAIL=1
_recover_or_preserve_past_migration_floor >>"$TMP_ROOT/out" 2>&1 || true
if [ -e "$RECOVERY" ] && [ "$(cat "$RECOVERY")" = "STAGED-NEW" ]; then
    pass "the staged binary survived a failed promote"
else
    fail "a failed promote lost the only migration-capable binary"
fi
if [ -z "${STAGED_BINARY:-}" ]; then
    pass "cleanup cannot delete it afterwards"
else
    fail "cleanup would delete the last bootable binary at '$STAGED_BINARY'"
fi

echo "§4 no staged binary means no action at all"

STUB_CURL_RC=7
reset_node
rm -f "$STAGED_BINARY"
printf 'ONLY-BINARY' >"$REL_BINARY"
STAGED_BINARY=""
: >"$TMP_ROOT/calls"
_recover_or_preserve_past_migration_floor >>"$TMP_ROOT/out" 2>&1 || true
if [ "$(cat "$REL_BINARY")" = "ONLY-BINARY" ] && [ ! -s "$TMP_ROOT/calls" ]; then
    pass "no staged binary means no bootout and no swap"
else
    fail "the recovery acted with no staged binary"
fi

echo "§5 liveness: only a refused connection proves nothing owns the port"

STUB_CURL_RC=0
STUB_HTTP_CODE=200
if _release_runtime_is_serving "$REL_PORT"; then
    pass "a runtime answering on the port counts as serving"
else
    fail "a healthy runtime was reported as not serving"
fi
STUB_HTTP_CODE=503
if _release_runtime_is_serving "$REL_PORT"; then
    pass "a degraded-but-answering runtime still counts as serving"
else
    fail "a degraded runtime was reported as not serving — the gate would be skipped for a live runtime"
fi
# A wedged handler still holds the port and still owns the frontier.
STUB_CURL_RC=28
if _release_runtime_is_serving "$REL_PORT"; then
    pass "a health handler that times out still counts as serving"
else
    fail "a timeout was read as absence — a live runtime would be stopped without its durability proof"
fi
STUB_CURL_RC=7
if _release_runtime_is_serving "$REL_PORT"; then
    fail "a crash-looping runtime was reported as serving — the node could never recover"
else
    pass "a refused connection counts as not serving"
fi
STUB_CURL_RC=0
if _release_runtime_is_serving ""; then
    fail "an empty port was treated as serving"
else
    pass "an unresolved port counts as not serving"
fi
if extract_function _release_runtime_is_serving | grep -v "^[[:space:]]*#" | grep -q "kill -0"; then
    fail "liveness uses kill -0, which a launchd-respawned crash loop always satisfies"
else
    pass "liveness does not rely on pid existence"
fi

echo "§6 the floor detector answers a fact, not a rollback policy"

if extract_function _migration_floor_may_advance | grep -q "AGENTDESK_DEPLOY_FORCE_ROLLBACK"; then
    fail "a rollback policy override can disarm floor detection"
else
    pass "no rollback override reaches the floor detector"
fi
if extract_function _rollback_would_brick_on_migration | grep -q "_migration_floor_may_advance"; then
    pass "the rollback guard reuses the one detector instead of duplicating the comparison"
else
    fail "the migration comparison is duplicated between the guard and the detector"
fi

echo "§7 the deploy arms before the migration runs and recovers before cleanup"

before_call="$(awk '/release-migrate-postgres; then/{exit} {print}' "$DEPLOY_SH" || true)"
if printf '%s\n' "$before_call" | grep -q "MIGRATION_FLOOR_ARMED=1"; then
    pass "the floor is armed before the migration is attempted"
else
    fail "arming happens only after a successful rc — a partial apply would brick the node"
fi
if printf '%s\n' "$before_call" | tail -20 | grep -q "_migration_floor_may_advance"; then
    pass "arming is gated on the factual detector"
else
    fail "arming is not gated on the migration-floor detector"
fi

cleanup_body="$(awk '/^_cleanup_on_exit\(\) \{/{p=1} p{print} p&&/^\}$/{exit}' "$DEPLOY_SH")"
recover_line=$(printf '%s\n' "$cleanup_body" | grep -n "_recover_or_preserve_past_migration_floor" | head -1 | cut -d: -f1 || true)
rm_line=$(printf '%s\n' "$cleanup_body" | grep -n 'rm -f "\$STAGED_BINARY"' | head -1 | cut -d: -f1 || true)
if [ -n "$recover_line" ] && [ -n "$rm_line" ] && [ "$recover_line" -lt "$rm_line" ]; then
    pass "recovery runs before the staged binary is deleted"
else
    fail "recovery is missing from the EXIT trap or runs after the staged binary is deleted (recover=${recover_line:-none} rm=${rm_line:-none})"
fi
if printf '%s\n' "$cleanup_body" | grep -q 'MIGRATION_FLOOR_ARMED:-0.*= 1'; then
    pass "the EXIT trap gates recovery on the migration floor"
else
    fail "the EXIT trap does not check MIGRATION_FLOOR_ARMED"
fi

if [ "$FAILURES" -gt 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all sections passed"
