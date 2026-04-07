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
DEV_WORKSPACE_DIR="${AGENTDESK_DEV_WORKSPACE_DIR:-$ADK_DEV/workspaces/agentdesk}"
DEV_POLICY_DIR="${AGENTDESK_DEV_POLICY_DIR:-$DEV_WORKSPACE_DIR/policies}"
REPORT_CHANNEL_ID="${AGENTDESK_REPORT_CHANNEL_ID:-}"
REPORT_PROVIDER="${AGENTDESK_REPORT_PROVIDER:-}"
DEV_DEPLOY_DETACHED_CHILD="${AGENTDESK_DEPLOY_DEV_DETACHED_CHILD:-0}"
DEV_DEPLOY_LOG_PATH="${AGENTDESK_DEPLOY_DEV_LOG_PATH:-}"
DEV_DEPLOY_TEST_MODE="${AGENTDESK_DEPLOY_DEV_TEST_MODE:-0}"
DEV_DEPLOY_DELAY_SECS="${AGENTDESK_DEPLOY_DEV_DELAY_SECS:-2}"
DEV_HEALTH_RETRIES="${AGENTDESK_DEPLOY_DEV_HEALTH_RETRIES:-20}"
DEV_HEALTH_DELAY_SECS="${AGENTDESK_DEPLOY_DEV_HEALTH_DELAY_SECS:-2}"

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

# ── Credential Sync ──────────────────────────────────────────────────
# Dev and release runtimes share a single credential directory so that
# bot tokens, OAuth secrets, etc. stay in sync across environments.
#
# Source-of-truth resolution chain (_resolve_shared_credential_dir):
#   1. $AGENTDESK_SHARED_CREDENTIAL_DIR env var  (explicit override)
#   2. Release credential symlink target          (~/.adk/release/credential -> …)
#   3. Hardcoded fallback                         (~/ObsidianVault/…/adk-config/credential)
#
# _sync_dev_credentials creates (or updates) a symlink at
# ~/.adk/dev/credential -> <shared dir>, so every dev deploy
# guarantees the dev bot uses the same credentials as release.
#
# Why here: the sync is idempotent and fast, consistent with the
# existing policy sync (step 3.7) and dashboard symlink (step 3.6).
#
# Release credential is manually maintained (symlink created once by
# the operator). deploy.sh does NOT auto-sync credentials.
# ─────────────────────────────────────────────────────────────────────
_resolve_shared_credential_dir() {
    local configured="${AGENTDESK_SHARED_CREDENTIAL_DIR:-}"
    if [ -n "$configured" ] && [ -d "$configured" ]; then
        printf '%s\n' "$configured"
        return 0
    fi

    local release_credential="$HOME/.adk/release/credential"
    if [ -L "$release_credential" ]; then
        local target
        target=$(readlink "$release_credential" 2>/dev/null || true)
        if [ -n "$target" ]; then
            # Resolve relative symlinks against the symlink's parent directory
            if [[ "$target" != /* ]]; then
                target="$(cd "$(dirname "$release_credential")" && cd "$(dirname "$target")" && pwd)/$(basename "$target")"
            fi
            if [ -d "$target" ]; then
                printf '%s\n' "$target"
                return 0
            fi
        fi
    fi

    local fallback="$HOME/ObsidianVault/RemoteVault/adk-config/credential"
    if [ -d "$fallback" ]; then
        printf '%s\n' "$fallback"
        return 0
    fi

    return 1
}

_sync_dev_credentials() {
    local shared_credential_dir
    shared_credential_dir=$(_resolve_shared_credential_dir) || {
        echo "▸ Shared credential dir not found; leaving dev credential as-is"
        return 0
    }

    local dev_credential_dir="$ADK_DEV/credential"
    if [ -L "$dev_credential_dir" ] && [ "$(readlink "$dev_credential_dir" 2>/dev/null || true)" = "$shared_credential_dir" ]; then
        echo "▸ Dev credential already linked to shared credential"
        return 0
    fi

    if [ -e "$dev_credential_dir" ] && [ ! -L "$dev_credential_dir" ]; then
        local backup_dir
        backup_dir="${dev_credential_dir}.bak.$(date '+%Y%m%d-%H%M%S')"
        mv "$dev_credential_dir" "$backup_dir"
        echo "▸ Backed up stale dev credential dir to $backup_dir"
    else
        rm -f "$dev_credential_dir"
    fi

    ln -sfn "$shared_credential_dir" "$dev_credential_dir"
    echo "▸ Linked dev credential -> $shared_credential_dir"
}

_finalize_detached_helper() {
    local status="${1:-0}"
    [ "$DEV_DEPLOY_DETACHED_CHILD" = "1" ] || return 0
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local content
    if [ "$status" -eq 0 ]; then
        content="✅ dev deploy complete"
    else
        content="❌ dev deploy failed (exit ${status})
log: ${DEV_DEPLOY_LOG_PATH:-n/a}"
        local summary
        summary=$(_tail_for_summary "$DEV_DEPLOY_LOG_PATH")
        if [ -n "$summary" ]; then
            content="${content}
${summary}"
        fi
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
export AGENTDESK_DEPLOY_DEV_TEST_MODE=$(printf '%q' "$DEV_DEPLOY_TEST_MODE")
${AGENTDESK_SHARED_CREDENTIAL_DIR:+export AGENTDESK_SHARED_CREDENTIAL_DIR=$(printf '%q' "$AGENTDESK_SHARED_CREDENTIAL_DIR")}
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
# Atomic binary swap: sign in tmp, then mv to replace inode.
# Prevents OS signing cache corruption on codesign failure.
chflags nouchg "$ADK_DEV/bin/agentdesk" 2>/dev/null || true
cp "$REPO/target/release/agentdesk" "$ADK_DEV/bin/agentdesk.new"
chmod +x "$ADK_DEV/bin/agentdesk.new"
xattr -d com.apple.provenance "$ADK_DEV/bin/agentdesk.new" 2>/dev/null || true
codesign -s "Developer ID Application: Wonchang Oh (A7LJY7HNGA)" --options runtime --identifier "com.itismyfield.agentdesk" --force "$ADK_DEV/bin/agentdesk.new"
# Verify signature before swap
if ! codesign -v "$ADK_DEV/bin/agentdesk.new" 2>/dev/null; then
    echo "✗ Codesign verification failed — aborting"
    rm -f "$ADK_DEV/bin/agentdesk.new"
    exit 1
fi
mv -f "$ADK_DEV/bin/agentdesk.new" "$ADK_DEV/bin/agentdesk"
# Lock binary to prevent unsigned overwrites
chflags uchg "$ADK_DEV/bin/agentdesk"

# 3.5. Register with macOS firewall (NOPASSWD via /etc/sudoers.d/agentdesk-firewall)
FW=/usr/libexec/ApplicationFirewall/socketfilterfw
sudo "$FW" --add "$ADK_DEV/bin/agentdesk" 2>/dev/null || true
sudo "$FW" --unblockapp "$ADK_DEV/bin/agentdesk" 2>/dev/null || true

# 3.6. Symlink dashboard dist
mkdir -p "$ADK_DEV/dashboard"
rm -rf "$ADK_DEV/dashboard/dist"
ln -sfn "$REPO/dashboard/dist" "$ADK_DEV/dashboard/dist"

# 3.7. Sync policies used by the dev runtime.
# Dev dcserver loads policies from its own workspace, not from the release worktree.
echo "▸ Syncing policies..."
mkdir -p "$DEV_POLICY_DIR"
rsync -a --delete "$REPO/policies/" "$DEV_POLICY_DIR/"

# 3.8. Keep dev bot credentials aligned with the shared runtime credential.
echo "▸ Syncing credentials..."
_sync_dev_credentials

# 3.9. Ensure the user-facing CLI wrapper is reachable via PATH.
echo "▸ Ensuring global agentdesk CLI..."
"$SCRIPT_DIR/ensure-agentdesk-cli.sh"

# 4. Start dev
echo "▸ Starting dev..."
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$PLIST.plist"

# 5. Health check
DEV_PORT="${AGENTDESK_DEV_PORT:-8799}"
echo "▸ Waiting for dev health on :${DEV_PORT}..."
DEV_HEALTHY=false

for i in $(seq 1 "$DEV_HEALTH_RETRIES"); do
    HEALTH_JSON=$(curl -s --max-time 5 "http://${ADK_DEFAULT_LOOPBACK}:${DEV_PORT}/api/health" 2>/dev/null || true)
    if echo "$HEALTH_JSON" | grep -q '"status":"healthy"'; then
        DEV_HEALTHY=true
        break
    fi

    echo "  ▸ Attempt $i/$DEV_HEALTH_RETRIES — not healthy yet"
    if [ "$i" -lt "$DEV_HEALTH_RETRIES" ]; then
        sleep "$DEV_HEALTH_DELAY_SECS"
    fi
done

if [ "$DEV_HEALTHY" = true ]; then
    echo "✓ Dev is healthy on :${DEV_PORT}"
else
    echo "✗ Health check failed after $DEV_HEALTH_RETRIES attempts — check logs: $ADK_DEV/logs/"
    exit 1
fi

echo "═══ Done ═══"
