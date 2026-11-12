#!/usr/bin/env bash
# Isolated rollback and pre-stage entrypoints; never source or run the deploy script.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
TEST_ROOT=$(mktemp -d)
trap 'python3 -c "import shutil,sys; shutil.rmtree(sys.argv[1])" "$TEST_ROOT"' EXIT
extract_function() {
    awk -v start="^$1[(][)] [{]$" '
        $0 ~ start { printing=1 }
        printing { print }
        printing && heredoc && /^PY$/ { heredoc=0; next }
        printing && /<<.?PY.?$/ { heredoc=1; next }
        printing && !heredoc && /^}$/ { exit }
    ' "$DEPLOY_SH"
}
for fn in _binary_build_flag _binary_would_revert_o_child_store _rollback_would_revert_o_child_store _rollback_release_binary; do
    body="$(extract_function "$fn")"
    test -n "$body" || { echo "FAIL missing $fn"; exit 1; }
    eval "$body"
done
ADK_REL="$TEST_ROOT/release"
mkdir -p "$ADK_REL/o_store" "$ADK_REL/runtime"
REL_BINARY_BACKUP="$TEST_ROOT/target"
REL_BINARY="$TEST_ROOT/current"
PLIST_REL=fixture
ADK_DEFAULT_LOOPBACK=127.0.0.1
# Deliberately stale manifest cannot make an incompatible actual binary compatible.
printf '{"o_store_thread_children":true}\n' > "$ADK_REL/runtime/release-source.json"
_rollback_would_brick_on_migration() { return 1; }
_rollback_would_revert_o_writer() { return 1; }
_rollback_would_revert_o_ledger() { return 1; }
# A downstream guard is the harmless sentinel: accepted cases must reach it once.
_rollback_would_revert_o_spool() { echo downstream >> "$TEST_ROOT/calls"; return 0; }
_binary_would_revert_o_ledger() { return 1; }
write_binary() {
    printf '#!/bin/sh\nprintf '\''%%s\\n'\'' '\''%s'\''\n' "$1" > "$REL_BINARY_BACKUP"
    chmod +x "$REL_BINARY_BACKUP"
}
for kind in missing false oa unknown schema schema_bool field_num array error exec_error true; do
    case "$kind" in
        missing) info='{"schema":1}';;
        false|oa) info='{"schema":1,"o_store_thread_children":false}';;
        unknown) info='{"schema":1,"o_store_thread_children":"true"}';;
        schema) info='{"schema":2,"o_store_thread_children":true}';;
        schema_bool) info='{"schema":true,"o_store_thread_children":true}';;
        field_num) info='{"schema":1,"o_store_thread_children":1}';;
        array) info='[]';;
        exec_error) info='{"schema":1,"o_store_thread_children":true}';;
        error) info='invalid';;
        true) info='{"schema":1,"o_store_thread_children":true}';;
    esac
    write_binary "$info"
    if [ "$kind" = true ]; then printf '{"o_store_thread_children":false}\n' > "$ADK_REL/runtime/release-source.json"; fi
    if [ "$kind" = exec_error ]; then printf "exit 1\n" >> "$REL_BINARY_BACKUP"; fi
    for floor in absent regular directory symlink; do
        python3 - "$ADK_REL/o_store/thread_children.floor" "$floor" <<'PY'
import pathlib,sys,shutil
p=pathlib.Path(sys.argv[1])
if p.is_symlink() or p.is_file(): p.unlink()
elif p.is_dir(): shutil.rmtree(p)
if sys.argv[2]=='regular': p.write_text('1\n')
if sys.argv[2]=='directory': p.mkdir()
if sys.argv[2]=='symlink': p.symlink_to('missing-target')
PY
        : > "$TEST_ROOT/calls"
        out="$(_rollback_release_binary 2>&1)"
        if [ "$floor" != absent ] && [ "$kind" != true ]; then
            printf '%s' "$out" | grep -q 'ROLLBACK REFUSED.*child store'
            test ! -s "$TEST_ROOT/calls"
        else
            test "$(cat "$TEST_ROOT/calls")" = downstream
        fi
        : > "$TEST_ROOT/calls"
        # Execute the real common pre-stage block used by external, source and peer artifacts.
        block="$(sed -n '/^# The binary itself reports its O ledger reader/,/^# Rebuild dashboard/{ /^# Rebuild dashboard/!p; }' "$DEPLOY_SH")"
        if ( SOURCE_BINARY="$REL_BINARY_BACKUP"; eval "$block"; echo artifact_stage >> "$TEST_ROOT/calls" ) > "$TEST_ROOT/stage.out" 2>&1; then rc=0; else rc=$?; fi
        if [ "$floor" != absent ] && [ "$kind" != true ]; then
            test "$rc" -ne 0
            test ! -s "$TEST_ROOT/calls"
            grep -q 'child store' "$TEST_ROOT/stage.out"
        else
            test "$rc" -eq 0
            test "$(cat "$TEST_ROOT/calls")" = artifact_stage
        fi
    done
done
echo 'PASS child store guard: 44 rollback + 44 common artifact cases, no service operations'
