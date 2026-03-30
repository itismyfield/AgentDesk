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
PROMOTE_HELPER_SESSION="${AGENTDESK_PROMOTE_HELPER_SESSION:-}"
PROMOTE_TEST_MODE="${AGENTDESK_PROMOTE_TEST_MODE:-0}"
PROMOTE_DELAY_SECS="${AGENTDESK_PROMOTE_DELAY_SECS:-2}"
DASHBOARD_SOURCE=""

echo "═══ ADK Promote Dev → Release ═══"

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
    if [ -d "$ADK_DEV/dashboard/dist" ] && [ -f "$ADK_DEV/dashboard/dist/index.html" ]; then
        printf '%s\n' "$ADK_DEV/dashboard/dist"
        return 0
    fi
    if [ -d "$REPO/dashboard/dist" ] && [ -f "$REPO/dashboard/dist/index.html" ]; then
        printf '%s\n' "$REPO/dashboard/dist"
        return 0
    fi
    return 1
}

_finalize_detached_helper() {
    local status="${1:-0}"
    [ "$PROMOTE_DETACHED_CHILD" = "1" ] || return 0
    [ -n "$REPORT_CHANNEL_ID" ] || return 0

    local content
    if [ "$status" -eq 0 ]; then
        content="✅ release promote helper finished
session: ${PROMOTE_HELPER_SESSION:-unknown}
log: ${PROMOTE_LOG_PATH:-n/a}"
    else
        content="❌ release promote helper failed (exit ${status})
session: ${PROMOTE_HELPER_SESSION:-unknown}
log: ${PROMOTE_LOG_PATH:-n/a}"
    fi

    local summary
    summary=$(_tail_for_summary "$PROMOTE_LOG_PATH")
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
# Wait for the invoking agent's turn to finish before restarting release dcserver.
# Poll the release API for inflight turns on the reporting channel.
REL_PORT="${AGENTDESK_REL_PORT:-$(printf '%q' "$ADK_DEFAULT_PORT")}"
WAIT_CHANNEL=$(printf '%q' "$REPORT_CHANNEL_ID")
MAX_WAIT=300  # 5 minutes max
WAITED=0
echo "▸ Waiting for turn on channel \$WAIT_CHANNEL to complete..."
while [ \$WAITED -lt \$MAX_WAIT ]; do
    # Check if the channel has an active turn via sessions API
    WORKING=\$(curl -sf "http://127.0.0.1:\${REL_PORT}/api/sessions" 2>/dev/null \
        | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    sessions = data if isinstance(data, list) else data.get('sessions', [])
    active = [s for s in sessions if s.get('status') == 'working']
    print(len(active))
except: print('0')
" 2>/dev/null || echo "0")
    if [ "\$WORKING" = "0" ]; then
        echo "✓ No active turns — proceeding with promotion"
        break
    fi
    sleep 5
    WAITED=\$((WAITED + 5))
done
if [ \$WAITED -ge \$MAX_WAIT ]; then
    echo "⚠ Timeout waiting for turn — proceeding anyway"
fi
sleep $(printf '%q' "$PROMOTE_DELAY_SECS")
export AGENTDESK_REPORT_CHANNEL_ID=$(printf '%q' "$REPORT_CHANNEL_ID")
export AGENTDESK_REPORT_PROVIDER=$(printf '%q' "$REPORT_PROVIDER")
export AGENTDESK_REPO_DIR=$(printf '%q' "$REPO")
export AGENTDESK_PROMOTE_DETACHED_CHILD=1
export AGENTDESK_PROMOTE_LOG_PATH=$(printf '%q' "$log_path")
export AGENTDESK_PROMOTE_HELPER_SESSION=$(printf '%q' "$helper_session")
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
DEV_PORT="${AGENTDESK_DEV_PORT:-$ADK_DEFAULT_PORT}"
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

# Stop release
echo "▸ Stopping release..."
launchctl bootout "gui/$(id -u)/$PLIST_REL" 2>/dev/null || true
sleep 2

# Copy binary from dev
echo "▸ Copying binary from dev..."
# Remove immutable flag if set (only deploy scripts should touch the binary)
chflags nouchg "$ADK_REL/bin/agentdesk" 2>/dev/null || true
cp "$ADK_DEV/bin/agentdesk" "$ADK_REL/bin/agentdesk"
chmod +x "$ADK_REL/bin/agentdesk"
xattr -d com.apple.provenance "$ADK_REL/bin/agentdesk" 2>/dev/null || true
codesign -f -s "Developer ID Application: Wonchang Oh (A7LJY7HNGA)" --options runtime --identifier "com.itismyfield.agentdesk" "$ADK_REL/bin/agentdesk"
# Verify signature
if ! codesign -v "$ADK_REL/bin/agentdesk" 2>/dev/null; then
    echo "✗ Codesign verification failed — aborting"
    exit 1
fi
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
