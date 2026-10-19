#!/usr/bin/env bash
# Keeps the release policy directory in the same deploy generation as the binary.
# policies.prev pairs with bin/agentdesk.prev: kept, restored and dropped with it.
# Sourced by deploy-release.sh; bash 3.2 compatible.

POLICY_GENERATION_SWAPPED=0

# Before the binary backup is chosen: a policy backup is only trusted while a
# prior deploy's binary backup is still the rollback target.
_arm_policy_generation_backup() {
    local rel="$1" binary_backup="$2"
    if [ -f "$binary_backup" ]; then
        if [ ! -d "$rel/policies.prev" ] && [ -d "$rel/policies" ]; then
            echo "⚠ Binary backup from a prior deploy has no paired policy backup — current policies become the rollback policy generation"
        fi
    elif [ -e "$rel/policies.prev" ]; then
        echo "▸ Discarding stale policy backup (no paired binary backup)..."
        rm -rf "$rel/policies.prev"
    fi
}

# Installs the staged policies, keeping the replaced generation as policies.prev
# unless a prior deploy's paired backup is already the rollback target.
_swap_policy_generation() {
    local rel="$1" staged="$2"
    rm -rf "$rel/policies.old"
    if [ -d "$rel/policies.prev" ]; then
        echo "▸ Preserving existing policy backup paired with the last-known-good binary..."
        if [ -d "$rel/policies" ]; then
            mv "$rel/policies" "$rel/policies.old"
        fi
    elif [ -d "$rel/policies" ]; then
        mv "$rel/policies" "$rel/policies.prev"
    else
        echo "⚠ No current policies to back up — a rollback keeps the new policies"
    fi
    POLICY_GENERATION_SWAPPED=1
    mv "$staged" "$rel/policies"
    rm -rf "$rel/policies.old"
}

# Rollback only, with the release stopped. Best-effort: never blocks the binary
# restore or the restart, but reports every way the generations can diverge.
_restore_policy_generation() {
    local rel="$1"
    if [ ! -d "$rel/policies.prev" ]; then
        if [ "${POLICY_GENERATION_SWAPPED:-0}" = 1 ]; then
            echo "⚠ No previous policy generation to restore (none existed before this deploy) — the new policies stay with the previous binary"
        fi
        return 0
    fi
    rm -rf "$rel/policies.old" 2>/dev/null || true
    if [ -d "$rel/policies" ] && ! mv "$rel/policies" "$rel/policies.old"; then
        echo "✗ Failed to move the new policies aside — previous policies kept at $rel/policies.prev; manual intervention required"
        return 0
    fi
    if ! mv "$rel/policies.prev" "$rel/policies"; then
        if [ -d "$rel/policies.old" ]; then
            mv "$rel/policies.old" "$rel/policies" 2>/dev/null || true
        fi
        echo "✗ Failed to restore previous policies from $rel/policies.prev — manual intervention required"
        return 0
    fi
    rm -rf "$rel/policies.old" 2>/dev/null || true
    echo "↩ Previous policies restored with the previous binary"
}

# Success path, after the binary backup is dropped: a leftover policy backup
# without its binary backup is discarded by the next deploy's arm step.
_drop_policy_generation_backup() {
    rm -rf "$1/policies.prev" 2>/dev/null || true
}
