#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
DASHBOARD_DIR="$REPO/dashboard"
REQUIRED_NODE_MAJOR=22

if ! command -v node >/dev/null 2>&1; then
  echo "Error: node is required to verify the dashboard" >&2
  exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "Error: npm is required to verify the dashboard" >&2
  exit 1
fi

NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
if [ "$NODE_MAJOR" -lt "$REQUIRED_NODE_MAJOR" ]; then
  echo "Error: dashboard verification requires Node >=${REQUIRED_NODE_MAJOR} (found $(node -v))" >&2
  exit 1
fi

if [ ! -f "$DASHBOARD_DIR/package.json" ]; then
  echo "Error: dashboard/package.json missing" >&2
  exit 1
fi

if [ ! -f "$DASHBOARD_DIR/package-lock.json" ]; then
  echo "Error: dashboard/package-lock.json missing" >&2
  exit 1
fi

cd "$DASHBOARD_DIR"

echo "==> Dashboard dependency install (npm ci)"
npm ci --no-audit --no-fund

echo "==> Dashboard build"
npm run build

echo "==> Dashboard test"
npm test
