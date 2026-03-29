#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

ADK_DEV="$HOME/.adk/dev"
PLIST="com.agentdesk.dev"
REPO="${AGENTDESK_REPO_DIR:-}"
if [ -z "$REPO" ]; then
    REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
fi
if [ ! -d "$REPO" ]; then
    echo "✗ Repo not found: $REPO"
    exit 1
fi
REPO="$(cd "$REPO" && pwd)"
REPORT_CHANNEL_ID="${AGENTDESK_REPORT_CHANNEL_ID:-}"
REPORT_PROVIDER="${AGENTDESK_REPORT_PROVIDER:-}"
DEV_DEPLOY_DETACHED_CHILD="${AGENTDESK_DEPLOY_DEV_DETACHED_CHILD:-0}"
DEV_DEPLOY_LOG_PATH="${AGENTDESK_DEPLOY_DEV_LOG_PATH:-}"
DEV_DEPLOY_HELPER_SESSION="${AGENTDESK_DEPLOY_DEV_HELPER_SESSION:-}"
DEV_DEPLOY_TEST_MODE="${AGENTDESK_DEPLOY_DEV_TEST_MODE:-0}"
DEV_DEPLOY_DELAY_SECS="${AGENTDESK_DEPLOY_DEV_DELAY_SECS:-2}"

echo "═══ ADK Dev Deploy ═══"

_notify_channel() {
    local content="$1"
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local payload
    payload=$(printf '%s' "$content" | jq -Rs --arg source "project-agentdesk" --arg target "channel:$REPORT_CHANNEL_ID" '{target:$target, content: ., source:$source, bot:"notify"}')

    local dev_port="${AGENTDESK_DEV_PORT:-8799}"
    local rel_port="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
    curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${dev_port}/api/send" \
        -H 'Content-Type: application/json' \
        --data-binary "$payload" >/dev/null 2>&1 \
        || curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${rel_port}/api/send" \
            -H 'Content-Type: application/json' \
            --data-binary "$payload" >/dev/null 2>&1 \
        || true
}

_tail_for_summary() {
    local log_path="$1"
    [ -f "$log_path" ] || return 0
    tail -n 12 "$log_path" 2>/dev/null || true
}

_finalize_detached_helper() {
    local status="${1:-0}"
    [ "$DEV_DEPLOY_DETACHED_CHILD" = "1" ] || return 0
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local content
    if [ "$status" -eq 0 ]; then
        content="✅ dev deploy helper finished
session: ${DEV_DEPLOY_HELPER_SESSION:-unknown}
log: ${DEV_DEPLOY_LOG_PATH:-n/a}"
    else
        content="❌ dev deploy helper failed (exit ${status})
session: ${DEV_DEPLOY_HELPER_SESSION:-unknown}
log: ${DEV_DEPLOY_LOG_PATH:-n/a}"
    fi

    local summary
    summary=$(_tail_for_summary "$DEV_DEPLOY_LOG_PATH")
    if [ -n "$summary" ]; then
        content="${content}

최근 로그:
${summary}"
    fi

    _notify_channel "$content"
}

_cleanup_on_exit() {
    local status=$?
    _finalize_detached_helper "$status"
}

trap _cleanup_on_exit EXIT

_self_hosted_dev_session() {
    [ "$DEV_DEPLOY_DETACHED_CHILD" != "1" ] || return 1
    [ -n "${TMUX:-}" ] || return 1
    [ -n "$REPORT_CHANNEL_ID" ] || return 1
    [ -n "$REPORT_PROVIDER" ] || return 1
    return 0
}

_spawn_detached_helper() {
    local tasks_dir="$ADK_DEV/runtime/self_hosted_deploy"
    mkdir -p "$tasks_dir"

    local stamp
    stamp=$(date '+%Y%m%d-%H%M%S')
    local helper_session="ADK-devdeploy-${REPORT_CHANNEL_ID}-${stamp}"
    local log_path="$tasks_dir/deploy-dev-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.log"
    local helper_script="$tasks_dir/deploy-dev-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.sh"
    local quoted_args=""
    if [ "$#" -gt 0 ]; then
        quoted_args=$(printf ' %q' "$@")
    fi

    cat > "$helper_script" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec >>$(printf '%q' "$log_path") 2>&1
sleep $(printf '%q' "$DEV_DEPLOY_DELAY_SECS")
export AGENTDESK_REPORT_CHANNEL_ID=$(printf '%q' "$REPORT_CHANNEL_ID")
export AGENTDESK_REPORT_PROVIDER=$(printf '%q' "$REPORT_PROVIDER")
export AGENTDESK_REPO_DIR=$(printf '%q' "$REPO")
export AGENTDESK_DEPLOY_DEV_DETACHED_CHILD=1
export AGENTDESK_DEPLOY_DEV_LOG_PATH=$(printf '%q' "$log_path")
export AGENTDESK_DEPLOY_DEV_HELPER_SESSION=$(printf '%q' "$helper_session")
export AGENTDESK_DEPLOY_DEV_TEST_MODE=$(printf '%q' "$DEV_DEPLOY_TEST_MODE")
cd $(printf '%q' "$REPO")
exec $(printf '%q' "$SCRIPT_DIR/deploy-dev.sh")${quoted_args}
EOF
    chmod +x "$helper_script"
    tmux new-session -d -s "$helper_session" "$helper_script"

    echo "▸ Self-hosted dev deploy detected — using detached helper"
    echo "  helper tmux: $helper_session"
    echo "  helper log: $log_path"
    echo "  current turn will finish before dcserver restart; final result will be reported automatically"
}

# 1. Build release
echo "▸ Building release..."
cd "$REPO"
make build 2>&1 | tail -3

if _self_hosted_dev_session; then
    _spawn_detached_helper "$@"
    exit 0
fi

if [ "$DEV_DEPLOY_TEST_MODE" = "1" ]; then
    echo "▸ TEST MODE: skipping dev bootout/copy/bootstrap"
    echo "✓ Detached helper dry run complete"
    exit 0
fi

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
# Remove immutable flag if set (only deploy scripts should touch the binary)
chflags nouchg "$ADK_DEV/bin/agentdesk" 2>/dev/null || true
cp "$REPO/target/release/agentdesk" "$ADK_DEV/bin/agentdesk"
chmod +x "$ADK_DEV/bin/agentdesk"
codesign -s "Developer ID Application: Wonchang Oh (A7LJY7HNGA)" --options runtime --identifier "com.itismyfield.agentdesk" --force "$ADK_DEV/bin/agentdesk"
# Verify signature
if ! codesign -v "$ADK_DEV/bin/agentdesk" 2>/dev/null; then
    echo "✗ Codesign verification failed — aborting"
    exit 1
fi
# Lock binary to prevent unsigned overwrites
chflags uchg "$ADK_DEV/bin/agentdesk"

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
DEV_PORT="${AGENTDESK_DEV_PORT:-$ADK_DEFAULT_PORT}"
if curl -s --max-time 5 "http://${ADK_DEFAULT_LOOPBACK}:${DEV_PORT}/api/health" | grep -q '"status":"healthy"'; then
    echo "✓ Dev is healthy on :${DEV_PORT}"
else
    echo "✗ Health check failed — check logs: $ADK_DEV/logs/"
    exit 1
fi

echo "═══ Done ═══"
