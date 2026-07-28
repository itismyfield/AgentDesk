#!/usr/bin/env bash
# Behavioral regression coverage for #4902 routine asset transactions.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=../scripts/routine-asset-surface.sh
. "$REPO_ROOT/scripts/routine-asset-surface.sh"

# The checked-in loader root itself is part of the contract: every tracked JS
# file under routines/ must be a QuickJS registration entrypoint, and each
# migrated helper must exist only on the sibling helper surface. This catches a
# future helper accidentally being moved back under the recursive loader root.
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    if [ -e "$REPO_ROOT/routines/$helper_ref" ]; then
        echo "FAIL: legacy helper returned to QuickJS loader root: $helper_ref" >&2
        exit 1
    fi
    if [ ! -f "$REPO_ROOT/routine-helpers/$helper_ref" ]; then
        echo "FAIL: migrated helper missing from sibling surface: $helper_ref" >&2
        exit 1
    fi
done
while IFS= read -r routine_ref; do
    if ! grep -Fq 'agentdesk.routines.register' "$REPO_ROOT/$routine_ref"; then
        echo "FAIL: tracked routines/ JS is not a QuickJS entrypoint: $routine_ref" >&2
        exit 1
    fi
done < <(git -C "$REPO_ROOT" ls-files 'routines/*.js' 'routines/**/*.js')

TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-routine-assets.XXXXXX")
trap 'adk_release_routine_asset_lock 2>/dev/null || true; rm -rf "$TMP_ROOT"' EXIT

fail_test() {
    echo "FAIL: $*" >&2
    exit 1
}

seed_required_helpers() {
    local helper_root="$1"
    local generation="$2"
    local helper_ref

    mkdir -p "$helper_root/monitoring"
    for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
        printf '%s:%s\n' "$generation" "$helper_ref" > "$helper_root/$helper_ref"
    done
    printf '%s\n' "$generation" > "$helper_root/generation-marker"
}

write_quickjs_routine() {
    local path="$1"
    local generation="$2"

    mkdir -p "$(dirname "$path")"
    printf 'agentdesk.routines.register({ name: "%s", tick() { return { action: "complete" }; } });\n' \
        "$generation" > "$path"
}

seed_source() {
    local root="$1"
    local generation="$2"

    seed_required_helpers "$root/routine-helpers" "$generation"
    write_quickjs_routine "$root/routines/monitoring/bundled.js" "$generation"
    printf '%s\n' "$generation" > "$root/routines/generation-marker"
}

seed_live() {
    local root="$1"
    local generation="$2"

    seed_required_helpers "$root/routine-helpers" "$generation"
    write_quickjs_routine "$root/routines/monitoring/bundled.js" "$generation"
    printf '%s\n' "$generation" > "$root/routines/generation-marker"
}

acquire_runtime_lock() {
    local runtime_root="$1"
    CURRENT_LOCK="$runtime_root/runtime/deploy-release.lock"
    adk_acquire_routine_asset_lock "$CURRENT_LOCK" 0
}

begin_staged_transaction() {
    local source_root="$1"
    local runtime_root="$2"

    CURRENT_TXN="$(
        adk_begin_routine_asset_transaction "$runtime_root" "$CURRENT_LOCK"
    )"
    adk_stage_routines "$source_root" "$runtime_root" "$CURRENT_TXN" >/dev/null
    adk_stage_routine_helpers "$source_root" "$runtime_root" "$CURRENT_TXN" >/dev/null
}

release_runtime_lock() {
    adk_release_routine_asset_lock
    CURRENT_LOCK=""
}

CURRENT_LOCK=""
CURRENT_TXN=""

# Unique stages preserve operator assets, exact tombstones, and root mode.
SOURCE_ROOT="$TMP_ROOT/repo-v1"
RUNTIME_ROOT="$TMP_ROOT/release-v0"
seed_source "$SOURCE_ROOT" 'v1'
seed_live "$RUNTIME_ROOT" 'v0'
# Make different same-size source/live bytes share mtimes so rsync's default
# quick-check cannot accidentally satisfy this authoritative-overlay regression.
touch -r "$RUNTIME_ROOT/routines/generation-marker" \
    "$SOURCE_ROOT/routines/generation-marker"
touch -r "$RUNTIME_ROOT/routines/monitoring/bundled.js" \
    "$SOURCE_ROOT/routines/monitoring/bundled.js"
touch -r "$RUNTIME_ROOT/routine-helpers/monitoring/weekly_churn_audit.py" \
    "$SOURCE_ROOT/routine-helpers/monitoring/weekly_churn_audit.py"
printf 'operator helper\n' > "$RUNTIME_ROOT/routine-helpers/operator-private.py"
write_quickjs_routine "$RUNTIME_ROOT/routines/monitoring/operator.js" 'operator'
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    mkdir -p "$(dirname "$RUNTIME_ROOT/routines/$helper_ref")"
    printf 'legacy helper\n' > "$RUNTIME_ROOT/routines/$helper_ref"
done
chmod 0710 "$RUNTIME_ROOT/routine-helpers"
acquire_runtime_lock "$RUNTIME_ROOT"
begin_staged_transaction "$SOURCE_ROOT" "$RUNTIME_ROOT"

[ "$CURRENT_TXN/staged/routines" != "$RUNTIME_ROOT/routines.new" ] \
    && [ -d "$CURRENT_TXN/staged/routines" ] \
    && [ -d "$CURRENT_TXN/staged/routine-helpers" ] \
    || fail_test 'transaction did not use unique owned stage paths'
[ -f "$CURRENT_TXN/staged/routine-helpers/operator-private.py" ] \
    || fail_test 'helper staging erased an operator-private asset'
[ -f "$CURRENT_TXN/staged/routines/monitoring/operator.js" ] \
    || fail_test 'routine staging erased an operator-private entrypoint'
cmp "$SOURCE_ROOT/routines/generation-marker" \
    "$CURRENT_TXN/staged/routines/generation-marker" >/dev/null \
    && cmp "$SOURCE_ROOT/routines/monitoring/bundled.js" \
        "$CURRENT_TXN/staged/routines/monitoring/bundled.js" >/dev/null \
    && cmp "$SOURCE_ROOT/routine-helpers/monitoring/weekly_churn_audit.py" \
        "$CURRENT_TXN/staged/routine-helpers/monitoring/weekly_churn_audit.py" >/dev/null \
    || fail_test 'source overlay lost authoritative bytes at equal size and mtime'
[ "$(_adk_path_mode "$CURRENT_TXN/staged/routine-helpers")" = \
    "$(_adk_path_mode "$RUNTIME_ROOT/routine-helpers")" ] \
    || fail_test 'staged helper root did not preserve live mode'
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    [ ! -e "$CURRENT_TXN/staged/routines/$helper_ref" ] \
        || fail_test "legacy helper survived exact tombstone: $helper_ref"
done

adk_promote_routine_asset_transaction "$RUNTIME_ROOT" "$CURRENT_TXN"
[ "$(<"$RUNTIME_ROOT/routines/generation-marker")" = 'v1' ] \
    && [ "$(<"$RUNTIME_ROOT/routines.old/generation-marker")" = 'v0' ] \
    && [ "$(<"$RUNTIME_ROOT/routine-helpers/generation-marker")" = 'v1' ] \
    || fail_test 'paired promotion did not retain the exact v0 rollback generation'
adk_commit_routine_asset_transaction "$RUNTIME_ROOT" "$CURRENT_TXN"
[ ! -e "$RUNTIME_ROOT/routines.old" ] \
    && [ ! -e "$RUNTIME_ROOT/routine-helpers.old" ] \
    && [ ! -e "$RUNTIME_ROOT/runtime/routine-assets.active" ] \
    || fail_test 'healthy commit retained transaction state'
release_runtime_lock

# Once health has durably selected commit, retry/recovery must retain the new
# generation instead of rolling assets back beside the proven new binary.
INTENT_RUNTIME="$TMP_ROOT/commit-intent-release"
INTENT_SOURCE="$TMP_ROOT/commit-intent-source"
seed_live "$INTENT_RUNTIME" 'v0'
seed_source "$INTENT_SOURCE" 'v1'
acquire_runtime_lock "$INTENT_RUNTIME"
begin_staged_transaction "$INTENT_SOURCE" "$INTENT_RUNTIME"
INTENT_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$INTENT_RUNTIME" "$INTENT_TXN"
adk_mark_routine_asset_transaction_committing "$INTENT_RUNTIME" "$INTENT_TXN"
adk_recover_active_routine_asset_transaction "$INTENT_RUNTIME"
[ "$(<"$INTENT_RUNTIME/routines/generation-marker")" = 'v1' ] \
    && [ "$(<"$INTENT_RUNTIME/routine-helpers/generation-marker")" = 'v1' ] \
    && [ ! -e "$INTENT_RUNTIME/routines.old" ] \
    && [ ! -e "$INTENT_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'durable commit intent recovered by rolling back the new generation'
release_runtime_lock

# v1 -> v2 commit cleanup failure leaves a durable `committing` transaction.
# The next lock holder must finish that commit, then v3 rollback must restore v2
# (the immediately previous live), never stale v1.
GEN_RUNTIME="$TMP_ROOT/generation-release"
GEN_V2_SOURCE="$TMP_ROOT/generation-v2"
GEN_V3_SOURCE="$TMP_ROOT/generation-v3"
seed_live "$GEN_RUNTIME" 'v1'
seed_source "$GEN_V2_SOURCE" 'v2'
seed_source "$GEN_V3_SOURCE" 'v3'
acquire_runtime_lock "$GEN_RUNTIME"
begin_staged_transaction "$GEN_V2_SOURCE" "$GEN_RUNTIME"
V2_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$GEN_RUNTIME" "$V2_TXN"

COMMIT_FAIL_PATH="$GEN_RUNTIME/routine-helpers.old"
rm() {
    local arg
    for arg in "$@"; do
        [ "$arg" != "$COMMIT_FAIL_PATH" ] || return 76
    done
    command rm "$@"
}
set +e
adk_commit_routine_asset_transaction "$GEN_RUNTIME" "$V2_TXN"
COMMIT_FAIL_STATUS=$?
set -e
unset -f rm
[ "$COMMIT_FAIL_STATUS" -ne 0 ] \
    && [ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v2' ] \
    && [ "$(<"$GEN_RUNTIME/routines.old/generation-marker")" = 'v1' ] \
    && [ "$(<"$V2_TXN/phase")" = 'committing' ] \
    || fail_test 'cleanup failure did not retain live v2 plus durable v1 cleanup state'
release_runtime_lock

# First recovery attempt fails the same cleanup again and must not manufacture
# or promote a new stage. The retry succeeds and bases v3 on authoritative v2.
acquire_runtime_lock "$GEN_RUNTIME"
rm() {
    local arg
    for arg in "$@"; do
        [ "$arg" != "$COMMIT_FAIL_PATH" ] || return 77
    done
    command rm "$@"
}
set +e
FAILED_BEGIN="$(
    adk_begin_routine_asset_transaction "$GEN_RUNTIME" "$CURRENT_LOCK"
)"
FAILED_BEGIN_STATUS=$?
set -e
unset -f rm
[ "$FAILED_BEGIN_STATUS" -ne 0 ] && [ -z "$FAILED_BEGIN" ] \
    && [ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v2' ] \
    || fail_test 'failed committing recovery changed authoritative live v2'

begin_staged_transaction "$GEN_V3_SOURCE" "$GEN_RUNTIME"
V3_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$GEN_RUNTIME" "$V3_TXN"
[ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v3' ] \
    && [ "$(<"$GEN_RUNTIME/routines.old/generation-marker")" = 'v2' ] \
    && [ "$(<"$GEN_RUNTIME/routine-helpers.old/generation-marker")" = 'v2' ] \
    || fail_test 'v3 transaction used stale v1 instead of immediate live v2'
adk_rollback_routine_asset_transaction "$GEN_RUNTIME" "$V3_TXN"
adk_rollback_routine_asset_transaction "$GEN_RUNTIME" "$V3_TXN"
[ "$(<"$GEN_RUNTIME/routines/generation-marker")" = 'v2' ] \
    && [ "$(<"$GEN_RUNTIME/routine-helpers/generation-marker")" = 'v2' ] \
    && [ ! -e "$GEN_RUNTIME/routines.old" ] \
    || fail_test 'v3 health rollback or double rollback failed to preserve v2'
release_runtime_lock

# TERM after live->old but before mv returns is recoverable because `armed` was
# durable before the first rename. The mv shim performs the rename, then returns
# a signal-like failure to force the exact interruption window.
SIGNAL_RUNTIME="$TMP_ROOT/signal-release"
SIGNAL_SOURCE="$TMP_ROOT/signal-source"
seed_live "$SIGNAL_RUNTIME" 'v0'
seed_source "$SIGNAL_SOURCE" 'v1'
acquire_runtime_lock "$SIGNAL_RUNTIME"
begin_staged_transaction "$SIGNAL_SOURCE" "$SIGNAL_RUNTIME"
SIGNAL_TXN="$CURRENT_TXN"
SIGNAL_FROM="$SIGNAL_RUNTIME/routines"
SIGNAL_TO="$SIGNAL_RUNTIME/routines.old"
SAW_DURABLE_ARM=0
mv() {
    if [ "$#" -eq 2 ] && [ "$1" = "$SIGNAL_FROM" ] && [ "$2" = "$SIGNAL_TO" ]; then
        [ "$(<"$SIGNAL_TXN/phase")" = 'armed' ] && SAW_DURABLE_ARM=1
        command mv "$@"
        return 143
    fi
    command mv "$@"
}
set +e
adk_promote_routine_asset_transaction "$SIGNAL_RUNTIME" "$SIGNAL_TXN"
SIGNAL_STATUS=$?
set -e
unset -f mv
[ "$SIGNAL_STATUS" -ne 0 ] && [ "$SAW_DURABLE_ARM" = 1 ] \
    && [ ! -e "$SIGNAL_RUNTIME/routines" ] \
    && [ "$(<"$SIGNAL_RUNTIME/routines.old/generation-marker")" = 'v0' ] \
    && [ "$(<"$SIGNAL_TXN/phase")" = 'armed' ] \
    && [ -e "$SIGNAL_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'signal-window state was not durably recoverable'
adk_rollback_routine_asset_transaction "$SIGNAL_RUNTIME" "$SIGNAL_TXN"
[ "$(<"$SIGNAL_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ ! -e "$SIGNAL_RUNTIME/routines.old" ] \
    && [ ! -e "$SIGNAL_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'signal-window rollback did not recover v0'
release_runtime_lock

# An interrupted rollback preserves live/new in swap-current while old/v0 is
# still available. A second call converges without deleting either sole copy.
RETRY_RUNTIME="$TMP_ROOT/retry-release"
RETRY_SOURCE="$TMP_ROOT/retry-source"
seed_live "$RETRY_RUNTIME" 'v0'
seed_source "$RETRY_SOURCE" 'v1'
acquire_runtime_lock "$RETRY_RUNTIME"
begin_staged_transaction "$RETRY_SOURCE" "$RETRY_RUNTIME"
RETRY_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$RETRY_RUNTIME" "$RETRY_TXN"
ROLLBACK_FAIL_FROM="$RETRY_RUNTIME/routines.old"
ROLLBACK_FAIL_TO="$RETRY_RUNTIME/routines"
mv() {
    if [ "$#" -eq 2 ] && [ "$1" = "$ROLLBACK_FAIL_FROM" ] \
      && [ "$2" = "$ROLLBACK_FAIL_TO" ]; then
        return 78
    fi
    command mv "$@"
}
set +e
adk_rollback_routine_asset_transaction "$RETRY_RUNTIME" "$RETRY_TXN"
RETRY_FAIL_STATUS=$?
set -e
unset -f mv
[ "$RETRY_FAIL_STATUS" -ne 0 ] \
    && [ "$(<"$RETRY_RUNTIME/routines/generation-marker")" = 'v1' ] \
    && [ "$(<"$RETRY_RUNTIME/routines.old/generation-marker")" = 'v0' ] \
    && [ "$(<"$RETRY_TXN/phase")" = 'rolling-back' ] \
    || fail_test 'failed rollback did not preserve both v0 and v1 copies'
adk_rollback_routine_asset_transaction "$RETRY_RUNTIME" "$RETRY_TXN"
[ "$(<"$RETRY_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ ! -e "$RETRY_RUNTIME/routines.old" ] \
    && [ ! -e "$RETRY_RUNTIME/routines.swap-current" ] \
    || fail_test 'rollback retry failed to converge to v0'
release_runtime_lock

# Fresh surfaces roll back to absence, and a second rollback is a no-op.
FRESH_RUNTIME="$TMP_ROOT/fresh-release"
FRESH_SOURCE="$TMP_ROOT/fresh-source"
seed_source "$FRESH_SOURCE" 'v1'
acquire_runtime_lock "$FRESH_RUNTIME"
begin_staged_transaction "$FRESH_SOURCE" "$FRESH_RUNTIME"
FRESH_TXN="$CURRENT_TXN"
adk_promote_routine_asset_transaction "$FRESH_RUNTIME" "$FRESH_TXN"
adk_rollback_routine_asset_transaction "$FRESH_RUNTIME" "$FRESH_TXN"
adk_rollback_routine_asset_transaction "$FRESH_RUNTIME" "$FRESH_TXN"
[ ! -e "$FRESH_RUNTIME/routines" ] && [ ! -e "$FRESH_RUNTIME/routine-helpers" ] \
    || fail_test 'fresh transaction rollback did not restore absent surfaces'
release_runtime_lock

# Pre-state-machine interruption may leave the only copies under .old. Beginning
# a new transaction restores both sole copies before it creates a unique stage.
SOLE_RUNTIME="$TMP_ROOT/sole-old-release"
seed_live "$SOLE_RUNTIME" 'v0'
mv "$SOLE_RUNTIME/routines" "$SOLE_RUNTIME/routines.old"
mv "$SOLE_RUNTIME/routine-helpers" "$SOLE_RUNTIME/routine-helpers.old"
acquire_runtime_lock "$SOLE_RUNTIME"
SOLE_TXN="$(adk_begin_routine_asset_transaction "$SOLE_RUNTIME" "$CURRENT_LOCK")"
[ "$(<"$SOLE_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ "$(<"$SOLE_RUNTIME/routine-helpers/generation-marker")" = 'v0' ] \
    && [ ! -e "$SOLE_RUNTIME/routines.old" ] \
    && [ ! -e "$SOLE_RUNTIME/routine-helpers.old" ] \
    || fail_test 'sole .old copies were not restored before a new transaction'
adk_abort_routine_asset_transaction "$SOLE_RUNTIME" "$SOLE_TXN"
release_runtime_lock

# The shared lock rejects a second entrypoint, and an old transaction cannot
# promote a later transaction's unique stage.
LOCK_RUNTIME="$TMP_ROOT/lock-release"
LOCK_SOURCE="$TMP_ROOT/lock-source"
seed_source "$LOCK_SOURCE" 'v1'
acquire_runtime_lock "$LOCK_RUNTIME"
begin_staged_transaction "$LOCK_SOURCE" "$LOCK_RUNTIME"
LOCK_TXN="$CURRENT_TXN"
set +e
(
    ADK_ROUTINE_ASSET_LOCK_DIR=""
    ADK_ROUTINE_ASSET_LOCK_TOKEN=""
    adk_acquire_routine_asset_lock "$CURRENT_LOCK" 0
) >/dev/null 2>&1
SECOND_LOCK_STATUS=$?
set -e
[ "$SECOND_LOCK_STATUS" -ne 0 ] || fail_test 'second entrypoint acquired the shared lock'
adk_abort_routine_asset_transaction "$LOCK_RUNTIME" "$LOCK_TXN"
LATER_TXN="$(adk_begin_routine_asset_transaction "$LOCK_RUNTIME" "$CURRENT_LOCK")"
set +e
adk_promote_routine_asset_transaction "$LOCK_RUNTIME" "$LOCK_TXN" >/dev/null 2>&1
OLD_PROMOTE_STATUS=$?
set -e
[ "$OLD_PROMOTE_STATUS" -ne 0 ] \
    || fail_test 'closed transaction promoted a later unique payload'
adk_abort_routine_asset_transaction "$LOCK_RUNTIME" "$LATER_TXN"
release_runtime_lock

# The bounded lexical validator ignores registration marker text in comments,
# strings, and templates, rejects an empty bundled root, and accepts a real
# standalone program-level call even when decoys are present.
assert_invalid_routine_source() {
    local name="$1"
    local body="$2"
    local root="$TMP_ROOT/lexer-$name"

    mkdir -p "$root/routines"
    seed_required_helpers "$root/routine-helpers" "$name"
    printf '%s\n' "$body" > "$root/routines/mutant.js"
    if adk_validate_repo_routine_assets "$root" >/dev/null 2>&1; then
        fail_test "lexical validator accepted $name marker mutant"
    fi
}

assert_invalid_routine_source block-comment \
    '/* agentdesk.routines.register({}); */ module.exports = {};'
assert_invalid_routine_source string \
    'const marker = "agentdesk.routines.register({})"; module.exports = {};'
assert_invalid_routine_source template \
    'const marker = `agentdesk.routines.register({})`; module.exports = {};'
assert_invalid_routine_source nested \
    'function later() { agentdesk.routines.register({}); }'
assert_invalid_routine_source malformed-tail \
    'agentdesk.routines.register({}); function broken() {'
EMPTY_ROOT="$TMP_ROOT/lexer-empty"
mkdir -p "$EMPTY_ROOT/routines"
seed_required_helpers "$EMPTY_ROOT/routine-helpers" 'empty'
if adk_validate_repo_routine_assets "$EMPTY_ROOT" >/dev/null 2>&1; then
    fail_test 'empty bundled routines root passed validation'
fi
VALID_ROOT="$TMP_ROOT/lexer-valid"
seed_required_helpers "$VALID_ROOT/routine-helpers" 'valid'
mkdir -p "$VALID_ROOT/routines"
printf '%s\n' \
    '/* agentdesk.routines.register({}); */' \
    'agentdesk.routines.register({ name: "real", tick() { return { action: "complete" }; } });' \
    > "$VALID_ROOT/routines/valid.js"
adk_validate_repo_routine_assets "$VALID_ROOT"
printf '%s\n' 'const decoy = "agentdesk.routines.register({})";' \
    > "$VALID_ROOT/routines/second-invalid.js"
if adk_validate_repo_routine_assets "$VALID_ROOT" >/dev/null 2>&1; then
    fail_test 'validator did not require a real registration in every JavaScript file'
fi
rm "$VALID_ROOT/routines/second-invalid.js"

# Node/Python helpers must not leak back into the QuickJS entrypoint root, and
# source/root symlinks remain fail-closed before rsync can follow them.
mkdir -p "$VALID_ROOT/routines/monitoring"
printf '%s\n' 'module.exports = {};' \
    > "$VALID_ROOT/routines/monitoring/local_worktree_inventory.js"
if adk_validate_repo_routine_assets "$VALID_ROOT" >/dev/null 2>&1; then
    fail_test 'non-QuickJS helper copied into routines passed validation'
fi
rm "$VALID_ROOT/routines/monitoring/local_worktree_inventory.js"

LINK_ROOT="$TMP_ROOT/symlink-source"
seed_source "$LINK_ROOT" 'linked'
mv "$LINK_ROOT/routine-helpers/monitoring/weekly_churn_audit.py" \
    "$TMP_ROOT/linked-helper.py"
ln -s "$TMP_ROOT/linked-helper.py" \
    "$LINK_ROOT/routine-helpers/monitoring/weekly_churn_audit.py"
if adk_validate_repo_routine_assets "$LINK_ROOT" >/dev/null 2>&1; then
    fail_test 'routine helper descendant symlink passed validation'
fi
ln -s "$LINK_ROOT" "$TMP_ROOT/symlink-source-root"
if adk_validate_repo_routine_assets "$TMP_ROOT/symlink-source-root" >/dev/null 2>&1; then
    fail_test 'symlinked routine asset source root passed validation'
fi

# Missing required source fails before transaction start; rsync failure leaves
# live assets untouched and a staging transaction safely abortable.
MISSING_ROOT="$TMP_ROOT/missing-source"
seed_source "$MISSING_ROOT" 'bad'
rm "$MISSING_ROOT/routine-helpers/monitoring/weekly_churn_audit.py"
if adk_validate_repo_routine_assets "$MISSING_ROOT" >/dev/null 2>&1; then
    fail_test 'missing authoritative helper passed source preflight'
fi
RSYNC_SOURCE="$TMP_ROOT/rsync-source"
RSYNC_RUNTIME="$TMP_ROOT/rsync-release"
seed_source "$RSYNC_SOURCE" 'v1'
seed_live "$RSYNC_RUNTIME" 'v0'
acquire_runtime_lock "$RSYNC_RUNTIME"
RSYNC_TXN="$(adk_begin_routine_asset_transaction "$RSYNC_RUNTIME" "$CURRENT_LOCK")"
RSYNC_FAIL_SOURCE="$RSYNC_SOURCE/routine-helpers/"
rsync() {
    local arg
    for arg in "$@"; do
        [ "$arg" != "$RSYNC_FAIL_SOURCE" ] || return 73
    done
    command rsync "$@"
}
set +e
adk_stage_routine_helpers "$RSYNC_SOURCE" "$RSYNC_RUNTIME" "$RSYNC_TXN" >/dev/null
RSYNC_STATUS=$?
set -e
unset -f rsync
[ "$RSYNC_STATUS" -ne 0 ] \
    && [ "$(<"$RSYNC_RUNTIME/routine-helpers/generation-marker")" = 'v0' ] \
    && [ ! -e "$RSYNC_TXN/staged/routine-helpers" ] \
    || fail_test 'rsync failure was masked or changed live helpers'
adk_abort_routine_asset_transaction "$RSYNC_RUNTIME" "$RSYNC_TXN"
release_runtime_lock

# Peer transport uses a validated absolute root, quoted fake-ssh command, and
# rsync --protect-args so spaces and shell metacharacters stay one remote path.
PEER_ROOT="$TMP_ROOT/peer/ADK Root;[\$HOME]"
mkdir -p "$PEER_ROOT/routines" "$PEER_ROOT/routine-helpers"
PEER_SSH_ARGS=()
ssh() {
    PEER_SSH_ARGS=("$@")
    command bash -c "${4:?missing fake-ssh remote command}"
}
adk_guard_peer_routine_asset_paths 'operator@peer.local' "$PEER_ROOT" 7
[ "${#PEER_SSH_ARGS[@]}" -ge 4 ] \
    && [[ "${PEER_SSH_ARGS[*]}" == *'bash -lc '* ]] \
    || fail_test 'fake ssh did not receive the quoted peer guard command'
ln -s "$TMP_ROOT" "$PEER_ROOT/routines/operator-link"
if adk_guard_peer_routine_asset_paths 'operator@peer.local' "$PEER_ROOT" 7 \
  >/dev/null 2>&1; then
    fail_test 'quoted peer guard missed a symlink under a metacharacter path'
fi
rm "$PEER_ROOT/routines/operator-link"
unset -f ssh
if adk_guard_peer_routine_asset_paths '-oProxyCommand=evil' "$PEER_ROOT" 7 \
  >/dev/null 2>&1; then
    fail_test 'peer guard accepted an option-like SSH destination'
fi
PEER_RSYNC_ARGS=()
rsync() {
    PEER_RSYNC_ARGS=("$@")
    return 0
}
adk_rsync_peer_asset_surface "$VALID_ROOT/routines" 'operator@peer.local' \
    "$PEER_ROOT" 'routines' 7
unset -f rsync
printf '%s\n' "${PEER_RSYNC_ARGS[@]}" | grep -Fxq -- '--protect-args' \
    || fail_test 'peer rsync omitted --protect-args'
[ "${PEER_RSYNC_ARGS[${#PEER_RSYNC_ARGS[@]}-1]}" = \
    "operator@peer.local:$PEER_ROOT/routines/" ] \
    || fail_test 'peer rsync split or rewrote the remote metacharacter path'

# Legacy macOS rsync rejects --protect-args. Its fallback must transfer the
# same metacharacter path through a quoted tar-over-SSH command, never retry an
# unprotected rsync invocation.
LEGACY_ROOT="$TMP_ROOT/legacy peer/ADK Root;[\$HOME]"
mkdir -p "$(dirname "$LEGACY_ROOT")"
LEGACY_RSYNC_PROBES=0
rsync() {
    if [ "${1:-}" = '--protect-args' ] && [ "${2:-}" = '--version' ]; then
        LEGACY_RSYNC_PROBES=$((LEGACY_RSYNC_PROBES + 1))
        return 1
    fi
    return 91
}
ssh() {
    command bash -c "${4:?missing legacy fake-ssh remote command}"
}
adk_rsync_peer_asset_surface "$VALID_ROOT/routines" 'operator@peer.local' \
    "$LEGACY_ROOT" 'routines' 7
unset -f ssh rsync
[ "$LEGACY_RSYNC_PROBES" -eq 1 ] \
    && cmp "$VALID_ROOT/routines/valid.js" \
        "$LEGACY_ROOT/routines/valid.js" >/dev/null \
    || fail_test 'legacy rsync fallback lost quoted tar-stream assets'

# Peer pre-sync owns only a unique inbox. The receiving deploy must hold its
# shared lock to claim it; staging merges operator files while repository bytes
# remain authoritative, and live v0 stays untouched until explicit promotion.
INBOX_RUNTIME="$TMP_ROOT/peer incoming runtime"
INBOX_SOURCE="$TMP_ROOT/peer-incoming-source"
INBOX_REPO="$TMP_ROOT/peer-incoming-repo"
seed_live "$INBOX_RUNTIME" 'v0'
seed_source "$INBOX_SOURCE" 'v1'
seed_source "$INBOX_REPO" 'v2'
write_quickjs_routine "$INBOX_SOURCE/routines/operator-private.js" 'operator'
printf 'operator helper\n' > "$INBOX_SOURCE/routine-helpers/operator-private.py"
INBOX_TOKEN='test.123.safe'
ssh() {
    command bash -c "${4:?missing inbox fake-ssh remote command}"
}
INBOX_PATH="$(adk_prepare_peer_asset_incoming 'operator@peer.local' \
    "$INBOX_RUNTIME" "$INBOX_TOKEN" 7)"
rsync() {
    if [ "${1:-}" = '--protect-args' ] && [ "${2:-}" = '--version' ]; then
        return 1
    fi
    return 91
}
adk_rsync_peer_asset_surface "$INBOX_SOURCE/routines" 'operator@peer.local' \
    "$INBOX_PATH" 'routines' 7
adk_rsync_peer_asset_surface "$INBOX_SOURCE/routine-helpers" 'operator@peer.local' \
    "$INBOX_PATH" 'routine-helpers' 7
unset -f rsync ssh
[ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ "$(<"$INBOX_RUNTIME/routine-helpers/generation-marker")" = 'v0' ] \
    || fail_test 'peer inbox transfer mutated live surfaces before remote lock'
if adk_claim_routine_asset_incoming "$INBOX_RUNTIME" "$INBOX_PATH" \
    "$INBOX_RUNTIME/runtime/deploy-release.lock" >/dev/null 2>&1; then
    fail_test 'peer inbox claim succeeded without owning the shared lock'
fi
acquire_runtime_lock "$INBOX_RUNTIME"
adk_claim_routine_asset_incoming "$INBOX_RUNTIME" "$INBOX_PATH" "$CURRENT_LOCK"
INBOX_TXN="$(adk_begin_routine_asset_transaction "$INBOX_RUNTIME" "$CURRENT_LOCK")"
adk_stage_routines "$INBOX_REPO" "$INBOX_RUNTIME" "$INBOX_TXN" \
    "$INBOX_PATH/routines" >/dev/null
adk_stage_routine_helpers "$INBOX_REPO" "$INBOX_RUNTIME" "$INBOX_TXN" \
    "$INBOX_PATH/routine-helpers" >/dev/null
[ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ "$(<"$INBOX_TXN/staged/routines/generation-marker")" = 'v2' ] \
    && [ "$(<"$INBOX_TXN/staged/routine-helpers/generation-marker")" = 'v2' ] \
    && [ -f "$INBOX_TXN/staged/routines/operator-private.js" ] \
    && [ -f "$INBOX_TXN/staged/routine-helpers/operator-private.py" ] \
    || fail_test 'remote lock-owned inbox staging changed live or lost overlay precedence'
adk_abort_routine_asset_transaction "$INBOX_RUNTIME" "$INBOX_TXN"
adk_remove_claimed_routine_asset_incoming "$INBOX_RUNTIME" "$INBOX_PATH" "$CURRENT_LOCK"
release_runtime_lock
[ ! -e "$INBOX_PATH" ] \
    && [ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    || fail_test 'aborted peer inbox transaction changed live or leaked its inbox'

# A partial transfer is inert and removable: without both surfaces the remote
# preflight cannot consume it, while the old live pair remains exact.
PARTIAL_TOKEN='partial.456.safe'
ssh() {
    command bash -c "${4:?missing partial-inbox fake-ssh command}"
}
PARTIAL_PATH="$(adk_prepare_peer_asset_incoming 'operator@peer.local' \
    "$INBOX_RUNTIME" "$PARTIAL_TOKEN" 7)"
rsync() {
    if [ "${1:-}" = '--protect-args' ] && [ "${2:-}" = '--version' ]; then
        return 1
    fi
    return 91
}
adk_rsync_peer_asset_surface "$INBOX_SOURCE/routines" 'operator@peer.local' \
    "$PARTIAL_PATH" 'routines' 7
unset -f rsync
[ -d "$PARTIAL_PATH/routines" ] \
    && [ ! -e "$PARTIAL_PATH/routine-helpers" ] \
    && [ "$(<"$INBOX_RUNTIME/routines/generation-marker")" = 'v0' ] \
    || fail_test 'partial peer transfer escaped its isolated inbox'
adk_remove_peer_asset_incoming 'operator@peer.local' "$INBOX_RUNTIME" \
    "$PARTIAL_TOKEN" 7
unset -f ssh
[ ! -e "$PARTIAL_PATH" ] \
    || fail_test 'partial peer transfer inbox could not be safely removed'

# Exercise installer preflight and promotion functions. An incomplete old
# artifact must fail before a binary marker or live assets change.
extract_function() {
    local function_name="$1"
    local source_file="${2:-$REPO_ROOT/scripts/install.sh}"
    awk -v start="^${function_name}[(][)] [{]$" '
        printing && $0 ~ /^[A-Za-z_][A-Za-z0-9_]*[(][)] [{]$/ { exit }
        $0 ~ start { printing = 1 }
        printing { print }
    ' "$source_file"
}
eval "$(extract_function prepare_install_routine_asset_surfaces)"
eval "$(extract_function promote_install_routine_asset_surfaces)"
eval "$(extract_function finalize_install_routine_asset_surfaces)"
eval "$(extract_function _install_sha256_file)"
eval "$(extract_function prepare_install_binary_transaction)"
eval "$(extract_function promote_install_binary_transaction)"
eval "$(extract_function _install_binary_live_sha256)"
eval "$(extract_function _install_binary_is_promoted)"
eval "$(extract_function _restore_install_binary_transaction)"
eval "$(extract_function _install_cleanup)"
warn() { :; }

reset_install_transaction_state() {
    INSTALL_ROUTINE_ASSET_TXN=""
    INSTALL_ROUTINE_ASSET_RUNTIME=""
    INSTALL_BINARY_LIVE=""
    INSTALL_BINARY_STAGE=""
    INSTALL_BINARY_BACKUP=""
    INSTALL_BINARY_NEW_SHA256=""
    INSTALL_BINARY_OLD_SHA256=""
    INSTALL_BINARY_HAD_LIVE=0
    INSTALL_BINARY_SWAP_ARMED=0
    INSTALL_BINARY_PROMOTED=0
    INSTALL_COMMIT_INTENT=0
    INSTALL_ASSET_FINALIZED=0
}

assert_install_generation() {
    local runtime_root="$1"
    local generation="$2"
    local binary="$3"

    [ "$(<"$runtime_root/routines/generation-marker")" = "$generation" ] \
        && [ "$(<"$runtime_root/routine-helpers/generation-marker")" = "$generation" ] \
        && [ "$(<"$runtime_root/bin/agentdesk")" = "$binary" ] \
        && [ ! -e "$runtime_root/runtime/routine-assets.active" ]
}

make_install_runtime() {
    local runtime_root="$1"
    seed_live "$runtime_root" 'v0'
    mkdir -p "$runtime_root/bin"
    printf 'old-binary\n' > "$runtime_root/bin/agentdesk"
    chmod +x "$runtime_root/bin/agentdesk"
}

BAD_ARTIFACT="$TMP_ROOT/old-artifact"
INSTALL_RUNTIME="$TMP_ROOT/install-release"
mkdir -p "$BAD_ARTIFACT/scripts" "$BAD_ARTIFACT/routines" \
    "$INSTALL_RUNTIME/bin"
cp "$REPO_ROOT/scripts/routine-asset-surface.sh" "$BAD_ARTIFACT/scripts/"
cp "$REPO_ROOT/scripts/validate-quickjs-routines.py" "$BAD_ARTIFACT/scripts/"
write_quickjs_routine "$BAD_ARTIFACT/routines/only.js" 'old-artifact'
seed_live "$INSTALL_RUNTIME" 'v0'
printf 'original binary\n' > "$INSTALL_RUNTIME/bin/agentdesk"
set +e
prepare_install_routine_asset_surfaces "$BAD_ARTIFACT" "$INSTALL_RUNTIME"
BAD_PREPARE_STATUS=$?
set -e
[ "$BAD_PREPARE_STATUS" -ne 0 ] \
    && [ "$(<"$INSTALL_RUNTIME/bin/agentdesk")" = 'original binary' ] \
    && [ "$(<"$INSTALL_RUNTIME/routines/generation-marker")" = 'v0' ] \
    && [ ! -e "$INSTALL_RUNTIME/runtime/routine-assets.active" ] \
    || fail_test 'old artifact preflight mutated binary or assets'

GOOD_ARTIFACT="$TMP_ROOT/good-artifact"
seed_source "$GOOD_ARTIFACT" 'v1'
mkdir -p "$GOOD_ARTIFACT/scripts"
cp "$REPO_ROOT/scripts/routine-asset-surface.sh" "$GOOD_ARTIFACT/scripts/"
cp "$REPO_ROOT/scripts/validate-quickjs-routines.py" "$GOOD_ARTIFACT/scripts/"
printf 'new-binary\n' > "$GOOD_ARTIFACT/agentdesk"
chmod +x "$GOOD_ARTIFACT/agentdesk"

# A binary copy failure occurs while both live surfaces still carry v0.
COPY_FAIL_RUNTIME="$TMP_ROOT/install-copy-fail"
make_install_runtime "$COPY_FAIL_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$COPY_FAIL_RUNTIME"
    cp() {
        [ "${1:-}" != "$GOOD_ARTIFACT/agentdesk" ] || return 71
        command cp "$@"
    }
    set +e
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$COPY_FAIL_RUNTIME"
    copy_status=$?
    unset -f cp
    _install_cleanup "$copy_status"
    cleanup_status=$?
    set -e
    [ "$copy_status" -ne 0 ] && [ "$cleanup_status" -ne 0 ]
) || fail_test 'installer masked staged binary copy failure'
assert_install_generation "$COPY_FAIL_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'binary copy failure produced an old-binary/new-assets pair'

# Failure after asset promotion but before the binary rename rolls assets back.
PRE_RENAME_RUNTIME="$TMP_ROOT/install-pre-rename"
make_install_runtime "$PRE_RENAME_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$PRE_RENAME_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$PRE_RENAME_RUNTIME"
    promote_install_routine_asset_surfaces
    set +e
    _install_cleanup 72
    cleanup_status=$?
    set -e
    [ "$cleanup_status" -eq 72 ]
) || fail_test 'installer cleanup failed at the assets-to-binary boundary'
assert_install_generation "$PRE_RENAME_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'pre-rename failure left promoted assets beside the old binary'

# Model TERM in the success-to-assignment gap: mv atomically replaces live,
# then reports 143 before INSTALL_BINARY_PROMOTED can be assigned.
TERM_GAP_RUNTIME="$TMP_ROOT/install-term-gap"
make_install_runtime "$TERM_GAP_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$TERM_GAP_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$TERM_GAP_RUNTIME"
    promote_install_routine_asset_surfaces
    promoted_stage="$INSTALL_BINARY_STAGE"
    promoted_live="$INSTALL_BINARY_LIVE"
    mv() {
        if [ "${1:-}" = '-f' ] \
          && [ "${2:-}" = "$promoted_stage" ] \
          && [ "${3:-}" = "$promoted_live" ]; then
            command mv "$@"
            return 143
        fi
        command mv "$@"
    }
    set +e
    promote_install_binary_transaction
    promote_status=$?
    unset -f mv
    _install_cleanup 143
    cleanup_status=$?
    set -e
    [ "$promote_status" -ne 0 ] && [ "$cleanup_status" -eq 143 ]
) || fail_test 'installer did not preserve TERM status across paired recovery'
assert_install_generation "$TERM_GAP_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'TERM after binary rename escaped paired rollback'

# Ordinary post-rename failure and cleanup-file deletion failure both leave a
# completely old pair. Cleanup failure must not undo the paired recovery.
CLEANUP_FAIL_RUNTIME="$TMP_ROOT/install-cleanup-fail"
make_install_runtime "$CLEANUP_FAIL_RUNTIME"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$CLEANUP_FAIL_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$CLEANUP_FAIL_RUNTIME"
    promote_install_routine_asset_surfaces
    promote_install_binary_transaction
    rollback_copy="$INSTALL_BINARY_BACKUP"
    rm() {
        [ "${1:-}" != '-f' ] || [ "${2:-}" != "$rollback_copy" ] || return 74
        command rm "$@"
    }
    set +e
    _install_cleanup 73
    cleanup_status=$?
    set -e
    unset -f rm
    [ "$cleanup_status" -ne 0 ]
) || fail_test 'installer masked cleanup-file deletion failure'
assert_install_generation "$CLEANUP_FAIL_RUNTIME" 'v0' 'old-binary' \
    || fail_test 'cleanup failure broke the recovered binary/assets pair'

# Healthy commit removes rollback generations only after durable commit intent.
INSTALL_RUNTIME="$TMP_ROOT/install-success"
make_install_runtime "$INSTALL_RUNTIME"
printf 'operator routine\n' > "$INSTALL_RUNTIME/routines/operator-private.txt"
printf 'operator helper\n' > "$INSTALL_RUNTIME/routine-helpers/operator-private.txt"
(
    reset_install_transaction_state
    prepare_install_routine_asset_surfaces "$GOOD_ARTIFACT" "$INSTALL_RUNTIME"
    prepare_install_binary_transaction "$GOOD_ARTIFACT/agentdesk" "$INSTALL_RUNTIME"
    promote_install_routine_asset_surfaces
    promote_install_binary_transaction
    finalize_install_routine_asset_surfaces
    _install_cleanup 0
)
[ "$(<"$INSTALL_RUNTIME/routines/generation-marker")" = 'v1' ] \
    && [ "$(<"$INSTALL_RUNTIME/bin/agentdesk")" = 'new-binary' ] \
    && [ ! -e "$INSTALL_RUNTIME/routines.old" ] \
    && [ -f "$INSTALL_RUNTIME/routines/operator-private.txt" ] \
    && [ -f "$INSTALL_RUNTIME/routine-helpers/operator-private.txt" ] \
    || fail_test 'installer did not atomically promote and finalize its paired payload'

# Exact stale-backup sequence: v0/M100 backup survives a healthy v1/M101
# cleanup failure, then v2 still embeds M101 and fails health. The rollback
# guard must read M100 from the digest-bound backup sidecar, never M101 from the
# now-newer live release manifest.
eval "$(extract_function _sha256_file "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _manifest_latest_migration_name "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _write_rollback_backup_metadata "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _rollback_backup_latest_migration_name "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _latest_postgres_migration_path "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _rollback_would_brick_on_migration "$REPO_ROOT/scripts/deploy-release.sh")"
eval "$(extract_function _migration_seq_from_name "$REPO_ROOT/scripts/_defaults.sh")"
eval "$(extract_function _migration_advanced "$REPO_ROOT/scripts/_defaults.sh")"

MIGRATION_REPO="$TMP_ROOT/migration-repo"
MIGRATION_RUNTIME="$TMP_ROOT/migration-runtime"
mkdir -p "$MIGRATION_REPO/migrations/postgres" "$MIGRATION_RUNTIME/bin" \
    "$MIGRATION_RUNTIME/runtime"
printf '%s\n' '-- M100' > "$MIGRATION_REPO/migrations/postgres/0100_m100.sql"
printf '%s\n' '-- M101' > "$MIGRATION_REPO/migrations/postgres/0101_m101.sql"
printf 'v0-binary\n' > "$MIGRATION_RUNTIME/bin/agentdesk.prev"
printf '%s\n' '{"latest_postgres_migration":"0100_m100.sql"}' \
    > "$MIGRATION_RUNTIME/runtime/release-source.json"
ADK_REL="$MIGRATION_RUNTIME"
REPO="$MIGRATION_REPO"
REL_BINARY_BACKUP="$MIGRATION_RUNTIME/bin/agentdesk.prev"
REL_BINARY_BACKUP_META="$REL_BINARY_BACKUP.meta"
_write_rollback_backup_metadata "$REL_BINARY_BACKUP" "$REL_BINARY_BACKUP_META"
[ "$(_rollback_backup_latest_migration_name)" = '0100_m100.sql' ] \
    || fail_test 'rollback sidecar was not bound to the v0 backup generation'

# v1 became healthy and wrote M101, but its simulated backup cleanup failed.
printf 'v1-binary\n' > "$MIGRATION_RUNTIME/bin/agentdesk"
printf '%s\n' '{"latest_postgres_migration":"0101_m101.sql"}' \
    > "$MIGRATION_RUNTIME/runtime/release-source.json"
set +e
_rollback_would_brick_on_migration >/dev/null 2>&1
STALE_BACKUP_GUARD_STATUS=$?
set -e
[ "$STALE_BACKUP_GUARD_STATUS" -eq 0 ] \
    || fail_test 'v2 rollback guard trusted the v1 live manifest for stale v0 backup'

# Force may skip migration ordering only after backup integrity succeeds. It
# must never turn a digest-mismatched or metadata-less file into executable
# rollback material.
printf 'tampered-v0-binary\n' > "$REL_BINARY_BACKUP"
AGENTDESK_DEPLOY_FORCE_ROLLBACK=1
set +e
_rollback_would_brick_on_migration >/dev/null 2>&1
TAMPERED_BACKUP_GUARD_STATUS=$?
set -e
unset AGENTDESK_DEPLOY_FORCE_ROLLBACK
[ "$TAMPERED_BACKUP_GUARD_STATUS" -eq 0 ] \
    || fail_test 'forced rollback bypassed backup digest verification'

# Gitignore keeps operator helpers private while the four bundled files remain
# explicitly trackable.
git -C "$REPO_ROOT" check-ignore --no-index -q -- routine-helpers/operator.py \
    || fail_test 'operator-private helper is not ignored'
for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
    if git -C "$REPO_ROOT" check-ignore --no-index -q -- "routine-helpers/$helper_ref"; then
        fail_test "authoritative helper is ignored: $helper_ref"
    fi
done

# Wiring ratchet ignores comment-only pseudo-calls and proves the shared lexer,
# transaction, lock, health commit, and artifact packaging cannot be removed.
assert_asset_wiring() {
    local root="$1"
    local prepare_calls

    grep -Fq '"$python_bin" "$ADK_QUICKJS_VALIDATOR" "$root"' \
        "$root/scripts/routine-asset-surface.sh" || return 1
    grep -Fq 'adk_acquire_routine_asset_lock "$DEPLOY_LOCK_FILE"' \
        "$root/scripts/deploy-release.sh" || return 1
    grep -Fq 'adk_promote_routine_asset_transaction "$ADK_REL"' \
        "$root/scripts/deploy-release.sh" || return 1
    grep -Fq 'adk_commit_routine_asset_transaction "$ADK_REL"' \
        "$root/scripts/deploy-release.sh" || return 1
    grep -Fq 'DEPLOY_HEALTH_OK=1' "$root/scripts/deploy.sh" || return 1
    grep -Fq 'adk_commit_routine_asset_transaction "$AD_HOME"' \
        "$root/scripts/deploy.sh" || return 1
    grep -Fq 'cp "scripts/validate-quickjs-routines.py"' \
        "$root/scripts/build-release.sh" || return 1
    prepare_calls="$(awk '
        /^[[:space:]]*prepare_install_routine_asset_surfaces / { count += 1 }
        END { print count + 0 }
    ' "$root/scripts/install.sh")"
    [ "$prepare_calls" -eq 2 ] || return 1
    if grep -Fq -- '--delete "$REPO/routines/"' "$root/scripts/deploy-release.sh"; then
        return 1
    fi
}

assert_asset_wiring "$REPO_ROOT" || fail_test 'production asset wiring is incomplete'
MUTATION_ROOT="$TMP_ROOT/wiring-mutation"
mkdir -p "$MUTATION_ROOT/scripts"
cp "$REPO_ROOT/scripts/"{build-release.sh,deploy-release.sh,deploy.sh,install.sh,routine-asset-surface.sh,validate-quickjs-routines.py} \
    "$MUTATION_ROOT/scripts/"
awk '
    /^[[:space:]]*prepare_install_routine_asset_surfaces / {
        print "  # " $0
        next
    }
    { print }
' "$MUTATION_ROOT/scripts/install.sh" > "$MUTATION_ROOT/scripts/install.sh.mutated"
mv "$MUTATION_ROOT/scripts/install.sh.mutated" "$MUTATION_ROOT/scripts/install.sh"
if assert_asset_wiring "$MUTATION_ROOT"; then
    fail_test 'wiring ratchet accepted comment-only installer calls'
fi

LEXER_MUTATION_ROOT="$TMP_ROOT/lexer-wiring-mutation"
mkdir -p "$LEXER_MUTATION_ROOT/scripts"
cp "$REPO_ROOT/scripts/"{build-release.sh,deploy-release.sh,deploy.sh,install.sh,routine-asset-surface.sh,validate-quickjs-routines.py} \
    "$LEXER_MUTATION_ROOT/scripts/"
awk 'index($0, "\"$python_bin\" \"$ADK_QUICKJS_VALIDATOR\" \"$root\"") == 0' \
    "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh" \
    > "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh.mutated"
mv "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh.mutated" \
    "$LEXER_MUTATION_ROOT/scripts/routine-asset-surface.sh"
if assert_asset_wiring "$LEXER_MUTATION_ROOT"; then
    fail_test 'wiring ratchet missed lexical validator removal'
fi

echo 'routine helper asset transaction tests passed'
