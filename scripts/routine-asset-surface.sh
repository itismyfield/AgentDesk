#!/usr/bin/env bash
# Shared staging primitives for routine-adjacent helper assets.
#
# `routines/` is a QuickJS-only loader root. These helpers deliberately live
# outside it so Node/Python modules cannot be discovered as executable routine
# entries. Keep the legacy list narrow: deploy must preserve operator-owned
# files and remove only the four paths that were previously bundled helpers.

ADK_LEGACY_ROUTINE_HELPER_REFS=(
    "monitoring/local_worktree_inventory.js"
    "monitoring/daily_log_digest.py"
    "monitoring/log_digest_issue_drafts.py"
    "monitoring/weekly_churn_audit.py"
)

adk_remove_legacy_routine_helpers() {
    local routines_root="$1"
    local helper_ref

    for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
        rm -f "$routines_root/$helper_ref"
    done
}

adk_stage_routine_helpers() {
    local repo_root="$1"
    local runtime_root="$2"
    local staged_root="$runtime_root/routine-helpers.new"

    rm -rf "$staged_root"
    mkdir -p "$staged_root"

    # Preserve operator-private helpers first. Repository-owned helpers then
    # win by exact relative path; no --delete can erase unrelated local work.
    if [ -d "$runtime_root/routine-helpers" ]; then
        rsync -a "$runtime_root/routine-helpers/" "$staged_root/"
    fi
    rsync -a "$repo_root/routine-helpers/" "$staged_root/"
    printf '%s\n' "$staged_root"
}

adk_swap_staged_routine_helpers() {
    local runtime_root="$1"
    local staged_root="$2"

    rm -rf "$runtime_root/routine-helpers.old"
    [ -d "$runtime_root/routine-helpers" ] \
        && mv "$runtime_root/routine-helpers" "$runtime_root/routine-helpers.old"
    mv "$staged_root" "$runtime_root/routine-helpers"
    rm -rf "$runtime_root/routine-helpers.old"
}
