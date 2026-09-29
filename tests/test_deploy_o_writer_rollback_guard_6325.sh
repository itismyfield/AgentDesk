#!/usr/bin/env bash
# A deploy whose source has the O writer on must not auto-roll back to a build
# that is not recorded as an O writer build: the old binary would hand Discord
# output back to the Legacy relay. With the switch off, rollback is unchanged.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-o-writer-rollback-test.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

FAILURES=0
fail() { echo "  ✗ $1" >&2; FAILURES=$((FAILURES + 1)); }
pass() { echo "  ✓ $1"; }

# Skips python heredoc bodies, whose closing brace would otherwise end the function.
extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && heredoc && /^PY$/ { heredoc = 0; next }
        printing && /<<.?PY.?$/ { heredoc = 1; next }
        printing && !heredoc && /^}$/ { exit }
    ' "$DEPLOY_SH"
}

for fn in _rollback_release_binary _source_o_tui_writer _manifest_o_tui_writer \
    _rollback_would_revert_o_writer _write_release_source_manifest; do
    body="$(extract_function "$fn")"
    if [ -z "$body" ] || ! bash -n <<<"$body" 2>/dev/null; then
        fail "$fn is not defined in $DEPLOY_SH"
        echo "$FAILURES failure(s)" >&2
        exit 1
    fi
    eval "$body"
done

# The migration guard has its own suite; here it always allows the rollback.
_rollback_would_brick_on_migration() { return 1; }
launchctl() { echo "launchctl $*" >>"$TMP_ROOT/calls"; return 0; }
chflags() { return 0; }
tmux() { return 0; }
xattr() { return 0; }
_launchd_domain() { echo "gui/501"; }
start_release_tmux_fallback() { return 0; }
wait_for_http_service_health() { return 0; }
_latest_postgres_migration_path() { return 0; }
_sha256_file() { return 0; }

# shellcheck disable=SC2034  # Read by the production functions loaded through eval.
PLIST_REL="com.agentdesk.test"
# shellcheck disable=SC2034
REL_PORT="18791"
# shellcheck disable=SC2034
ADK_DEFAULT_LOOPBACK="127.0.0.1"
# shellcheck disable=SC2034
DEPLOY_HEALTH_RETRIES=1
# shellcheck disable=SC2034
DEPLOY_HEALTH_DELAY_SECS=1
# shellcheck disable=SC2034
DEPLOY_BUILD_PROFILE="release"
# shellcheck disable=SC2034
CODESIGN_IDENTITY=""
# shellcheck disable=SC2034
ALLOW_ADHOC_RELEASE_SIGN="0"
ADK_REL="$TMP_ROOT/rel"
REPO="$TMP_ROOT/src-repo"
REL_BINARY="$ADK_REL/bin/agentdesk"
REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
MANIFEST="$ADK_REL/runtime/release-source.json"

# $1 topology.rs body ("" = no switch in the source), $2 manifest JSON ("" = none).
setup_case() {
    rm -rf "$ADK_REL" "$REPO" "$TMP_ROOT/calls"
    mkdir -p "$ADK_REL/bin" "$ADK_REL/runtime" "$REPO/src/services/tui_o"
    printf 'new\n' >"$REL_BINARY"
    printf 'old\n' >"$REL_BINARY_BACKUP"
    [ -z "$1" ] || printf '%s\n' "$1" >"$REPO/src/services/tui_o/topology.rs"
    [ -z "$2" ] || printf '%s\n' "$2" >"$MANIFEST"
}

# $1 label, $2 expected: rolled_back | refused
expect() {
    local label="$1" expected="$2" out actual
    out="$(_rollback_release_binary 2>&1)"
    if [ "$(cat "$REL_BINARY")" = "old" ] && [ ! -e "$REL_BINARY_BACKUP" ]; then
        actual=rolled_back
    elif [ "$(cat "$REL_BINARY")" = "new" ] && [ "$(cat "$REL_BINARY_BACKUP")" = "old" ] \
        && ! grep -q bootout "$TMP_ROOT/calls" 2>/dev/null \
        && printf '%s' "$out" | grep -q "ROLLBACK REFUSED"; then
        actual=refused
    else
        actual=other
    fi
    if [ "$actual" = "$expected" ]; then pass "$label → $expected"; else fail "$label → expected $expected, got $actual: $out"; fi
}

SWITCH_OFF='pub(crate) const O_TUI_WRITER: bool = false;'
SWITCH_ON='pub(crate) const O_TUI_WRITER: bool = true;'
OLD_MANIFEST='{"repo_head":"abc","latest_postgres_migration":"0001_init.sql"}'
LEGACY_MANIFEST='{"repo_head":"abc","o_tui_writer":"false"}'
O_MANIFEST='{"repo_head":"abc","o_tui_writer":"true"}'

echo "== switch off: rollback unchanged =="
setup_case "$SWITCH_OFF" "$OLD_MANIFEST"
expect "switch off, old-format manifest" rolled_back
setup_case "" ""
expect "source without the switch, no manifest" rolled_back

echo "== switch on: the rollback target must be an O writer build =="
setup_case "$SWITCH_ON" "$OLD_MANIFEST"
expect "switch on, old-format manifest" refused
setup_case "$SWITCH_ON" "$LEGACY_MANIFEST"
expect "switch on, Legacy rollback target" refused
setup_case "$SWITCH_ON" ""
expect "switch on, no manifest" refused
setup_case "$SWITCH_ON" "not json"
expect "switch on, unreadable manifest" refused
setup_case "$SWITCH_ON" "$O_MANIFEST"
expect "switch on, O writer rollback target" rolled_back

echo "== an unreadable switch counts as on =="
setup_case 'pub(crate) const O_TUI_WRITER: bool = cfg!(feature = "o");' "$LEGACY_MANIFEST"
expect "non-literal switch" refused
setup_case "$SWITCH_OFF
// pub(crate) const O_TUI_WRITER: bool = false;" "$LEGACY_MANIFEST"
expect "switch plus a commented-out copy" refused
setup_case "#[cfg(test)]
$SWITCH_OFF
#[cfg(not(test))]
$SWITCH_ON" "$LEGACY_MANIFEST"
expect "two definitions that disagree" refused

echo "== the manifest records the switch for the next deploy's guard =="
for value in true false; do
    setup_case "pub(crate) const O_TUI_WRITER: bool = ${value};" ""
    _write_release_source_manifest >/dev/null 2>&1 || true
    recorded="$(_manifest_o_tui_writer || true)"
    if [ "$recorded" = "$value" ]; then pass "manifest records ${value}"; else fail "manifest records ${value}: got '${recorded}'"; fi
done

echo "== this repository's switch reads as one value =="
real="$(REPO="$REPO_ROOT" _source_o_tui_writer)"
case "$real" in
    true|false) pass "repository switch reads as $real" ;;
    *) fail "repository switch reads as '$real'; every O build's rollback would be refused" ;;
esac

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all O writer rollback guard checks passed"
