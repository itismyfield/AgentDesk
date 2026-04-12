#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ADK_DEV="${AGENTDESK_DEV_ROOT:-$HOME/.adk/dev}"
PLIST="com.agentdesk.dev"
NOTICE_PATH="$ADK_DEV/RELEASE_ONLY_NOTICE.txt"

echo "═══ ADK Dev Runtime Cleanup Shim ═══"
echo "▸ This script no longer builds or starts dev."
echo "▸ Release-only flow: scripts/build-release.sh -> scripts/promote-release.sh"

remove_path() {
    local path="$1"
    if [ -L "$path" ] || [ -e "$path" ]; then
        rm -rf "$path"
        echo "  ▸ Removed $path"
    fi
}

write_release_only_notice() {
    mkdir -p "$ADK_DEV"
    cat > "$NOTICE_PATH" <<'EOF'
AgentDesk runtime policy: release only.

- Build, deploy, and restart operations must target `~/.adk/release`.
- Use `scripts/build-release.sh` for builds and `scripts/promote-release.sh` for runtime promotion.
- `scripts/deploy-dev.sh` is a cleanup shim only. It must not be used as a build or restart entrypoint.
- Do not start `dcserver` from `~/.adk/dev`.
- Remove stray dev runtime artifacts under `~/.adk/dev/bin`, `dashboard`, `logs`, `metrics`, `runtime`, and `skills` if they reappear.
- `~/.adk/dev/config` and `~/.adk/dev/data` are preserved unless an operator explicitly deletes them.
- If dev must ever be re-enabled, restore the backed up LaunchAgent plist and bot settings explicitly.
EOF
    echo "▸ Wrote release-only notice: $NOTICE_PATH"
}

disable_dev_launch_agent() {
    local plist_path="$HOME/Library/LaunchAgents/$PLIST.plist"
    local disabled_path="${plist_path}.disabled"

    echo "▸ Disabling dev launch agent..."
    launchctl bootout "gui/$(id -u)/$PLIST" 2>/dev/null || true
    launchctl disable "gui/$(id -u)/$PLIST" 2>/dev/null || true

    local remaining
    remaining="$(pgrep -f "$ADK_DEV/bin/agentdesk dcserver" 2>/dev/null || true)"
    if [ -n "$remaining" ]; then
        echo "  ▸ Killing lingering dev processes: $remaining"
        echo "$remaining" | xargs kill 2>/dev/null || true
        sleep 1
        remaining="$(pgrep -f "$ADK_DEV/bin/agentdesk dcserver" 2>/dev/null || true)"
        if [ -n "$remaining" ]; then
            echo "  ▸ Force killing lingering dev processes: $remaining"
            echo "$remaining" | xargs kill -9 2>/dev/null || true
            sleep 1
        fi
    fi

    rm -f "$ADK_DEV/runtime/dcserver.lock" "$ADK_DEV/runtime/dcserver.pid" "$ADK_DEV/runtime/dcserver.version"

    if [ -f "$plist_path" ]; then
        if [ -e "$disabled_path" ]; then
            disabled_path="${plist_path}.disabled-$(date '+%Y%m%d-%H%M%S')"
        fi
        mv -f "$plist_path" "$disabled_path"
        echo "  ▸ Moved dev plist to $disabled_path"
    else
        echo "  ▸ Dev plist already absent: $plist_path"
    fi

    if launchctl print-disabled "gui/$(id -u)" 2>/dev/null | grep -Fq "\"$PLIST\" => disabled"; then
        echo "  ▸ launchctl state: disabled"
    else
        echo "  ⚠ launchctl did not report $PLIST as disabled"
    fi
}

cleanup_dev_artifacts() {
    echo "▸ Removing dev runtime/build artifacts..."
    chflags nouchg "$ADK_DEV/bin/agentdesk" 2>/dev/null || true

    local cleanup_paths=(
        "$ADK_DEV/bin"
        "$ADK_DEV/dashboard"
        "$ADK_DEV/logs"
        "$ADK_DEV/metrics"
        "$ADK_DEV/restart_pending"
        "$ADK_DEV/runtime"
        "$ADK_DEV/skills"
    )

    local path
    for path in "${cleanup_paths[@]}"; do
        remove_path "$path"
    done
}

write_release_only_notice
disable_dev_launch_agent
cleanup_dev_artifacts

echo "▸ Ensuring global agentdesk CLI..."
"$SCRIPT_DIR/ensure-agentdesk-cli.sh"

echo "✓ Dev runtime stays disabled and cleaned"
echo "✓ Use scripts/build-release.sh + scripts/promote-release.sh for release operations"
echo "═══ Done ═══"
