#!/usr/bin/env bash
# deploy-dashboard.sh — Build and deploy dashboard to dev or release runtime.
# Usage: deploy-dashboard.sh [dev|release]
#   dev     → symlink (instant, hot-reload friendly)
#   release → copy to ~/.adk/release/dashboard/dist/
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
    mkdir -p "$DEST"
    # Clean old assets to avoid hash collision
    rm -rf "$DEST/assets"
    cp -r "$DASHBOARD_SRC/dist/"* "$DEST/"
    echo "  ✓ Deployed → $DEST/"
    # Verify
    if [[ -f "$DEST/index.html" ]]; then
        echo "  ✓ index.html present"
    else
        echo "  ✗ ERROR: index.html missing at $DEST" >&2
        exit 1
    fi
else
    echo "Usage: deploy-dashboard.sh [dev|release]" >&2
    exit 1
fi

echo "Done. Hard-refresh browser to see changes."
