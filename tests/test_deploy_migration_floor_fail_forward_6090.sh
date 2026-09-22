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
    _migration_floor_artifact_path _migration_floor_may_advance; do
    body="$(extract_function "$fn")"
    if [ -z "$body" ]; then
        fail "$fn is not defined in $DEPLOY_SH"
        echo "$FAILURES failure(s)" >&2
        exit 1
    fi
    eval "$body"
done

if ! declare -F _release_runtime_is_serving >/dev/null; then
    fail "_release_runtime_is_serving is not defined in scripts/_defaults.sh"
    echo "$FAILURES failure(s)" >&2
    exit 1
fi

# Emulates curl closely enough that -f/--fail and the exit code both matter: a
# probe that only accepts rc 0 would look correct here and ship a live-runtime bug.
curl() {
    local fail_on_http=0 a
    for a in "$@"; do
        case "$a" in --fail) fail_on_http=1 ;; --*) ;; -*f*) fail_on_http=1 ;; esac
    done
    local rc="${STUB_CURL_RC:-0}"
    if [ -n "${STUB_CURL_SEQ:-}" ]; then
        rc="${STUB_CURL_SEQ%% *}"
        case "$STUB_CURL_SEQ" in *" "*) STUB_CURL_SEQ="${STUB_CURL_SEQ#* }" ;; esac
    fi
    [ "$rc" = 0 ] || return "$rc"
    printf '%s' "${STUB_HTTP_CODE:-200}"
    if [ "$fail_on_http" = 1 ] && [ "${STUB_HTTP_CODE:-200}" -ge 400 ]; then
        return 22
    fi
    return 0
}
launchctl() {
    if [ "${1:-}" = "print" ]; then
        [ -n "${STUB_JOB_PID:-}" ] || return 1
        printf '\tstate = running\n\tpid = %s\n\tlast exit code = 0\n' "$STUB_JOB_PID"
        return 0
    fi
    echo "launchctl $*" >>"$TMP_ROOT/calls"
    return 0
}
chflags() { echo "chflags $*" >>"$TMP_ROOT/calls"; return 0; }
tmux() { echo "tmux $*" >>"$TMP_ROOT/calls"; return 0; }
xattr() { return 0; }
kill() {
    case "$*" in
        "-0 4242") return "${STUB_PID_ALIVE:-1}" ;;
        "-0 "*) return "${STUB_LOCKPID_ALIVE:-1}" ;;
    esac
    echo "kill $*" >>"$TMP_ROOT/calls"
    return 0
}
cp() { [ "${STUB_CP_FAIL:-0}" = 1 ] && return 1; command cp "$@"; }
sleep() { return 0; }
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
mkdir -p "$ADK_REL/runtime"
# shellcheck disable=SC2034  # named by the forbidden-reference check in §4
LOCK_FILE="$ADK_REL/runtime/dcserver.lock"

reset_node() {
    rm -f "$REL_BINARY" "$REL_BINARY_BACKUP" "$RECOVERY" "$ADK_REL/bin/agentdesk.deploy.test"
    printf 'OLD-UNBOOTABLE' >"$REL_BINARY"
    STAGED_BINARY="$ADK_REL/bin/agentdesk.deploy.test"
    printf 'STAGED-NEW' >"$STAGED_BINARY"
    rm -f "$ADK_REL"/bin/agentdesk.migration-floor-recovery* "$ADK_REL"/bin/agentdesk.pre-migration-floor*
    : >"$TMP_ROOT/calls"
    STUB_MV_FAIL=0
    STUB_CURL_SEQ=""
    STUB_PID_ALIVE=1
    STUB_JOB_PID=""
    STUB_CP_FAIL=0
    # shellcheck disable=SC2034  # Read by the production function loaded through eval.
    OLD_PID="4242"
}

echo "§1 nothing serving: install the staged binary so the crash loop heals itself"

STUB_CURL_RC=7
STUB_HTTP_CODE=000
reset_node
_recover_or_preserve_past_migration_floor >"$TMP_ROOT/out" 2>&1 || true

if [ "$(cat "$REL_BINARY")" = "STAGED-NEW" ]; then
    pass "the staged binary is now the one a restart would load"
else
    fail "live binary is still '$(cat "$REL_BINARY")' — the node stays bricked"
fi
if [ -z "${STAGED_BINARY:-}" ]; then
    pass "STAGED_BINARY was cleared so the EXIT cleanup cannot delete the live binary"
else
    fail "STAGED_BINARY still points at '$STAGED_BINARY' after the install"
fi
if ! grep -qE "^(launchctl|tmux|kill)" "$TMP_ROOT/calls"; then
    pass "no process was stopped or started to install it"
else
    fail "recovery issued process control: $(tr '\n' ';' <"$TMP_ROOT/calls")"
fi
if [ ! -e "$REL_BINARY_BACKUP" ]; then
    pass "the unbootable binary was not recorded as last-known-good"
else
    fail ".prev was written with a binary that cannot boot"
fi
if ls "$ADK_REL"/bin/agentdesk.pre-migration-floor* >/dev/null 2>&1; then
    pass "the replaced binary was kept, in case the database never actually advanced"
else
    fail "the replaced binary was destroyed — nothing can boot if the schema did not advance"
fi

echo "§2 a serving runtime keeps its own image, and still gets a bootable binary on disk"

STUB_CURL_RC=0
STUB_HTTP_CODE=200
reset_node
_recover_or_preserve_past_migration_floor >>"$TMP_ROOT/out" 2>&1 || true

if [ "$(cat "$REL_BINARY")" = "STAGED-NEW" ]; then
    pass "installing under a serving runtime is allowed: a rename cannot disturb it"
else
    fail "the node was left holding '$(cat "$REL_BINARY")', which cannot boot"
fi
if ! grep -qE "^(launchctl|tmux|kill)" "$TMP_ROOT/calls"; then
    pass "the serving runtime was not stopped, so its in-flight frontier survives"
else
    fail "recovery touched a serving runtime: $(tr '\n' ';' <"$TMP_ROOT/calls")"
fi
if ls "$ADK_REL"/bin/agentdesk.pre-migration-floor* >/dev/null 2>&1; then
    pass "the binary it replaced was kept"
else
    fail "the replaced binary was destroyed"
fi

echo "§3 a draining or replacement process changes nothing: still installed, still untouched"

for scenario in draining replacement; do
    reset_node
    STUB_CURL_RC=7
    STUB_HTTP_CODE=000
    case "$scenario" in
        draining) STUB_PID_ALIVE=0 ;;
        replacement) STUB_PID_ALIVE=1; STUB_JOB_PID="9931" ;;
    esac
    _recover_or_preserve_past_migration_floor >>"$TMP_ROOT/out" 2>&1 || true
    if [ "$(cat "$REL_BINARY")" = "STAGED-NEW" ]; then
        pass "$scenario: the binary on disk can boot"
    else
        fail "$scenario: the node was left on '$(cat "$REL_BINARY")'"
    fi
    if ! grep -qE "^(launchctl|tmux|kill)" "$TMP_ROOT/calls"; then
        pass "$scenario: nothing was stopped"
    else
        fail "$scenario: a live process was acted on: $(tr '\n' ';' <"$TMP_ROOT/calls")"
    fi
done
STUB_JOB_PID=""

echo "§4 recovery has no destructive process control at all, whatever the stubs do"

recovery_src="$(extract_function _recover_or_preserve_past_migration_floor | grep -v "^[[:space:]]*#")"
for forbidden in bootout "kill-session" bootstrap "kill " LOCK_FILE dcserver.lock; do
    if grep -qF -- "$forbidden" <<<"$recovery_src"; then
        fail "recovery still references '$forbidden' — the sample-then-act window is back"
    else
        pass "recovery never references '$forbidden'"
    fi
done


echo "§9 the swap is refused when the replaced binary cannot be kept"

reset_node
STUB_CURL_RC=7
STUB_CP_FAIL=1
_recover_or_preserve_past_migration_floor >>"$TMP_ROOT/out" 2>&1 || true
if [ "$(cat "$REL_BINARY")" = "OLD-UNBOOTABLE" ]; then
    pass "a failed backup blocks the swap instead of destroying the old binary"
else
    fail "the old binary was destroyed although the backup failed"
fi
if ls "$ADK_REL"/bin/agentdesk.migration-floor-recovery* >/dev/null 2>&1; then
    pass "the staged binary is preserved on that refusal"
else
    fail "the refusal lost the migration-capable binary"
fi

echo "§10 no staged binary means no action at all"

STUB_CURL_RC=7
reset_node
rm -f "$STAGED_BINARY"
printf 'ONLY-BINARY' >"$REL_BINARY"
STAGED_BINARY=""
: >"$TMP_ROOT/calls"
_recover_or_preserve_past_migration_floor >>"$TMP_ROOT/out" 2>&1 || true
if [ "$(cat "$REL_BINARY")" = "ONLY-BINARY" ] && [ ! -s "$TMP_ROOT/calls" ]; then
    pass "no staged binary means no action at all"
else
    fail "the recovery acted with no staged binary"
fi

echo "§11 liveness: only a refused connection proves nothing owns the port"

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
# An unresolved port proves nothing, so it must not license skipping the gate.
if _release_runtime_is_serving ""; then
    pass "an unresolved port counts as serving, so the gate still runs"
else
    fail "an unresolved port was read as absence — the durability gate would be skipped blind"
fi
if extract_function _release_runtime_is_serving | grep -v "^[[:space:]]*#" | grep -q "kill -0"; then
    fail "liveness uses kill -0, which a launchd-respawned crash loop always satisfies"
else
    pass "liveness does not rely on pid existence"
fi

echo "§12 the floor detector answers a fact, not a rollback policy"

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

echo "§13 the deploy arms before the migration runs and recovers before cleanup"

before_call="$(awk '/release-migrate-postgres; then/{exit} {print}' "$DEPLOY_SH" || true)"
if grep -q "MIGRATION_FLOOR_ARMED=1" <<<"$before_call"; then
    pass "the floor is armed before the migration is attempted"
else
    fail "arming happens only after a successful rc — a partial apply would brick the node"
fi
if tail -20 <<<"$before_call" | grep -q "_migration_floor_may_advance"; then
    pass "arming is gated on the factual detector"
else
    fail "arming is not gated on the migration-floor detector"
fi

cleanup_body="$(awk '/^_cleanup_on_exit\(\) \{/{p=1} p{print} p&&/^\}$/{exit}' "$DEPLOY_SH")"
recover_line=$(grep -n "_recover_or_preserve_past_migration_floor" <<<"$cleanup_body" | head -1 | cut -d: -f1 || true)
rm_line=$(grep -n 'rm -f "\$STAGED_BINARY"' <<<"$cleanup_body" | head -1 | cut -d: -f1 || true)
if [ -n "$recover_line" ] && [ -n "$rm_line" ] && [ "$recover_line" -lt "$rm_line" ]; then
    pass "recovery runs before the staged binary is deleted"
else
    fail "recovery is missing from the EXIT trap or runs after the staged binary is deleted (recover=${recover_line:-none} rm=${rm_line:-none})"
fi
if grep -q 'MIGRATION_FLOOR_ARMED:-0.*= 1' <<<"$cleanup_body"; then
    pass "the EXIT trap gates recovery on the migration floor"
else
    fail "the EXIT trap does not check MIGRATION_FLOOR_ARMED"
fi

if [ "$FAILURES" -gt 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all sections passed"
