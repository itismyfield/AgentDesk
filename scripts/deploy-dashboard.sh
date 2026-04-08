#!/usr/bin/env bash
# deploy-dashboard.sh — Build and deploy dashboard to dev or release runtime.
# Usage: deploy-dashboard.sh [dev|release]
#   dev     → symlink (instant, hot-reload friendly)
#   release → atomic swap to ~/.adk/release/dashboard/dist/
#   (default: release)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
DASHBOARD_SRC="$REPO/dashboard"
TARGET="${1:-release}"

# Build
echo "▸ Building dashboard..."
cd "$DASHBOARD_SRC"
npm run build --silent
echo "  ✓ Built → $DASHBOARD_SRC/dist/"

if [[ "$TARGET" == "dev" ]]; then
    ADK_DEV="$HOME/.adk/dev"
    mkdir -p "$ADK_DEV/dashboard"
    ln -sfn "$DASHBOARD_SRC/dist" "$ADK_DEV/dashboard/dist"
    echo "  ✓ Symlinked → $ADK_DEV/dashboard/dist"
elif [[ "$TARGET" == "release" ]]; then
    ADK_REL="$HOME/.adk/release"
    DEST="$ADK_REL/dashboard/dist"
    DEST_TMP="$ADK_REL/dashboard/dist.new"
    DEST_OLD="$ADK_REL/dashboard/dist.old"

    # Atomic swap: copy to temp dir, then rename
    rm -rf "$DEST_TMP" "$DEST_OLD"
    mkdir -p "$DEST_TMP"
    cp -r "$DASHBOARD_SRC/dist/"* "$DEST_TMP/"

    # Verify before swap
    if [[ ! -f "$DEST_TMP/index.html" ]]; then
        echo "  ✗ ERROR: index.html missing in staging dir" >&2
        rm -rf "$DEST_TMP"
        exit 1
    fi

    # Swap: old → dist.old, new → dist (near-atomic on same filesystem)
    # Keep dist.old so in-flight clients can still load previous chunks
    rm -rf "$DEST_OLD"
    if [[ -d "$DEST" ]]; then
        mv "$DEST" "$DEST_OLD"
    fi
    mv "$DEST_TMP" "$DEST"

    echo "  ✓ Deployed → $DEST/ (atomic swap, previous build kept in dist.old)"
    echo "  ✓ index.html present"
else
    echo "Usage: deploy-dashboard.sh [dev|release]" >&2
    exit 1
fi

echo "Done. Hard-refresh browser to see changes."
