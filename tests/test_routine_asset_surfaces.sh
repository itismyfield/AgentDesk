#!/usr/bin/env bash
# Regression coverage for #4902 helper-asset deployment boundaries.
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
trap 'rm -rf "$TMP_ROOT"' EXIT

fail_test() {
    echo "FAIL: $*" >&2
    exit 1
}

seed_required_helpers() {
    local helper_root="$1"
    local label="$2"
    local helper_ref

    mkdir -p "$helper_root/monitoring"
    for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
        printf '%s-%s\n' "$label" "$helper_ref" > "$helper_root/$helper_ref"
    done
}

write_quickjs_routine() {
    local path="$1"
    local name="$2"

    mkdir -p "$(dirname "$path")"
    printf 'agentdesk.routines.register({ name: "%s", tick() { return { action: "complete" }; } });\n' \
        "$name" > "$path"
}

SOURCE_ROOT="$TMP_ROOT/repo"
RUNTIME_ROOT="$TMP_ROOT/release"
HELPER_DIR="monitoring"
seed_required_helpers "$SOURCE_ROOT/routine-helpers" 'repo'
seed_required_helpers "$RUNTIME_ROOT/routine-helpers" 'stale'
mkdir -p "$RUNTIME_ROOT/routines/$HELPER_DIR"
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    printf 'legacy-%s\n' "$helper_ref" > "$RUNTIME_ROOT/routines/$helper_ref"
done
printf 'operator-private helper\n' \
    > "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR/operator-private.py"
write_quickjs_routine \
    "$RUNTIME_ROOT/routines/$HELPER_DIR/operator-private.js" 'operator-private'
write_quickjs_routine \
    "$RUNTIME_ROOT/routines/$HELPER_DIR/daily-log-digest.js" 'old-daily-log-digest'
write_quickjs_routine \
    "$SOURCE_ROOT/routines/$HELPER_DIR/daily-log-digest.js" 'daily-log-digest'
chmod 0710 "$RUNTIME_ROOT/routine-helpers"

# Existing helper assets survive; tracked source wins only at matching paths.
# The staged root also inherits the live root's operator-selected mode.
HELPERS_STAGED="$(adk_stage_routine_helpers "$SOURCE_ROOT" "$RUNTIME_ROOT")"
[ "$(<"$HELPERS_STAGED/$HELPER_DIR/operator-private.py")" = 'operator-private helper' ] \
    || fail_test 'helper staging erased an operator-private asset'
[ "$(<"$HELPERS_STAGED/$HELPER_DIR/daily_log_digest.py")" = \
    'repo-monitoring/daily_log_digest.py' ] \
    || fail_test 'repository helper did not overlay its exact path'
[ "$(_adk_path_mode "$HELPERS_STAGED")" = \
    "$(_adk_path_mode "$RUNTIME_ROOT/routine-helpers")" ] \
    || fail_test 'helper staging did not preserve the live root mode'

# Exactly the four migrated paths disappear from routines; unrelated QuickJS
# assets survive, proving the compatibility tombstone is surgical.
ROUTINES_STAGED="$(adk_stage_routines "$SOURCE_ROOT" "$RUNTIME_ROOT")"
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    [ ! -e "$ROUTINES_STAGED/$helper_ref" ] \
        || fail_test "legacy helper survived routine tombstone: $helper_ref"
done
[ -f "$ROUTINES_STAGED/$HELPER_DIR/operator-private.js" ] \
    && [ -f "$ROUTINES_STAGED/$HELPER_DIR/daily-log-digest.js" ] \
    || fail_test 'routine tombstone removed a non-legacy entrypoint or operator asset'

# The first successful swap retains .old until an explicit health commit.
adk_swap_staged_routine_helpers "$RUNTIME_ROOT" "$HELPERS_STAGED"
[ -d "$RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'helper swap discarded rollback state before commit'
[ -f "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR/operator-private.py" ] \
    && [ -f "$RUNTIME_ROOT/routine-helpers/$HELPER_DIR/weekly_churn_audit.py" ] \
    || fail_test 'routine helper swap did not produce the expected live surface'
adk_commit_routine_helper_swap "$RUNTIME_ROOT"
[ ! -e "$RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'helper commit retained its transaction backup'

# The deploy-release EXIT path uses this rollback after a post-promotion health
# failure. It must restore the old surface and consume the transaction backup.
ROLLBACK_SOURCE_ROOT="$TMP_ROOT/rollback-repo"
ROLLBACK_RUNTIME_ROOT="$TMP_ROOT/rollback-release"
seed_required_helpers "$ROLLBACK_SOURCE_ROOT/routine-helpers" 'new'
seed_required_helpers "$ROLLBACK_RUNTIME_ROOT/routine-helpers" 'old'
printf 'old live\n' > "$ROLLBACK_RUNTIME_ROOT/routine-helpers/old-marker"
ROLLBACK_STAGED="$(
    adk_stage_routine_helpers "$ROLLBACK_SOURCE_ROOT" "$ROLLBACK_RUNTIME_ROOT"
)"
adk_swap_staged_routine_helpers "$ROLLBACK_RUNTIME_ROOT" "$ROLLBACK_STAGED"
adk_rollback_routine_helper_swap "$ROLLBACK_RUNTIME_ROOT"
[ -f "$ROLLBACK_RUNTIME_ROOT/routine-helpers/old-marker" ] \
    && [ ! -e "$ROLLBACK_RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'helper rollback did not restore the pre-swap surface'

# Missing one authoritative source helper fails before staging or live mutation.
MISSING_SOURCE_ROOT="$TMP_ROOT/missing-repo"
MISSING_RUNTIME_ROOT="$TMP_ROOT/missing-release"
seed_required_helpers "$MISSING_SOURCE_ROOT/routine-helpers" 'repo'
rm "$MISSING_SOURCE_ROOT/routine-helpers/monitoring/weekly_churn_audit.py"
seed_required_helpers "$MISSING_RUNTIME_ROOT/routine-helpers" 'live'
printf 'live marker\n' > "$MISSING_RUNTIME_ROOT/routine-helpers/live-marker"
set +e
adk_stage_routine_helpers "$MISSING_SOURCE_ROOT" "$MISSING_RUNTIME_ROOT" >/dev/null
MISSING_STATUS=$?
set -e
[ "$MISSING_STATUS" -ne 0 ] || fail_test 'missing required helper passed preflight'
[ -f "$MISSING_RUNTIME_ROOT/routine-helpers/live-marker" ] \
    && [ ! -e "$MISSING_RUNTIME_ROOT/routine-helpers.new" ] \
    || fail_test 'missing-source preflight mutated the live helper surface'

# A deterministic rsync shim fails only the repository helper overlay. Status
# must propagate; the trailing stage-path print must never mask it.
FAIL_SOURCE_ROOT="$TMP_ROOT/failing-repo"
FAIL_RUNTIME_ROOT="$TMP_ROOT/failing-release"
seed_required_helpers "$FAIL_SOURCE_ROOT/routine-helpers" 'new'
seed_required_helpers "$FAIL_RUNTIME_ROOT/routine-helpers" 'live'
printf 'live helper\n' > "$FAIL_RUNTIME_ROOT/routine-helpers/live-marker"
rsync() {
    local arg
    for arg in "$@"; do
        if [ "$arg" = "$FAIL_SOURCE_ROOT/routine-helpers/" ]; then
            return 73
        fi
    done
    command rsync "$@"
}
set +e
FAILED_STAGE="$(adk_stage_routine_helpers "$FAIL_SOURCE_ROOT" "$FAIL_RUNTIME_ROOT")"
FAILED_STATUS=$?
set -e
unset -f rsync
[ "$FAILED_STATUS" -ne 0 ] && [ -z "$FAILED_STAGE" ] \
    || fail_test 'helper overlay rsync failure was masked by stage output'
[ -f "$FAIL_RUNTIME_ROOT/routine-helpers/live-marker" ] \
    && [ ! -e "$FAIL_RUNTIME_ROOT/routine-helpers.new" ] \
    && [ ! -e "$FAIL_RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'failed helper staging mutated or swapped the live surface'

# A Node helper copied back into the QuickJS-only routines tree is rejected in
# both source and staged/default-root validation.
REENTRY_SOURCE_ROOT="$TMP_ROOT/reentry-repo"
REENTRY_RUNTIME_ROOT="$TMP_ROOT/reentry-release"
mkdir -p "$REENTRY_SOURCE_ROOT/routines/monitoring" \
    "$REENTRY_RUNTIME_ROOT/routines/monitoring"
cp "$REPO_ROOT/routine-helpers/monitoring/local_worktree_inventory.js" \
    "$REENTRY_SOURCE_ROOT/routines/monitoring/reentered-helper.js"
write_quickjs_routine \
    "$REENTRY_RUNTIME_ROOT/routines/monitoring/operator.js" 'operator'
set +e
adk_stage_routines "$REENTRY_SOURCE_ROOT" "$REENTRY_RUNTIME_ROOT" >/dev/null
REENTRY_STATUS=$?
set -e
[ "$REENTRY_STATUS" -ne 0 ] \
    && [ ! -e "$REENTRY_RUNTIME_ROOT/routines.new" ] \
    || fail_test 'non-registering Node helper re-entered the routines root'

# Source descendants, the source root itself, and the runtime root all fail
# closed when symlinked; rsync must never follow them into a transaction.
LINK_SOURCE_ROOT="$TMP_ROOT/link-repo"
LINK_RUNTIME_ROOT="$TMP_ROOT/link-release"
seed_required_helpers "$LINK_SOURCE_ROOT/routine-helpers" 'repo'
seed_required_helpers "$LINK_RUNTIME_ROOT/routine-helpers" 'live'
ln -s "$LINK_SOURCE_ROOT/routine-helpers/monitoring/daily_log_digest.py" \
    "$LINK_SOURCE_ROOT/routine-helpers/linked-helper"
set +e
adk_stage_routine_helpers "$LINK_SOURCE_ROOT" "$LINK_RUNTIME_ROOT" >/dev/null
LINK_STATUS=$?
set -e
[ "$LINK_STATUS" -ne 0 ] || fail_test 'helper descendant symlink passed validation'

LINK_ROOT_SOURCE="$TMP_ROOT/link-root-repo"
mkdir -p "$LINK_ROOT_SOURCE"
ln -s "$SOURCE_ROOT/routine-helpers" "$LINK_ROOT_SOURCE/routine-helpers"
set +e
adk_stage_routine_helpers "$LINK_ROOT_SOURCE" "$LINK_RUNTIME_ROOT" >/dev/null
LINK_ROOT_STATUS=$?
set -e
[ "$LINK_ROOT_STATUS" -ne 0 ] || fail_test 'symlinked helper root passed validation'

REAL_RUNTIME_ROOT="$TMP_ROOT/real-symlink-release"
SYMLINK_RUNTIME_ROOT="$TMP_ROOT/symlink-release"
mkdir -p "$REAL_RUNTIME_ROOT"
ln -s "$REAL_RUNTIME_ROOT" "$SYMLINK_RUNTIME_ROOT"
set +e
adk_stage_routine_helpers "$SOURCE_ROOT" "$SYMLINK_RUNTIME_ROOT" >/dev/null
RUNTIME_LINK_STATUS=$?
set -e
[ "$RUNTIME_LINK_STATUS" -ne 0 ] || fail_test 'symlinked runtime root passed validation'

# A staged->live rename failure immediately restores the original live tree and
# leaves no fake success state. Inject only that exact mv edge.
MV_SOURCE_ROOT="$TMP_ROOT/mv-repo"
MV_RUNTIME_ROOT="$TMP_ROOT/mv-release"
seed_required_helpers "$MV_SOURCE_ROOT/routine-helpers" 'new'
seed_required_helpers "$MV_RUNTIME_ROOT/routine-helpers" 'live'
printf 'original live\n' > "$MV_RUNTIME_ROOT/routine-helpers/live-marker"
MV_STAGED="$(adk_stage_routine_helpers "$MV_SOURCE_ROOT" "$MV_RUNTIME_ROOT")"
MV_FAIL_FROM="$MV_STAGED"
MV_FAIL_TO="$MV_RUNTIME_ROOT/routine-helpers"
mv() {
    if [ "$#" -eq 2 ] && [ "$1" = "$MV_FAIL_FROM" ] && [ "$2" = "$MV_FAIL_TO" ]; then
        return 74
    fi
    command mv "$@"
}
set +e
adk_swap_staged_routine_helpers "$MV_RUNTIME_ROOT" "$MV_STAGED"
MV_STATUS=$?
set -e
unset -f mv
[ "$MV_STATUS" -ne 0 ] \
    && [ -f "$MV_RUNTIME_ROOT/routine-helpers/live-marker" ] \
    && [ -d "$MV_STAGED" ] \
    && [ ! -e "$MV_RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'failed staged rename did not immediately restore live helpers'

# Retrying with a pre-existing .old must preserve that sole known-good backup.
# A failed retry restores the displaced current live; a successful retry keeps
# the original .old until commit.
RETRY_RUNTIME_ROOT="$TMP_ROOT/retry-release"
seed_required_helpers "$RETRY_RUNTIME_ROOT/routine-helpers" 'current'
seed_required_helpers "$RETRY_RUNTIME_ROOT/routine-helpers.old" 'known-good'
seed_required_helpers "$RETRY_RUNTIME_ROOT/routine-helpers.new" 'retry'
printf 'current live\n' > "$RETRY_RUNTIME_ROOT/routine-helpers/current-marker"
printf 'known good\n' > "$RETRY_RUNTIME_ROOT/routine-helpers.old/known-good-marker"
printf 'retry staged\n' > "$RETRY_RUNTIME_ROOT/routine-helpers.new/retry-marker"
RETRY_STAGED="$RETRY_RUNTIME_ROOT/routine-helpers.new"
MV_FAIL_FROM="$RETRY_STAGED"
MV_FAIL_TO="$RETRY_RUNTIME_ROOT/routine-helpers"
mv() {
    if [ "$#" -eq 2 ] && [ "$1" = "$MV_FAIL_FROM" ] && [ "$2" = "$MV_FAIL_TO" ]; then
        return 75
    fi
    command mv "$@"
}
set +e
adk_swap_staged_routine_helpers "$RETRY_RUNTIME_ROOT" "$RETRY_STAGED"
RETRY_STATUS=$?
set -e
unset -f mv
[ "$RETRY_STATUS" -ne 0 ] \
    && [ -f "$RETRY_RUNTIME_ROOT/routine-helpers/current-marker" ] \
    && [ -f "$RETRY_RUNTIME_ROOT/routine-helpers.old/known-good-marker" ] \
    && [ -d "$RETRY_STAGED" ] \
    || fail_test 'failed retry deleted .old or failed to restore current live'
adk_swap_staged_routine_helpers "$RETRY_RUNTIME_ROOT" "$RETRY_STAGED"
[ -f "$RETRY_RUNTIME_ROOT/routine-helpers/retry-marker" ] \
    && [ -f "$RETRY_RUNTIME_ROOT/routine-helpers.old/known-good-marker" ] \
    || fail_test 'successful retry replaced its original rollback backup'
adk_commit_routine_helper_swap "$RETRY_RUNTIME_ROOT"
[ ! -e "$RETRY_RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'successful retry commit retained .old'

# An interrupted prior transaction can leave only .old. Staging restores it to
# live first, then preserves its operator assets in the next staged overlay.
STALE_SOURCE_ROOT="$TMP_ROOT/stale-repo"
STALE_RUNTIME_ROOT="$TMP_ROOT/stale-release"
seed_required_helpers "$STALE_SOURCE_ROOT/routine-helpers" 'repo'
seed_required_helpers "$STALE_RUNTIME_ROOT/routine-helpers.old" 'old'
printf 'operator backup\n' \
    > "$STALE_RUNTIME_ROOT/routine-helpers.old/operator-marker"
STALE_STAGED="$(adk_stage_routine_helpers "$STALE_SOURCE_ROOT" "$STALE_RUNTIME_ROOT")"
[ -f "$STALE_RUNTIME_ROOT/routine-helpers/operator-marker" ] \
    && [ -f "$STALE_STAGED/operator-marker" ] \
    && [ ! -e "$STALE_RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'stale .old was not restored before staging'

# Exercise install.sh production wiring without its network/build bootstrap.
extract_function() {
    local function_name="$1"
    awk -v start="^${function_name}[(][)] [{]$" '
        $0 ~ start { printing = 1 }
        printing { print }
        printing && /^}$/ { exit }
    ' "$REPO_ROOT/scripts/install.sh"
}
eval "$(extract_function install_routine_asset_surfaces)"

INSTALL_SOURCE_ROOT="$TMP_ROOT/install-artifact"
INSTALL_RUNTIME_ROOT="$TMP_ROOT/install-release"
mkdir -p "$INSTALL_SOURCE_ROOT/scripts"
cp "$REPO_ROOT/scripts/routine-asset-surface.sh" \
    "$INSTALL_SOURCE_ROOT/scripts/routine-asset-surface.sh"
seed_required_helpers "$INSTALL_SOURCE_ROOT/routine-helpers" 'installed'
seed_required_helpers "$INSTALL_RUNTIME_ROOT/routine-helpers" 'old-live'
write_quickjs_routine \
    "$INSTALL_SOURCE_ROOT/routines/$HELPER_DIR/installed.js" 'installed'
write_quickjs_routine \
    "$INSTALL_RUNTIME_ROOT/routines/$HELPER_DIR/operator.js" 'operator'
printf 'operator helper\n' \
    > "$INSTALL_RUNTIME_ROOT/routine-helpers/$HELPER_DIR/operator.js"
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    printf 'old layout\n' > "$INSTALL_RUNTIME_ROOT/routines/$helper_ref"
done
install_routine_asset_surfaces "$INSTALL_SOURCE_ROOT" "$INSTALL_RUNTIME_ROOT"
[ -f "$INSTALL_RUNTIME_ROOT/routines/$HELPER_DIR/installed.js" ] \
    && [ -f "$INSTALL_RUNTIME_ROOT/routines/$HELPER_DIR/operator.js" ] \
    && [ -f "$INSTALL_RUNTIME_ROOT/routine-helpers/$HELPER_DIR/weekly_churn_audit.py" ] \
    && [ -f "$INSTALL_RUNTIME_ROOT/routine-helpers/$HELPER_DIR/operator.js" ] \
    || fail_test 'install wiring did not preserve and overlay both asset surfaces'
for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
    [ ! -e "$INSTALL_RUNTIME_ROOT/routines/$helper_ref" ] \
        || fail_test "install wiring retained old-layout helper: $helper_ref"
done
[ ! -e "$INSTALL_RUNTIME_ROOT/routines.old" ] \
    && [ ! -e "$INSTALL_RUNTIME_ROOT/routine-helpers.old" ] \
    || fail_test 'installer did not commit successful local asset swaps'

# Operator-private helpers stay ignored while the four shipped assets remain
# explicitly trackable.
git -C "$REPO_ROOT" check-ignore --no-index -q -- \
    routine-helpers/operator-private.js \
    || fail_test 'operator-private routine helper is not ignored'
for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
    if git -C "$REPO_ROOT" check-ignore --no-index -q -- \
        "routine-helpers/$helper_ref"; then
        fail_test "authoritative helper is ignored: $helper_ref"
    fi
done

# Ratchet release wiring itself, then prove the ratchet catches representative
# removal and broad-delete mutations rather than only passing production text.
assert_asset_wiring() {
    local root="$1"
    local script
    local health_line
    local commit_line

    for script in build-release.sh deploy-release.sh deploy.sh install.sh; do
        grep -Fq 'routine-helpers' "$root/scripts/$script" || return 1
    done
    grep -Fq 'adk_validate_repo_routine_assets "$PROJECT_DIR"' \
        "$root/scripts/build-release.sh" || return 1
    grep -Fq 'adk_validate_repo_routine_assets "$REPO"' \
        "$root/scripts/deploy-release.sh" || return 1
    grep -Fq 'ROUTINE_SWAP_ARMED=1' "$root/scripts/deploy-release.sh" || return 1
    grep -Fq 'adk_rollback_routine_helper_swap "$ADK_REL"' \
        "$root/scripts/deploy-release.sh" || return 1
    grep -Fq 'adk_commit_routine_swap "$ADK_REL"' \
        "$root/scripts/deploy-release.sh" || return 1
    grep -Fq 'remote routine asset symlink guard failed' \
        "$root/scripts/deploy-release.sh" || return 1
    [ "$(grep -Fc 'install_routine_asset_surfaces "$TMPDIR_' \
        "$root/scripts/install.sh")" -eq 2 ] || return 1
    grep -Fq 'cp "scripts/routine-asset-surface.sh"' \
        "$root/scripts/build-release.sh" || return 1
    health_line="$(grep -n '^DEPLOY_OK=1$' "$root/scripts/deploy-release.sh" \
        | head -1 | cut -d: -f1)"
    commit_line="$(grep -n 'adk_commit_routine_swap "$ADK_REL"' \
        "$root/scripts/deploy-release.sh" | head -1 | cut -d: -f1)"
    [ -n "$health_line" ] && [ -n "$commit_line" ] \
        && [ "$health_line" -lt "$commit_line" ] || return 1
    if grep -Fq -- '--delete "$REPO/routines/" "$ROUTINES_STAGED/"' \
        "$root/scripts/deploy-release.sh"; then
        return 1
    fi
}

assert_asset_wiring "$REPO_ROOT" || fail_test 'production routine asset wiring is incomplete'

REMOVAL_ROOT="$TMP_ROOT/wiring-removal"
mkdir -p "$REMOVAL_ROOT/scripts"
cp "$REPO_ROOT/scripts/"{build-release.sh,deploy-release.sh,deploy.sh,install.sh} \
    "$REMOVAL_ROOT/scripts/"
awk 'index($0, "install_routine_asset_surfaces \"$TMPDIR_DL/${ARTIFACT}\"") == 0' \
    "$REMOVAL_ROOT/scripts/install.sh" > "$REMOVAL_ROOT/scripts/install.sh.mutated"
mv "$REMOVAL_ROOT/scripts/install.sh.mutated" "$REMOVAL_ROOT/scripts/install.sh"
if assert_asset_wiring "$REMOVAL_ROOT"; then
    fail_test 'wiring ratchet missed an installer-path removal mutation'
fi

BROAD_DELETE_ROOT="$TMP_ROOT/wiring-broad-delete"
mkdir -p "$BROAD_DELETE_ROOT/scripts"
cp "$REPO_ROOT/scripts/"{build-release.sh,deploy-release.sh,deploy.sh,install.sh} \
    "$BROAD_DELETE_ROOT/scripts/"
printf '\nrsync -a --delete "$REPO/routines/" "$ROUTINES_STAGED/"\n' \
    >> "$BROAD_DELETE_ROOT/scripts/deploy-release.sh"
if assert_asset_wiring "$BROAD_DELETE_ROOT"; then
    fail_test 'wiring ratchet missed a broad routine-delete mutation'
fi

echo 'routine helper asset surface tests passed'
