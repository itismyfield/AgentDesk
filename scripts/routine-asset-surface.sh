#!/usr/bin/env bash
# Shared staging primitives for routine-adjacent helper assets.
#
# `routines/` is a QuickJS-only loader root. These helpers deliberately live
# outside it so Node/Python modules cannot be discovered as executable routine
# entries. Keep the legacy list narrow: deploy must preserve operator-owned
# files and remove only the four paths that were previously bundled helpers.

ADK_REQUIRED_ROUTINE_HELPER_REFS=(
    "monitoring/local_worktree_inventory.js"
    "monitoring/daily_log_digest.py"
    "monitoring/log_digest_issue_drafts.py"
    "monitoring/weekly_churn_audit.py"
)
ADK_LEGACY_ROUTINE_HELPER_REFS=("${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}")

_adk_path_mode() {
    local path="$1"
    local mode

    if mode="$(stat -f '%Lp' "$path" 2>/dev/null)"; then
        printf '%s\n' "$mode"
        return 0
    fi
    stat -c '%a' "$path" 2>/dev/null
}

_adk_assert_no_symlink_tree() {
    local root="$1"
    local parent
    local first_link

    parent="$(dirname "$root")" || return 1
    if [ -L "$parent" ] || [ -L "$root" ]; then
        echo "Refusing symlinked routine asset ancestor: $root" >&2
        return 1
    fi
    [ -d "$root" ] || {
        echo "Required routine asset directory missing: $root" >&2
        return 1
    }
    if ! first_link="$(find "$root" -type l -print -quit 2>/dev/null)"; then
        echo "Could not inspect routine asset symlinks: $root" >&2
        return 1
    fi
    if [ -n "$first_link" ]; then
        echo "Refusing symlink in routine asset surface: $first_link" >&2
        return 1
    fi
}

adk_validate_routine_helper_surface() {
    local root="$1"
    local helper_ref

    _adk_assert_no_symlink_tree "$root" || return 1
    for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
        if [ ! -f "$root/$helper_ref" ] || [ -L "$root/$helper_ref" ]; then
            echo "Required regular routine helper missing: $root/$helper_ref" >&2
            return 1
        fi
    done
}

adk_validate_quickjs_routine_tree() {
    local root="$1"
    local script

    _adk_assert_no_symlink_tree "$root" || return 1
    while IFS= read -r -d '' script; do
        if ! grep -Eq '^[[:space:]]*agentdesk[.]routines[.]register[[:space:]]*[(]' "$script"; then
            echo "Non-QuickJS-registration JavaScript found in routines root: $script" >&2
            return 1
        fi
    done < <(find "$root" -type f -name '*.js' -print0)
}

adk_validate_repo_routine_assets() {
    local repo_root="$1"

    adk_validate_quickjs_routine_tree "$repo_root/routines" \
        && adk_validate_routine_helper_surface "$repo_root/routine-helpers"
}

adk_remove_legacy_routine_helpers() {
    local routines_root="$1"
    local helper_ref

    for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
        rm -f "$routines_root/$helper_ref" || return 1
    done
}

_adk_recover_stale_asset_backup() {
    local runtime_root="$1"
    local surface="$2"
    local live_root="$runtime_root/$surface"
    local backup_root="$runtime_root/$surface.old"
    local staged_root="$runtime_root/$surface.new"
    local retry_root="$runtime_root/$surface.swap-current"

    if [ -L "$runtime_root" ] || [ -L "$live_root" ] || [ -L "$backup_root" ] \
      || [ -L "$staged_root" ] || [ -L "$retry_root" ]; then
        echo "Refusing symlinked routine asset transaction path: $surface" >&2
        return 1
    fi
    if [ -e "$backup_root" ] && [ ! -d "$backup_root" ]; then
        echo "Routine asset backup is not a directory: $backup_root" >&2
        return 1
    fi
    if [ -d "$backup_root" ]; then
        _adk_assert_no_symlink_tree "$backup_root" || return 1
    fi
    if [ -e "$retry_root" ] && [ ! -d "$retry_root" ]; then
        echo "Routine asset retry path is not a directory: $retry_root" >&2
        return 1
    fi
    if [ -d "$retry_root" ]; then
        _adk_assert_no_symlink_tree "$retry_root" || return 1
    fi
    if [ ! -e "$live_root" ] && [ -d "$backup_root" ]; then
        mv "$backup_root" "$live_root" || return 1
    fi
}

_adk_stage_preserved_asset_surface() {
    local source_root="$1"
    local runtime_root="$2"
    local surface="$3"
    local live_root="$runtime_root/$surface"
    local staged_root="$runtime_root/$surface.new"
    local root_mode

    _adk_recover_stale_asset_backup "$runtime_root" "$surface" || return 1
    _adk_assert_no_symlink_tree "$source_root" || return 1
    if [ -e "$staged_root" ]; then
        _adk_assert_no_symlink_tree "$staged_root" || return 1
    fi
    if [ -e "$live_root" ]; then
        _adk_assert_no_symlink_tree "$live_root" || return 1
        root_mode="$(_adk_path_mode "$live_root")" || return 1
    else
        root_mode="$(_adk_path_mode "$source_root")" || return 1
    fi

    rm -rf "$staged_root" || return 1
    mkdir -p "$staged_root" || return 1
    chmod "$root_mode" "$staged_root" || return 1

    # Preserve operator-private assets first. Repository-owned files then win
    # by exact relative path; no --delete can erase unrelated local work.
    if [ -d "$live_root" ]; then
        if ! rsync -a "$live_root/" "$staged_root/"; then
            rm -rf "$staged_root" 2>/dev/null || true
            return 1
        fi
    fi
    if ! rsync -a "$source_root/" "$staged_root/"; then
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    fi
    chmod "$root_mode" "$staged_root" || {
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    }
}

adk_stage_routines() {
    local repo_root="$1"
    local runtime_root="$2"
    local staged_root="$runtime_root/routines.new"

    adk_validate_quickjs_routine_tree "$repo_root/routines" || return 1
    _adk_stage_preserved_asset_surface \
        "$repo_root/routines" "$runtime_root" "routines" \
        || return 1
    if ! adk_remove_legacy_routine_helpers "$staged_root"; then
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    fi
    if ! adk_validate_quickjs_routine_tree "$staged_root"; then
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    fi
    printf '%s\n' "$staged_root"
}

adk_stage_routine_helpers() {
    local repo_root="$1"
    local runtime_root="$2"
    local staged_root="$runtime_root/routine-helpers.new"

    adk_validate_routine_helper_surface "$repo_root/routine-helpers" || return 1
    _adk_stage_preserved_asset_surface \
        "$repo_root/routine-helpers" "$runtime_root" "routine-helpers" \
        || return 1
    if ! adk_validate_routine_helper_surface "$staged_root"; then
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    fi
    printf '%s\n' "$staged_root"
}

_adk_swap_staged_asset_surface() {
    local runtime_root="$1"
    local surface="$2"
    local staged_root="$3"
    local live_root="$runtime_root/$surface"
    local backup_root="$runtime_root/$surface.old"
    local retry_root="$runtime_root/$surface.swap-current"
    local had_backup=0

    if [ -L "$runtime_root" ] || [ -L "$staged_root" ] \
      || [ -L "$live_root" ] || [ -L "$backup_root" ] || [ -L "$retry_root" ]; then
        echo "Refusing symlinked routine asset transaction path: $surface" >&2
        return 1
    fi
    if [ "$staged_root" != "$runtime_root/$surface.new" ]; then
        echo "Refusing unexpected routine asset stage path: $staged_root" >&2
        return 1
    fi
    [ -d "$staged_root" ] || return 1
    _adk_recover_stale_asset_backup "$runtime_root" "$surface" || return 1
    if [ -d "$live_root" ]; then
        _adk_assert_no_symlink_tree "$live_root" || return 1
    fi

    rm -rf "$retry_root" || return 1
    if [ -d "$backup_root" ]; then
        had_backup=1
        # A previous uncommitted transaction owns .old. Never delete or replace
        # that sole known-good tree on retry; displace the current live tree.
        [ ! -d "$live_root" ] || mv "$live_root" "$retry_root" || return 1
    elif [ -d "$live_root" ]; then
        mv "$live_root" "$backup_root" || return 1
    fi

    if ! mv "$staged_root" "$live_root"; then
        if [ "$had_backup" = 1 ] && [ -d "$retry_root" ]; then
            # This was a retry of an uncommitted transaction. Restore the
            # displaced current tree and retain its original .old backup.
            mv "$retry_root" "$live_root" 2>/dev/null || true
        elif [ -d "$backup_root" ]; then
            if ! mv "$backup_root" "$live_root"; then
                [ ! -d "$retry_root" ] || mv "$retry_root" "$live_root" 2>/dev/null || true
            fi
        elif [ -d "$retry_root" ]; then
            mv "$retry_root" "$live_root" 2>/dev/null || true
        fi
        return 1
    fi
    rm -rf "$retry_root" 2>/dev/null || true
}

_adk_rollback_asset_swap() {
    local runtime_root="$1"
    local surface="$2"
    local live_root="$runtime_root/$surface"
    local backup_root="$runtime_root/$surface.old"
    local retry_root="$runtime_root/$surface.swap-current"

    if [ -L "$runtime_root" ] || [ -L "$live_root" ] \
      || [ -L "$backup_root" ] || [ -L "$retry_root" ]; then
        return 1
    fi
    if [ -d "$live_root" ]; then
        _adk_assert_no_symlink_tree "$live_root" || return 1
    fi
    if [ -d "$backup_root" ]; then
        _adk_assert_no_symlink_tree "$backup_root" || return 1
    fi
    if [ -d "$retry_root" ]; then
        _adk_assert_no_symlink_tree "$retry_root" || return 1
    fi
    rm -rf "$retry_root" || return 1
    if [ -d "$backup_root" ]; then
        [ ! -d "$live_root" ] || mv "$live_root" "$retry_root" || return 1
        if ! mv "$backup_root" "$live_root"; then
            [ ! -d "$retry_root" ] || mv "$retry_root" "$live_root" 2>/dev/null || true
            return 1
        fi
        rm -rf "$retry_root" 2>/dev/null || true
    else
        rm -rf "$live_root" || return 1
    fi
}

_adk_commit_asset_swap() {
    local runtime_root="$1"
    local surface="$2"
    local backup_root="$runtime_root/$surface.old"
    local retry_root="$runtime_root/$surface.swap-current"

    if [ -L "$runtime_root" ] || [ -L "$backup_root" ] || [ -L "$retry_root" ]; then
        return 1
    fi
    if [ -d "$backup_root" ]; then
        _adk_assert_no_symlink_tree "$backup_root" || return 1
    fi
    if [ -d "$retry_root" ]; then
        _adk_assert_no_symlink_tree "$retry_root" || return 1
    fi
    rm -rf "$backup_root" "$retry_root"
}

adk_swap_staged_routines() {
    adk_validate_quickjs_routine_tree "$2" \
        && _adk_swap_staged_asset_surface "$1" "routines" "$2"
}

adk_swap_staged_routine_helpers() {
    adk_validate_routine_helper_surface "$2" \
        && _adk_swap_staged_asset_surface "$1" "routine-helpers" "$2"
}

adk_rollback_routine_swap() {
    _adk_rollback_asset_swap "$1" "routines"
}

adk_rollback_routine_helper_swap() {
    _adk_rollback_asset_swap "$1" "routine-helpers"
}

adk_commit_routine_swap() {
    _adk_commit_asset_swap "$1" "routines"
}

adk_commit_routine_helper_swap() {
    _adk_commit_asset_swap "$1" "routine-helpers"
}
