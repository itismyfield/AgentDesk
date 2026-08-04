#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="${1:-$(cd "$SCRIPT_DIR/.." && pwd)}"
NVMRC="$REPO/.nvmrc"
DASHBOARD_DIR="$REPO/dashboard"

if [ ! -f "$NVMRC" ]; then
  echo "Error: .nvmrc is required to determine the dashboard Node version" >&2
  exit 1
fi

REQUIRED_NODE_VERSION="$(tr -d '[:space:]' < "$NVMRC")"
if ! printf '%s\n' "$REQUIRED_NODE_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "Error: invalid dashboard Node version in .nvmrc: $REQUIRED_NODE_VERSION" >&2
  exit 1
fi

if ! command -v node >/dev/null 2>&1; then
  echo "Error: dashboard requires Node >=${REQUIRED_NODE_VERSION}, but node is not installed" >&2
  exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "Error: dashboard requires npm, but npm is not installed" >&2
  exit 1
fi

if ! REQUIRED_NODE_VERSION="$REQUIRED_NODE_VERSION" node -e '
const min = process.env.REQUIRED_NODE_VERSION.split(".").map(Number);
const cur = process.versions.node.split(".").map(Number);
const ok = cur[0] > min[0]
  || (cur[0] === min[0] && cur[1] > min[1])
  || (cur[0] === min[0] && cur[1] === min[1] && cur[2] >= min[2]);
process.exit(ok ? 0 : 1);
'; then
  echo "Error: dashboard requires Node >=${REQUIRED_NODE_VERSION} (found $(node --version 2>/dev/null || echo unknown))" >&2
  echo "       Upgrade Node (for nvm: nvm install ${REQUIRED_NODE_VERSION}) and retry." >&2
  exit 1
fi

if [ ! -f "$DASHBOARD_DIR/package.json" ]; then
  echo "Error: dashboard/package.json missing" >&2
  exit 1
fi

if [ ! -f "$DASHBOARD_DIR/package-lock.json" ]; then
  echo "Error: dashboard/package-lock.json missing — deterministic install is unavailable" >&2
  exit 1
fi
