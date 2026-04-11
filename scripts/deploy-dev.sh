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
DEV_SKILLS_DIR="${AGENTDESK_DEV_SKILLS_DIR:-$ADK_DEV/skills}"
REPORT_CHANNEL_ID="${AGENTDESK_REPORT_CHANNEL_ID:-}"
REPORT_PROVIDER="${AGENTDESK_REPORT_PROVIDER:-}"
DEV_DEPLOY_DETACHED_CHILD="${AGENTDESK_DEPLOY_DEV_DETACHED_CHILD:-0}"
DEV_DEPLOY_LOG_PATH="${AGENTDESK_DEPLOY_DEV_LOG_PATH:-}"
DEV_DEPLOY_TEST_MODE="${AGENTDESK_DEPLOY_DEV_TEST_MODE:-0}"
DEV_DEPLOY_DELAY_SECS="${AGENTDESK_DEPLOY_DEV_DELAY_SECS:-2}"
DEV_HEALTH_RETRIES="${AGENTDESK_DEPLOY_DEV_HEALTH_RETRIES:-20}"
DEV_HEALTH_DELAY_SECS="${AGENTDESK_DEPLOY_DEV_HEALTH_DELAY_SECS:-2}"
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"
ALLOW_ADHOC_DEV_SIGN="${AGENTDESK_ALLOW_ADHOC_DEV_SIGN:-1}"

echo "═══ ADK Dev Deploy ═══"

sign_dev_binary_with_fallback() {
    local target="$1"
    local identity="${CODESIGN_IDENTITY:--}"

    if [ -z "$identity" ]; then
        identity="-"
    fi

    if [ "$identity" != "-" ] && command -v security >/dev/null 2>&1; then
        if ! security find-identity -v -p codesigning 2>/dev/null | grep -Fq "$identity"; then
            if [ "$ALLOW_ADHOC_DEV_SIGN" = "1" ]; then
                echo "⚠ Signing identity '$identity' not found; falling back to ad-hoc signature"
                identity="-"
            else
                echo "✗ Signing identity not found locally: $identity"
                echo "  Set AGENTDESK_ALLOW_ADHOC_DEV_SIGN=1 to permit ad-hoc signing for dev deploys"
                exit 1
            fi
        fi
    fi

    if [ "$identity" = "-" ]; then
        if [ "$ALLOW_ADHOC_DEV_SIGN" != "1" ]; then
            echo "✗ Refusing ad-hoc dev signing without AGENTDESK_ALLOW_ADHOC_DEV_SIGN=1"
            exit 1
        fi
        codesign -f -s "$identity" --identifier "com.itismyfield.agentdesk" "$target"
    else
        codesign -f -s "$identity" --options runtime --identifier "com.itismyfield.agentdesk" "$target"
    fi

    if ! codesign -v "$target" 2>/dev/null; then
        echo "✗ Codesign verification failed — aborting"
        rm -f "$target"
        exit 1
    fi
}

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
#   3. Release credential directory               (~/.adk/release/credential)
#   4. Hardcoded fallback                         (~/ObsidianVault/…/adk-config/credential)
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

    if [ -d "$release_credential" ]; then
        printf '%s\n' "$release_credential"
        return 0
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

    if [ -d "$dev_credential_dir" ] && [ ! -L "$dev_credential_dir" ]; then
        # Independent credential directory — don't overwrite with symlink.
        echo "▸ Dev credential is an independent directory — keeping as-is"
        return 0
    fi

    rm -f "$dev_credential_dir"
    ln -sfn "$shared_credential_dir" "$dev_credential_dir"
    echo "▸ Linked dev credential -> $shared_credential_dir"
}

_sync_dev_runtime_bot_settings() {
    local release_settings="$HOME/.adk/release/config/bot_settings.json"
    local dev_settings="$ADK_DEV/config/bot_settings.json"
    local dev_settings_migrated="$ADK_DEV/config/bot_settings.json.migrated"
    local dev_config="$ADK_DEV/config/agentdesk.yaml"

    mkdir -p "$(dirname "$dev_settings")"

    if [ -f "$release_settings" ]; then
        if [ -f "$dev_config" ] && command -v python3 >/dev/null 2>&1; then
            if python3 - "$release_settings" "$dev_config" "$dev_settings" <<'PY'
import json
import pathlib
import re
import sys

release_settings = pathlib.Path(sys.argv[1])
dev_config = pathlib.Path(sys.argv[2])
dev_settings = pathlib.Path(sys.argv[3])

allowed_agents = []
pattern = re.compile(r'^\s*-\s*id:\s*["\']?([^"\']+)')
for line in dev_config.read_text().splitlines():
    match = pattern.match(line)
    if match:
        allowed_agents.append(match.group(1).strip())

if not allowed_agents:
    raise SystemExit(1)

data = json.loads(release_settings.read_text())
filtered = {
    key: value
    for key, value in data.items()
    if isinstance(value, dict) and value.get("agent") in set(allowed_agents)
}

if not filtered:
    raise SystemExit(1)

dev_settings.write_text(json.dumps(filtered, ensure_ascii=False, indent=2) + "\n")
PY
            then
                echo "▸ Synced dev bot settings from release (filtered to dev agents)"
                return 0
            fi
            echo "⚠ Failed to filter release bot settings; falling back to direct copy"
        fi

        cp "$release_settings" "$dev_settings"
        echo "▸ Copied release bot settings to dev runtime"
        return 0
    fi

    if [ -f "$dev_settings_migrated" ] && [ ! -f "$dev_settings" ]; then
        cp "$dev_settings_migrated" "$dev_settings"
        echo "▸ Restored dev bot settings from migrated backup"
        return 0
    fi

    echo "▸ No runtime bot settings source found; leaving dev bot settings as-is"
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
export AGENTDESK_CODESIGN_IDENTITY=$(printf '%q' "$CODESIGN_IDENTITY")
export AGENTDESK_ALLOW_ADHOC_DEV_SIGN=$(printf '%q' "$ALLOW_ADHOC_DEV_SIGN")
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
sign_dev_binary_with_fallback "$ADK_DEV/bin/agentdesk.new"
mv -f "$ADK_DEV/bin/agentdesk.new" "$ADK_DEV/bin/agentdesk"
# Lock binary to prevent unsigned overwrites
chflags uchg "$ADK_DEV/bin/agentdesk"

# 3.5. Register with macOS firewall (NOPASSWD via /etc/sudoers.d/agentdesk-firewall)
FW=/usr/libexec/ApplicationFirewall/socketfilterfw
FIREWALL_SKIP_LOGGED=0
for firewall_arg in --add --unblockapp; do
    if ! sudo -n "$FW" "$firewall_arg" "$ADK_DEV/bin/agentdesk" >/dev/null 2>&1; then
        if [ "$FIREWALL_SKIP_LOGGED" = "0" ]; then
            echo "▸ Skipping firewall registration (sudo -n unavailable in non-interactive deploy)"
            FIREWALL_SKIP_LOGGED=1
        fi
    fi
done

# 3.6. Symlink dashboard dist
mkdir -p "$ADK_DEV/dashboard"
rm -rf "$ADK_DEV/dashboard/dist"
ln -sfn "$REPO/dashboard/dist" "$ADK_DEV/dashboard/dist"

# 3.7. Sync policies used by the dev runtime.
# Dev dcserver loads policies from its own workspace, not from the release worktree.
echo "▸ Syncing policies..."
mkdir -p "$DEV_POLICY_DIR"
rsync -a --delete "$REPO/policies/" "$DEV_POLICY_DIR/"

# 3.8. Sync managed skills used by the dev runtime.
echo "▸ Syncing managed skills..."
if [ ! -d "$REPO/skills" ]; then
    echo "✗ Managed skills not found: $REPO/skills"
    exit 1
fi
mkdir -p "$DEV_SKILLS_DIR"
rsync -a --delete "$REPO/skills/" "$DEV_SKILLS_DIR/"

# 3.9. Keep dev bot credentials aligned with the shared runtime credential.
echo "▸ Syncing credentials..."
_sync_dev_credentials

# 3.10. Keep dev runtime bot settings aligned with the release runtime.
echo "▸ Syncing bot settings..."
_sync_dev_runtime_bot_settings

# 3.11. Ensure the user-facing CLI wrapper is reachable via PATH.
echo "▸ Ensuring global agentdesk CLI..."
"$SCRIPT_DIR/ensure-agentdesk-cli.sh"

# 3.12. Re-apply optional local launchd env overrides before restart.
DEV_LAUNCHD_ENV_FILE="$ADK_DEV/config/launchd.env"
if [ -f "$DEV_LAUNCHD_ENV_FILE" ]; then
    echo "▸ Syncing dev launchd env..."
    sync_launchd_plist_environment_from_file "$HOME/Library/LaunchAgents/$PLIST.plist" "$DEV_LAUNCHD_ENV_FILE"
fi

# 4. Start dev
echo "▸ Starting dev..."
launchctl enable "gui/$(id -u)/$PLIST" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$PLIST.plist"

# 5. Health check
DEV_PORT="${AGENTDESK_DEV_PORT:-8799}"
echo "▸ Waiting for dev health on :${DEV_PORT}..."
DEV_HEALTHY=false
if wait_for_http_service_health "$PLIST" "$DEV_PORT" "$DEV_HEALTH_RETRIES" "$DEV_HEALTH_DELAY_SECS" 0 1; then
    DEV_HEALTHY=true
fi

if [ "$DEV_HEALTHY" = true ]; then
    if _health_json_reconcile_only "${WAIT_FOR_HTTP_SERVICE_LAST_HEALTH_JSON:-}"; then
        echo "✓ Dev is serving on :${DEV_PORT} (provider reconcile in progress)"
    else
        echo "✓ Dev is healthy on :${DEV_PORT}"
    fi
else
    echo "✗ Health check failed after $DEV_HEALTH_RETRIES attempts — check logs: $ADK_DEV/logs/"
    exit 1
fi

echo "═══ Done ═══"
