#!/usr/bin/env bash
# A binary reporting the O writer on must not auto-roll back to a non-O-writer build nor be
# replaced by an external artifact; switches come from `agentdesk build-info`, never source.

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
    _binary_build_flag _manifest_o_tui_writer \
    _rollback_would_revert_o_ledger _binary_would_revert_o_ledger \
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
# The rollback also restores policies through the helper deploy-release.sh sources.
POLICY_HELPER="$(dirname "$DEPLOY_SH")/_deploy_policy_generation.sh"
# shellcheck source=/dev/null
if [ -f "$POLICY_HELPER" ]; then . "$POLICY_HELPER"; fi

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

# $1 path, $2 marker, $3 `build-info` stdout ("" = a binary without the subcommand).
make_binary() {
    {
        printf '#!/bin/sh\n# %s\n' "$2"
        if [ -n "$3" ]; then
            printf 'if [ "$1" = build-info ]; then printf "%%s\\n" %s; exit 0; fi\n' "'$3'"
        fi
        printf 'exit 2\n'
    } >"$1"
    chmod +x "$1"
}

marker() { sed -n 2p "$1" 2>/dev/null; }

# $1 new binary's build-info ("" = none), $2 manifest JSON ("" = none).
setup_case() {
    clean_paths "$ADK_REL" "$REPO" "$TMP_ROOT/calls"
    mkdir -p "$ADK_REL/bin" "$ADK_REL/runtime" "$REPO"
    make_binary "$REL_BINARY" new "$1"
    make_binary "$REL_BINARY_BACKUP" old ""
    [ -z "$2" ] || printf '%s\n' "$2" >"$MANIFEST"
}

# $1 label, $2 expected: rolled_back | refused
expect() {
    local label="$1" expected="$2" out actual
    out="$(_rollback_release_binary 2>&1)"
    if [ "$(marker "$REL_BINARY")" = "# old" ] && [ ! -e "$REL_BINARY_BACKUP" ]; then
        actual=rolled_back
    elif [ "$(marker "$REL_BINARY")" = "# new" ] && [ "$(marker "$REL_BINARY_BACKUP")" = "# old" ] \
        && ! grep -q bootout "$TMP_ROOT/calls" 2>/dev/null \
        && printf '%s' "$out" | grep -q "ROLLBACK REFUSED"; then
        actual=refused
    else
        actual=other
    fi
    if [ "$actual" = "$expected" ]; then pass "$label → $expected"; else fail "$label → expected $expected, got $actual: $out"; fi
}

SWITCH_OFF='{"schema":1,"o_tui_writer":false,"o_ledger_operator_resume":false}'
SWITCH_ON='{"schema":1,"o_tui_writer":true,"o_ledger_operator_resume":false}'
CAPABLE='{"schema":1,"o_tui_writer":false,"o_ledger_operator_resume":true}'
OLD_MANIFEST='{"repo_head":"abc","latest_postgres_migration":"0001_init.sql"}'
LEGACY_MANIFEST='{"repo_head":"abc","o_tui_writer":"false"}'
O_MANIFEST='{"repo_head":"abc","o_tui_writer":"true"}'

echo "== switch off: rollback unchanged =="
setup_case "$SWITCH_OFF" "$OLD_MANIFEST"
expect "switch off, old-format manifest" rolled_back
setup_case "$SWITCH_OFF" ""
expect "switch off, no manifest" rolled_back

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
setup_case "" "$LEGACY_MANIFEST"
expect "binary without build-info" refused
setup_case '{"schema":1,"o_ledger_operator_resume":false}' "$LEGACY_MANIFEST"
expect "build-info without the switch (constant removed)" refused
setup_case '{"schema":1,"o_tui_writer":"false"}' "$LEGACY_MANIFEST"
expect "non-boolean switch" refused
setup_case '{"schema":2,"o_tui_writer":false}' "$LEGACY_MANIFEST"
expect "unknown build-info schema" refused
setup_case 'not json' "$LEGACY_MANIFEST"
expect "unparseable build-info" refused

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
if ! grep -qx "if _external_artifact_would_skip_o_writer; then" <<<"$selection"; then
    fail "the binary selection range does not run _external_artifact_would_skip_o_writer"
    exit 1
fi
if grep -qE '_source_o_|_source_would_revert|const O_TUI_WRITER|OPERATOR_RESUME_SUPPORTED|src/services/tui_o' "$DEPLOY_SH"; then
    fail "deploy-release.sh still reads Rust source for a build switch"
fi
# The floor check runs on the selected binary once it exists, before anything is staged.
floor_check="$(awk '
    /^[[:space:]]*\[ -e "\$SOURCE_BINARY" \] && touch "\$SOURCE_BINARY"$/ { collecting = 1; next }
    collecting && /^# Rebuild dashboard so deploy never ships a stale dist\.$/ { print body; exit }
    collecting { body = body $0 ORS }
' "$DEPLOY_SH")"
if ! grep -qx 'if _binary_would_revert_o_ledger "$SOURCE_BINARY"; then' <<<"$floor_check" \
    || ! bash -n <<<"${floor_check#fi}" 2>/dev/null; then
    fail "the build floor check after binary selection is missing or invalid"
    exit 1
fi
_resolve_default_release_binary() {
    echo "select own-build" >>"$TMP_ROOT/calls"
    echo "own-build"
}
run_selection() {
    AGENTDESK_DEPLOY_BINARY="$1" REPO="$REPO" ADK_REL="$ADK_REL" TMP_ROOT="$TMP_ROOT" \
        DEPLOY_BUILD_PROFILE=release bash -euo pipefail -c "$(declare -f _binary_build_flag \
        _external_artifact_would_skip_o_writer _resolve_default_release_binary)
$selection
echo \"\$SOURCE_BINARY\""
}
ARTIFACT="$TMP_ROOT/artifact"
# $1 label, $2 artifact build-info ("" = none), $3 deploy an artifact (yes|no), $4 expected
expect_binary() {
    local actual deploy_binary=""
    setup_case "$SWITCH_OFF" ""
    make_binary "$ARTIFACT" artifact "$2"
    [ "$3" = no ] || deploy_binary="$ARTIFACT"
    actual="$(run_selection "$deploy_binary" 2>/dev/null)" || actual=refused
    [ "$actual" != "$ARTIFACT" ] || actual=artifact
    if [ "$actual" = "$4" ]; then pass "$1 → $4"; else fail "$1 → expected $4, got $actual"; fi
}
run_floor_check() {
    SOURCE_BINARY="$1" ADK_REL="$ADK_REL" bash -euo pipefail -c "$(declare -f _binary_build_flag \
        _binary_would_revert_o_ledger)
${floor_check#fi}
echo staged"
}
# $1 label, $2 built binary's build-info ("" = none), $3 floor (yes|no), $4 expected: staged | refused
expect_build_floor() {
    local actual status=0
    setup_case "$SWITCH_OFF" ""
    make_binary "$ARTIFACT" built "$2"
    if [ "$3" = yes ]; then
        mkdir -p "$ADK_REL/o_store"
        printf '1\n' >"$ADK_REL/o_store/operator_resume.floor"
    fi
    actual="$(run_floor_check "$ARTIFACT" 2>"$TMP_ROOT/floor-error")" || status=$?
    if [ "$4" = refused ]; then
        if [ "$status" -eq 1 ] && [ -z "$actual" ] \
            && grep -q "OperatorResume floor refuses" "$TMP_ROOT/floor-error"; then
            pass "$1 → refused before staging"
        else
            fail "$1 → expected refusal, got rc=$status output='$actual'"
        fi
    elif [ "$status" -eq 0 ] && [ "$actual" = staged ]; then
        pass "$1 → staged"
    else
        fail "$1 → expected staged, got rc=$status output='$actual'"
    fi
}
expect_binary "switch on, external artifact" "$SWITCH_ON" yes refused
expect_binary "artifact without build-info" "" yes refused
expect_binary "capable artifact even with O switch off" "$CAPABLE" yes refused
expect_binary "switch off, external artifact" "$SWITCH_OFF" yes artifact
expect_binary "own build" "$SWITCH_ON" no own-build

echo "== the manifest records the installed binary's switch for the next deploy's guard =="
for value in true false; do
    setup_case "{\"schema\":1,\"o_tui_writer\":${value},\"o_ledger_operator_resume\":false}" ""
    _write_release_source_manifest >/dev/null 2>&1 || true
    recorded="$(_manifest_o_tui_writer || true)"
    if [ "$recorded" = "$value" ]; then pass "manifest records ${value}"; else fail "manifest records ${value}: got '${recorded}'"; fi
done

echo "== the manifest records the installed binary's OperatorResume capability =="
setup_case "$CAPABLE" ""
_write_release_source_manifest >/dev/null 2>&1 || true
if python3 - "$MANIFEST" <<'PY'
import json
import sys
assert json.load(open(sys.argv[1]))["o_ledger_operator_resume"] is True
PY
then pass "manifest records OperatorResume capability"; else fail "manifest misses capability"; fi
setup_case "" ""
_write_release_source_manifest >/dev/null 2>&1 || true
recorded="$(_manifest_o_tui_writer || true)"
if [ "$recorded" = unknown ]; then pass "binary without build-info records unknown"; else fail "binary without build-info records '$recorded'"; fi

echo "== the OperatorResume floor reads the built binary =="
expect_build_floor "floor, compatible build" "$CAPABLE" yes staged
expect_build_floor "no floor, legacy build" "$SWITCH_OFF" no staged
expect_build_floor "floor, legacy build" "$SWITCH_OFF" yes refused
expect_build_floor "floor, build without build-info" "" yes refused
expect_build_floor "floor, build without the capability field" '{"schema":1,"o_tui_writer":true}' yes refused

echo "== the binary reports every switch this script reads =="
for field in o_tui_writer o_ledger_operator_resume; do
    if grep -q "\"$field\":" "$REPO_ROOT/src/cli/build_info.rs"; then
        pass "build-info reports $field"
    else
        fail "build-info no longer reports $field; every guard reading it would refuse"
    fi
done

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES failure(s)" >&2
    exit 1
fi
echo "all O writer rollback guard checks passed"
