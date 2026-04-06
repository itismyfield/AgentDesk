#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

ADK_DEV="$HOME/.adk/dev"
ADK_REL="$HOME/.adk/release"
PLIST_REL="com.agentdesk.release"
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
PROMOTE_DETACHED_CHILD="${AGENTDESK_PROMOTE_DETACHED_CHILD:-0}"
PROMOTE_LOG_PATH="${AGENTDESK_PROMOTE_LOG_PATH:-}"
PROMOTE_TEST_MODE="${AGENTDESK_PROMOTE_TEST_MODE:-0}"
PROMOTE_DELAY_SECS="${AGENTDESK_PROMOTE_DELAY_SECS:-2}"
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"
ALLOW_ADHOC_RELEASE_SIGN="${AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN:-0}"
DASHBOARD_SOURCE=""

echo "═══ ADK Promote Dev → Release ═══"

sign_binary_with_fallback() {
    local target="$1"
    local identity="${CODESIGN_IDENTITY:--}"

    if [ -z "$identity" ]; then
        if [ "$ALLOW_ADHOC_RELEASE_SIGN" = "1" ]; then
            echo "⚠ No signing identity configured; using explicit ad-hoc release signature override"
            identity="-"
        else
            echo "✗ No release signing identity configured"
            echo "  Set AGENTDESK_CODESIGN_IDENTITY to a valid Developer ID Application certificate"
            echo "  or set AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=1 for an explicit local override"
            exit 1
        fi
    fi

    if [ "$identity" = "-" ] && [ "$ALLOW_ADHOC_RELEASE_SIGN" != "1" ]; then
        echo "✗ Refusing ad-hoc release signing without AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=1"
        exit 1
    fi

    if [ -n "$identity" ] && [ "$identity" != "-" ] && command -v security >/dev/null 2>&1; then
        if ! security find-identity -v -p codesigning 2>/dev/null | grep -Fq "$identity"; then
            if [ "$ALLOW_ADHOC_RELEASE_SIGN" = "1" ]; then
                echo "⚠ Signing identity not found locally; using explicit ad-hoc release signature override"
                identity="-"
            else
                echo "✗ Signing identity not found locally: $identity"
                echo "  Refusing release promotion without a valid Developer ID Application certificate"
                echo "  Set AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN=1 only for an explicit local override"
                exit 1
            fi
        fi
    fi

    if [ "$identity" = "-" ]; then
        codesign -f -s "$identity" --identifier "com.itismyfield.agentdesk" "$target"
    else
        codesign -f -s "$identity" --options runtime --identifier "com.itismyfield.agentdesk" "$target"
    fi

    if ! codesign -v "$target" 2>/dev/null; then
        echo "✗ Codesign verification failed — aborting"
        exit 1
    fi
}

_notify_channel() {
    local content="$1"
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local payload
    payload=$(printf '%s' "$content" | jq -Rs --arg source "project-agentdesk" --arg target "channel:$REPORT_CHANNEL_ID" '{target:$target, content: ., source:$source, bot:"notify"}')

    local rel_port="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
    local dev_port="${AGENTDESK_DEV_PORT:-8799}"
    curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${rel_port}/api/send" \
        -H 'Content-Type: application/json' \
        --data-binary "$payload" >/dev/null 2>&1 \
        || curl -sf -X POST "http://${ADK_DEFAULT_LOOPBACK}:${dev_port}/api/send" \
            -H 'Content-Type: application/json' \
            --data-binary "$payload" >/dev/null 2>&1 \
        || true
}

_tail_for_summary() {
    local log_path="$1"
    [ -f "$log_path" ] || return 0
    tail -n 12 "$log_path" 2>/dev/null || true
}

_resolve_dashboard_source() {
    # Dev dashboard may be a symlink (deploy-dashboard.sh dev uses ln -sfn).
    # Resolve to the real path so cp -r copies actual files, not dangling links.
    local candidate
    for candidate in "$ADK_DEV/dashboard/dist" "$REPO/dashboard/dist"; do
        if [ -d "$candidate" ]; then
            local resolved
            resolved="$(cd "$candidate" && pwd -P)"
            if [ -f "$resolved/index.html" ]; then
                printf '%s\n' "$resolved"
                return 0
            fi
        fi
    done
    return 1
}

_finalize_detached_helper() {
    local status="${1:-0}"
    [ "$PROMOTE_DETACHED_CHILD" = "1" ] || return 0
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local content
    if [ "$status" -eq 0 ]; then
        content="✅ release promote complete"
    else
        content="❌ release promote failed (exit ${status})
log: ${PROMOTE_LOG_PATH:-n/a}"
        local summary
        summary=$(_tail_for_summary "$PROMOTE_LOG_PATH")
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

_self_hosted_release_session() {
    [ "$PROMOTE_DETACHED_CHILD" != "1" ] || return 1
    [ -n "${TMUX:-}" ] || return 1
    [ -n "$REPORT_CHANNEL_ID" ] || return 1
    [ -n "$REPORT_PROVIDER" ] || return 1
    return 0
}

_spawn_detached_helper() {
    local tasks_dir="$ADK_REL/runtime/self_hosted_promote"
    mkdir -p "$tasks_dir"

    local stamp
    stamp=$(date '+%Y%m%d-%H%M%S')
    local helper_session="ADK-promote-${REPORT_CHANNEL_ID}-${stamp}"
    local log_path="$tasks_dir/promote-release-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.log"
    local helper_script="$tasks_dir/promote-release-${REPORT_PROVIDER}-${REPORT_CHANNEL_ID}-${stamp}.sh"
    local quoted_args=""
    if [ "$#" -gt 0 ]; then
        quoted_args=$(printf ' %q' "$@")
    fi

    cat > "$helper_script" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec >>$(printf '%q' "$log_path") 2>&1
sleep $(printf '%q' "$PROMOTE_DELAY_SECS")
export AGENTDESK_REPORT_CHANNEL_ID=$(printf '%q' "$REPORT_CHANNEL_ID")
export AGENTDESK_REPORT_PROVIDER=$(printf '%q' "$REPORT_PROVIDER")
export AGENTDESK_REPO_DIR=$(printf '%q' "$REPO")
export AGENTDESK_PROMOTE_DETACHED_CHILD=1
export AGENTDESK_PROMOTE_LOG_PATH=$(printf '%q' "$log_path")
export AGENTDESK_PROMOTE_TEST_MODE=$(printf '%q' "$PROMOTE_TEST_MODE")
cd $(printf '%q' "$REPO")
exec $(printf '%q' "$SCRIPT_DIR/promote-release.sh")${quoted_args}
EOF
    chmod +x "$helper_script"
    tmux new-session -d -s "$helper_session" "$helper_script"

    echo "▸ Self-hosted release promotion detected — using detached helper"
    echo "  helper tmux: $helper_session"
    echo "  helper log: $log_path"
    echo "  current turn will finish before dcserver restart; final result will be reported automatically"
}

# Safety check: review must be passed (unless --skip-review is passed)
if [[ "${1:-}" != "--skip-review" ]]; then
    # Check if the latest commit has a review-passed marker (may be in dev or release runtime)
    LAST_COMMIT=$(cd "$REPO" && git rev-parse HEAD 2>/dev/null)
    REVIEW_MARKER_DEV="$ADK_DEV/runtime/review_passed/$LAST_COMMIT"
    REVIEW_MARKER_REL="$ADK_REL/runtime/review_passed/$LAST_COMMIT"
    if [ ! -f "$REVIEW_MARKER_DEV" ] && [ ! -f "$REVIEW_MARKER_REL" ]; then
        echo "✗ Review not passed for commit $LAST_COMMIT — aborting promotion"
        echo "  Run counter-review first, or use --skip-review to override"
        exit 1
    fi
    echo "▸ Review passed for $LAST_COMMIT"
fi

# Safety check: dev must be healthy
DEV_PORT="${AGENTDESK_DEV_PORT:-8799}"
if ! curl -s --max-time 5 "http://${ADK_DEFAULT_LOOPBACK}:${DEV_PORT}/api/health" | grep -q '"status":"healthy"'; then
    echo "✗ Dev is not healthy — aborting promotion"
    exit 1
fi

echo "▸ Dev is healthy — proceeding"

if ! DASHBOARD_SOURCE=$(_resolve_dashboard_source); then
    echo "✗ Dashboard dist not found in dev or workspace — aborting promotion"
    echo "  looked for:"
    echo "    - $ADK_DEV/dashboard/dist/index.html"
    echo "    - $REPO/dashboard/dist/index.html"
    echo "  Run 'cd $REPO/dashboard && npm run build' to generate it"
    exit 1
fi
if [ "$DASHBOARD_SOURCE" = "$REPO/dashboard/dist" ] && [ ! -f "$ADK_DEV/dashboard/dist/index.html" ]; then
    echo "▸ Dashboard source: workspace fallback ($DASHBOARD_SOURCE)"
else
    echo "▸ Dashboard source: $DASHBOARD_SOURCE"
fi

if _self_hosted_release_session; then
    _spawn_detached_helper "$@"
    exit 0
fi

if [ "$PROMOTE_TEST_MODE" = "1" ]; then
    echo "▸ TEST MODE: skipping release bootout/copy/bootstrap"
    echo "✓ Detached helper dry run complete"
    exit 0
fi

# Ensure release dir exists
mkdir -p "$ADK_REL"/{bin,config,data,logs}

# Stage dashboard before stopping release so missing dist never causes downtime.
echo "▸ Staging dashboard..."
mkdir -p "$ADK_REL/dashboard"
DIST_STAGED="$ADK_REL/dashboard/dist.new"
rm -rf "$DIST_STAGED"
cp -r "$DASHBOARD_SOURCE" "$DIST_STAGED"

# Stop release — wait for process to actually die (flock release)
echo "▸ Stopping release..."
LOCK_FILE="$ADK_REL/runtime/dcserver.lock"
OLD_PID=""
if [ -f "$LOCK_FILE" ]; then
    OLD_PID=$(cat "$LOCK_FILE" 2>/dev/null || true)
fi
launchctl bootout "gui/$(id -u)/$PLIST_REL" 2>/dev/null || true
if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
    echo "  waiting for PID $OLD_PID to exit..."
    WAIT_SECS=0
    while kill -0 "$OLD_PID" 2>/dev/null && [ "$WAIT_SECS" -lt 15 ]; do
        sleep 1
        WAIT_SECS=$((WAIT_SECS + 1))
    done
    if kill -0 "$OLD_PID" 2>/dev/null; then
        echo "  ⚠ PID $OLD_PID did not exit after 15s — sending SIGKILL"
        kill -9 "$OLD_PID" 2>/dev/null || true
        sleep 1
    fi
    echo "  ✓ old process terminated (${WAIT_SECS}s)"
else
    sleep 2
fi

# Copy binary from dev — atomic: sign in tmp, then mv to replace inode.
# In-place codesign can corrupt the OS signing cache if it fails mid-write,
# causing SIGKILL on subsequent launches even though the binary is valid.
echo "▸ Copying binary from dev..."
chflags nouchg "$ADK_REL/bin/agentdesk" 2>/dev/null || true
cp "$ADK_DEV/bin/agentdesk" "$ADK_REL/bin/agentdesk.new"
chmod +x "$ADK_REL/bin/agentdesk.new"
xattr -d com.apple.provenance "$ADK_REL/bin/agentdesk.new" 2>/dev/null || true
sign_binary_with_fallback "$ADK_REL/bin/agentdesk.new"
mv -f "$ADK_REL/bin/agentdesk.new" "$ADK_REL/bin/agentdesk"
# Lock binary to prevent unsigned overwrites
chflags uchg "$ADK_REL/bin/agentdesk"

# Atomic swap: old → .old, staged → dist, cleanup
rm -rf "$ADK_REL/dashboard/dist.old"
[ -d "$ADK_REL/dashboard/dist" ] && mv "$ADK_REL/dashboard/dist" "$ADK_REL/dashboard/dist.old"
mv "$DIST_STAGED" "$ADK_REL/dashboard/dist"
rm -rf "$ADK_REL/dashboard/dist.old"

# Keep the user-facing CLI wrapper discoverable via PATH.
echo "▸ Ensuring global agentdesk CLI..."
"$SCRIPT_DIR/ensure-agentdesk-cli.sh"

# Initialize release database if it doesn't exist (never overwrite release data)
if [ ! -f "$ADK_REL/data/agentdesk.sqlite" ]; then
    echo "▸ Initializing release database from dev..."
    cp "$ADK_DEV/data/agentdesk.sqlite" "$ADK_REL/data/agentdesk.sqlite"
else
    echo "▸ Release database exists — preserving release data (skip copy)"
fi

# Start release
echo "▸ Starting release..."
xattr -d com.apple.quarantine "$HOME/Library/LaunchAgents/$PLIST_REL.plist" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$PLIST_REL.plist"
sleep 3

# Health check (server health + dashboard availability)
REL_PORT="${AGENTDESK_REL_PORT:-$ADK_DEFAULT_PORT}"
HEALTH_JSON=$(curl -s --max-time 5 "http://${ADK_DEFAULT_LOOPBACK}:${REL_PORT}/api/health")
if echo "$HEALTH_JSON" | grep -q '"status":"healthy"'; then
    echo "✓ Release is healthy on :${REL_PORT}"
else
    echo "✗ Release health check failed — check logs: $ADK_REL/logs/"
    exit 1
fi
if ! echo "$HEALTH_JSON" | grep -q '"dashboard":true'; then
    echo "✗ Dashboard not available after promotion — check dist copy"
    exit 1
fi

echo "═══ Promotion Complete ═══"
