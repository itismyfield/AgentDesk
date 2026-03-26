#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

ADK_DEV="$HOME/.adk/dev"
ADK_REL="$HOME/.adk/release"
PLIST_REL="com.agentdesk.release"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "═══ ADK Promote Dev → Release ═══"

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

# Ensure release dir exists
mkdir -p "$ADK_REL"/{bin,config,data,logs}

# Stop release
echo "▸ Stopping release..."
launchctl bootout "gui/$(id -u)/$PLIST_REL" 2>/dev/null || true
sleep 2

# Copy binary from dev
echo "▸ Copying binary from dev..."
cp "$ADK_DEV/bin/agentdesk" "$ADK_REL/bin/agentdesk"
chmod +x "$ADK_REL/bin/agentdesk"
xattr -d com.apple.provenance "$ADK_REL/bin/agentdesk" 2>/dev/null || true
codesign -f -s "Developer ID Application: Wonchang Oh (A7LJY7HNGA)" --options runtime "$ADK_REL/bin/agentdesk" 2>/dev/null || true

# Copy dashboard from dev (with fallback to workspace source)
# Stage into a temp dir first, then swap — never delete existing dist before new one is ready
echo "▸ Copying dashboard from dev..."
mkdir -p "$ADK_REL/dashboard"
DIST_STAGED="$ADK_REL/dashboard/dist.new"
rm -rf "$DIST_STAGED"
if [ -d "$ADK_DEV/dashboard/dist" ] && [ -f "$ADK_DEV/dashboard/dist/index.html" ]; then
    cp -r "$ADK_DEV/dashboard/dist" "$DIST_STAGED"
elif [ -d "$REPO/dashboard/dist" ] && [ -f "$REPO/dashboard/dist/index.html" ]; then
    echo "  ⚠ Dev dist missing, falling back to workspace source"
    cp -r "$REPO/dashboard/dist" "$DIST_STAGED"
else
    echo "✗ Dashboard dist not found in dev or workspace — aborting promotion"
    echo "  Run 'cd $REPO/dashboard && npm run build' to generate it"
    exit 1
fi
# Atomic swap: old → .old, staged → dist, cleanup
rm -rf "$ADK_REL/dashboard/dist.old"
[ -d "$ADK_REL/dashboard/dist" ] && mv "$ADK_REL/dashboard/dist" "$ADK_REL/dashboard/dist.old"
mv "$DIST_STAGED" "$ADK_REL/dashboard/dist"
rm -rf "$ADK_REL/dashboard/dist.old"

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
