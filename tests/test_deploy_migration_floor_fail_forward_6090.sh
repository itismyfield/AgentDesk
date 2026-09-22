#!/usr/bin/env bash
# Regression test for #6090: aborting a deploy after Postgres migrations have been
# applied, but before the staged binary is promoted, must not leave the node on the
# old binary. sqlx refuses to boot a binary older than the applied migration, so
# launchd crash-loops it forever (mac-mini did this on migrations 113/116/120/122).
#
# #4348 already covers the post-promotion rollback direction. This file pins the
# pre-promotion abort direction and the gate liveness precondition that triggers it.

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

for fn in _fail_forward_promote_staged_binary _release_runtime_is_serving; do
    body="$(extract_function "$fn")"
    if [ -z "$body" ]; then
        fail "$fn is not defined in $DEPLOY_SH"
        echo "$FAILURES failure(s)" >&2
        exit 1
    fi
    eval "$body"
done

echo "§1 fail-forward promotes the staged binary instead of stranding the old one"

launchctl() { echo "launchctl $*" >>"$TMP_ROOT/calls"; return 0; }
chflags() { echo "chflags $*" >>"$TMP_ROOT/calls"; return 0; }
xattr() { return 0; }
_launchd_domain() { echo "gui/501"; }
start_release_tmux_fallback() { echo "tmux-fallback" >>"$TMP_ROOT/calls"; return 0; }
wait_for_http_service_health() { echo "health $*" >>"$TMP_ROOT/calls"; return 0; }

ADK_REL="$TMP_ROOT/rel"
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
PLIST_REL="com.agentdesk.test"
REL_PORT="18791"
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_HEALTH_RETRIES=1
# shellcheck disable=SC2034  # Read by the production function loaded through eval.
DEPLOY_HEALTH_DELAY_SECS=1
mkdir -p "$ADK_REL/bin"
REL_BINARY="$ADK_REL/bin/agentdesk"
REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"

printf 'OLD-UNBOOTABLE' >"$REL_BINARY"
STAGED_BINARY="$ADK_REL/bin/agentdesk.deploy.test"
printf 'STAGED-NEW' >"$STAGED_BINARY"
: >"$TMP_ROOT/calls"

_fail_forward_promote_staged_binary >"$TMP_ROOT/out" 2>&1 || true

if [ "$(cat "$REL_BINARY")" = "STAGED-NEW" ]; then
    pass "the staged binary is now live"
else
    fail "live binary is still '$(cat "$REL_BINARY")' — the node stays bricked"
fi
if [ ! -e "$ADK_REL/bin/agentdesk.deploy.test" ]; then
    pass "the staged path was consumed by the promote"
else
    fail "staged binary still present after promote"
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

echo "§2 fail-forward is a no-op when there is nothing staged to promote"

printf 'ONLY-BINARY' >"$REL_BINARY"
STAGED_BINARY=""
: >"$TMP_ROOT/calls"
_fail_forward_promote_staged_binary >>"$TMP_ROOT/out" 2>&1 || true
if [ "$(cat "$REL_BINARY")" = "ONLY-BINARY" ] && [ ! -s "$TMP_ROOT/calls" ]; then
    pass "no staged binary means no bootout and no swap"
else
    fail "fail-forward acted with no staged binary"
fi

echo "§3 gate liveness is probed on the port, not with kill -0"

# Emulates curl closely enough that -f/--fail changes the outcome: without it a
# probe that only accepts 2xx would look correct here and ship a live-runtime bug.
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
STUB_CURL_RC=0
STUB_HTTP_CODE=200
if _release_runtime_is_serving "$REL_PORT"; then
    pass "a runtime answering on the port counts as serving"
else
    fail "a healthy runtime was reported as not serving"
fi
# A degraded runtime still owns an in-flight frontier, so skipping the gate for it
# would drop the very guarantee the gate exists to enforce.
STUB_HTTP_CODE=503
if _release_runtime_is_serving "$REL_PORT"; then
    pass "a degraded-but-answering runtime still counts as serving"
else
    fail "a degraded runtime was reported as not serving — the durability gate would be skipped for a live runtime"
fi
STUB_CURL_RC=7
STUB_HTTP_CODE=000
if _release_runtime_is_serving "$REL_PORT"; then
    fail "a crash-looping runtime was reported as serving — the gate would still wait for an ack that can never arrive"
else
    pass "a runtime that never binds the port counts as not serving"
fi
STUB_CURL_RC=0
STUB_HTTP_CODE=200
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

echo "§4 the deploy wires the floor flag and runs fail-forward before staged cleanup"

arm_block="$(awk '/release-migrate-postgres; then/{found=1} found && /MIGRATION_FLOOR_CROSSED=1/{print; exit} found{buf=buf $0 "\n"} END{}' "$DEPLOY_SH" || true)"
if [ -n "$arm_block" ]; then
    pass "the floor is armed after release-migrate-postgres"
else
    fail "MIGRATION_FLOOR_CROSSED is never armed after release-migrate-postgres"
fi

# Arming unconditionally would make every pre-promotion abort promote forward,
# including deploys that shipped no migration and could safely keep the old binary.
arm_guard="$(awk '/release-migrate-postgres; then/{found=1} found && /MIGRATION_FLOOR_CROSSED=1/{exit} found{print}' "$DEPLOY_SH" || true)"
if printf '%s\n' "$arm_guard" | grep -q "_rollback_would_brick_on_migration"; then
    pass "arming is gated on Postgres actually moving past the live binary"
else
    fail "the floor is armed unconditionally — a no-migration deploy would also fail forward"
fi

cleanup_body="$(awk '/^_cleanup_on_exit\(\) \{/{p=1} p{print} p&&/^\}$/{exit}' "$DEPLOY_SH")"
forward_line=$(printf '%s\n' "$cleanup_body" | grep -n "_fail_forward_promote_staged_binary" | head -1 | cut -d: -f1 || true)
rm_line=$(printf '%s\n' "$cleanup_body" | grep -n 'rm -f "\$STAGED_BINARY"' | head -1 | cut -d: -f1 || true)
if [ -n "$forward_line" ] && [ -n "$rm_line" ] && [ "$forward_line" -lt "$rm_line" ]; then
    pass "fail-forward runs before the staged binary is deleted"
else
    fail "fail-forward is missing from the EXIT trap or runs after the staged binary is deleted (forward=${forward_line:-none} rm=${rm_line:-none})"
fi
if printf '%s\n' "$cleanup_body" | grep -q 'MIGRATION_FLOOR_CROSSED:-0.*= 1'; then
    pass "the EXIT trap gates fail-forward on the migration floor"
else
    fail "the EXIT trap does not check MIGRATION_FLOOR_CROSSED"
fi

if [ "$FAILURES" -gt 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all sections passed"
