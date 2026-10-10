#!/usr/bin/env bash
# A deploy whose source has the O writer on must not auto-roll back to a build
# that is not recorded as an O writer build, nor ship an external artifact instead
# of its own build. With the switch off, rollback and artifact deploys are unchanged.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Overridable so a mutation run can point the same assertions at a patched copy.
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-o-writer-rollback-test.XXXXXX")
trap 'clean_paths "$TMP_ROOT"' EXIT

clean_paths() {
    python3 - "$@" <<'PY_CLEAN'
import pathlib
import shutil
import sys
for name in sys.argv[1:]:
    path = pathlib.Path(name)
    if path.is_dir():
        shutil.rmtree(path)
    elif path.exists():
        path.unlink()
PY_CLEAN
}

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

for fn in _rollback_release_binary _signal_release_lock_pid _wait_release_stopped \
    _source_o_tui_writer _manifest_o_tui_writer \
    _source_o_ledger_operator_resume _rollback_would_revert_o_ledger _source_would_revert_o_ledger \
    _rollback_would_revert_o_writer _write_release_source_manifest \
    _external_artifact_would_skip_o_writer; do
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
launchctl() {
    echo "launchctl $*" >>"$TMP_ROOT/calls"
    [ "$1" != print ]
}
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
    clean_paths "$ADK_REL" "$REPO" "$TMP_ROOT/calls"
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
setup_case "$SWITCH_ON" '{"repo_head":"abc","o_tui_writer":"unknown"}'
expect "switch on, rollback target switch unknown" refused
setup_case "$SWITCH_ON" "$O_MANIFEST"
expect "switch on, O writer rollback target" rolled_back

echo "== OperatorResume floor applies only after first use =="
setup_case "$SWITCH_ON" "$O_MANIFEST"
expect "no floor, old O reader (first deployment failure)" rolled_back
for target in "$O_MANIFEST" "not json" "" \
    '{"o_tui_writer":"true","o_ledger_operator_resume":false}' \
    '{"o_tui_writer":"true","o_ledger_operator_resume":"true"}'; do
    setup_case "$SWITCH_OFF" "$target"
    mkdir -p "$ADK_REL/o_store"
    printf '1\n' >"$ADK_REL/o_store/operator_resume.floor"
    AGENTDESK_DEPLOY_FORCE_ROLLBACK=1 expect "floor, incompatible target: $target" refused
done
setup_case "$SWITCH_ON" '{"o_tui_writer":"true","o_ledger_operator_resume":true}'
mkdir -p "$ADK_REL/o_store"
printf '1\n' >"$ADK_REL/o_store/operator_resume.floor"
expect "floor, compatible target" rolled_back

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

echo "== switch on: only this source's own build is deployed =="
# Buffer the bounded selection range so a missing boundary cannot include later deploy steps.
selection="$(awk '
    /^_check_repo_remote_freshness$/ { collecting = 1; next }
    collecting && /^# Cluster peers are pinned to this commit,/ { print body; exit }
    collecting { body = body $0 ORS }
' "$DEPLOY_SH")"
if [ -z "$selection" ] || ! bash -n <<<"$selection" 2>/dev/null; then
    fail "the source floor and binary selection range is missing or invalid"
    exit 1
fi
for guard in _source_would_revert_o_ledger _external_artifact_would_skip_o_writer; do
    if ! grep -qx "if $guard; then" <<<"$selection"; then
        fail "the source floor and binary selection range does not run $guard"
        exit 1
    fi
done
_resolve_default_release_binary() {
    echo "select own-build" >>"$TMP_ROOT/calls"
    echo "own-build"
}
run_selection() {
    AGENTDESK_DEPLOY_BINARY="$1" REPO="$REPO" ADK_REL="$ADK_REL" TMP_ROOT="$TMP_ROOT" \
        DEPLOY_BUILD_PROFILE=release bash -euo pipefail -c "$(declare -f _source_o_tui_writer \
        _source_o_ledger_operator_resume _source_would_revert_o_ledger \
        _external_artifact_would_skip_o_writer _resolve_default_release_binary)
$selection
echo \"\$SOURCE_BINARY\""
}
# $1 label, $2 topology.rs body, $3 AGENTDESK_DEPLOY_BINARY, $4 expected binary or "refused"
expect_binary() {
    local actual
    setup_case "$2" ""
    actual="$(run_selection "$3" 2>/dev/null)" || actual=refused
    if [ "$actual" = "$4" ]; then pass "$1 → $4"; else fail "$1 → expected $4, got $actual"; fi
}
expect_source_floor() {
    local actual status=0 calls
    actual="$(run_selection "" 2>"$TMP_ROOT/selection-error")" || status=$?
    calls="$(cat "$TMP_ROOT/calls" 2>/dev/null || true)"
    if [ "$2" = refused ]; then
        if [ "$status" -eq 1 ] && [ -z "$actual" ] && [ -z "$calls" ] \
            && grep -q "OperatorResume floor refuses" "$TMP_ROOT/selection-error"; then
            pass "$1 → refused before binary selection"
        else
            fail "$1 → expected refusal before selection, got rc=$status output='$actual' calls='$calls'"
        fi
    elif [ "$status" -eq 0 ] && [ "$actual" = "$2" ] && [ "$calls" = "select own-build" ]; then
        pass "$1 → $2 with binary selection observed"
    else
        fail "$1 → expected $2 with selection, got rc=$status output='$actual' calls='$calls'"
    fi
}
expect_binary "switch on, external artifact" "$SWITCH_ON" /tmp/artifact refused
expect_binary "unreadable switch, external artifact" \
    'pub(crate) const O_TUI_WRITER: bool = cfg!(feature = "o");' /tmp/artifact refused
expect_binary "switch on, own build" "$SWITCH_ON" "" own-build
expect_binary "switch off, external artifact" "$SWITCH_OFF" /tmp/artifact /tmp/artifact
expect_binary "source without the switch, external artifact" "" /tmp/artifact /tmp/artifact
expect_binary "switch off, own build" "$SWITCH_OFF" "" own-build

echo "== the manifest records the switch for the next deploy's guard =="
for value in true false; do
    setup_case "pub(crate) const O_TUI_WRITER: bool = ${value};" ""
    _write_release_source_manifest >/dev/null 2>&1 || true
    recorded="$(_manifest_o_tui_writer || true)"
    if [ "$recorded" = "$value" ]; then pass "manifest records ${value}"; else fail "manifest records ${value}: got '${recorded}'"; fi
done

echo "== the capability records only this source's own reader and consumer =="
setup_case "$SWITCH_OFF" ""
mkdir -p "$REPO/src/services/tui_o/store"
printf 'pub const OPERATOR_RESUME_SUPPORTED: bool = true;\n' >"$REPO/src/services/tui_o/store/ledger.rs"
_write_release_source_manifest >/dev/null 2>&1 || true
if python3 - "$MANIFEST" <<'PY'
import json
import sys
assert json.load(open(sys.argv[1]))["o_ledger_operator_resume"] is True
PY
then pass "manifest records OperatorResume capability"; else fail "manifest misses capability"; fi
if AGENTDESK_DEPLOY_BINARY=/tmp/artifact _external_artifact_would_skip_o_writer; then
    pass "capable source refuses an external artifact even with O switch off"
else fail "external artifact could be assigned an unverified capability"; fi
mkdir -p "$ADK_REL/o_store"
printf '1\n' >"$ADK_REL/o_store/operator_resume.floor"
expect_source_floor "source floor, compatible source" own-build
setup_case "$SWITCH_OFF" ""
mkdir -p "$REPO/src/services/tui_o/store"
printf 'pub const OPERATOR_RESUME_SUPPORTED: bool = false;\n' >"$REPO/src/services/tui_o/store/ledger.rs"
expect_source_floor "no source floor, legacy source" own-build
for capability in missing false unknown unreadable; do
    setup_case "$SWITCH_OFF" ""
    mkdir -p "$ADK_REL/o_store" "$REPO/src/services/tui_o/store"
    printf '1\n' >"$ADK_REL/o_store/operator_resume.floor"
    case "$capability" in
        false) printf 'pub const OPERATOR_RESUME_SUPPORTED: bool = false;\n' \
            >"$REPO/src/services/tui_o/store/ledger.rs" ;;
        unknown) printf 'pub const OPERATOR_RESUME_SUPPORTED: bool = cfg!(feature = "o");\n' \
            >"$REPO/src/services/tui_o/store/ledger.rs" ;;
        unreadable) mkdir "$REPO/src/services/tui_o/store/ledger.rs" ;;
    esac
    expect_source_floor "source floor, $capability capability" refused
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
