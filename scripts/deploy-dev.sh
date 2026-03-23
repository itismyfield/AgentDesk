#!/usr/bin/env bash
set -euo pipefail

ADK_DEV="$HOME/.adk/dev"
PLIST="com.agentdesk.dev"
REPO="$HOME/AgentDesk"

echo "═══ ADK Dev Deploy ═══"

# 1. Build release
echo "▸ Building release..."
cd "$REPO"
cargo build --release 2>&1 | tail -1

# 2. Stop dev only — leave release untouched
echo "▸ Stopping dev..."
launchctl bootout "gui/$(id -u)/$PLIST" 2>/dev/null || true
sleep 1

# Kill only dev orphans (match dev binary path, not release)
REMAINING=$(pgrep -f "$ADK_DEV/bin/agentdesk dcserver" 2>/dev/null || true)
if [ -n "$REMAINING" ]; then
    echo "  ▸ Killing orphaned dev processes: $REMAINING"
    echo "$REMAINING" | xargs kill 2>/dev/null || true
    sleep 2
    STILL=$(pgrep -f "$ADK_DEV/bin/agentdesk dcserver" 2>/dev/null || true)
    if [ -n "$STILL" ]; then
        echo "  ▸ Force killing: $STILL"
        echo "$STILL" | xargs kill -9 2>/dev/null || true
        sleep 1
    fi
fi

# Remove stale lock file
rm -f "$ADK_DEV/runtime/dcserver.lock"

# 3. Copy binary
echo "▸ Copying binary..."
cp "$REPO/target/release/agentdesk" "$ADK_DEV/bin/agentdesk"
chmod +x "$ADK_DEV/bin/agentdesk"
codesign -s - --identifier "com.itismyfield.agentdesk" --force "$ADK_DEV/bin/agentdesk" 2>/dev/null || true

# 3.5. Register with macOS firewall (NOPASSWD via /etc/sudoers.d/agentdesk-firewall)
FW=/usr/libexec/ApplicationFirewall/socketfilterfw
sudo "$FW" --add "$ADK_DEV/bin/agentdesk" 2>/dev/null || true
sudo "$FW" --unblockapp "$ADK_DEV/bin/agentdesk" 2>/dev/null || true

# 3.6. Symlink dashboard dist
mkdir -p "$ADK_DEV/dashboard"
ln -sfn "$REPO/dashboard/dist" "$ADK_DEV/dashboard/dist"

# 4. Start dev
echo "▸ Starting dev..."
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$PLIST.plist"
sleep 3

# 5. Health check
DEV_PORT="${AGENTDESK_DEV_PORT:-8791}"
if curl -s --max-time 5 "http://127.0.0.1:${DEV_PORT}/api/health" | grep -q '"status":"healthy"'; then
    echo "✓ Dev is healthy on :${DEV_PORT}"
else
    echo "✗ Health check failed — check logs: $ADK_DEV/logs/"
    exit 1
fi

echo "═══ Done ═══"
