#!/usr/bin/env bash
set -euo pipefail

dashboard_dir="${1:-}"
if [ -z "$dashboard_dir" ]; then
    echo "usage: $0 <dashboard-dir>" >&2
    exit 2
fi
if [ ! -f "$dashboard_dir/package.json" ] || [ ! -f "$dashboard_dir/package-lock.json" ]; then
    echo "dashboard package.json and package-lock.json are required: $dashboard_dir" >&2
    exit 2
fi

# Dashboard compilation requires TypeScript, Vite, and the other devDependencies.
# An inherited NODE_ENV=production or npm omit=dev policy must not silently prune
# those build tools from an otherwise deterministic package-lock install.
echo "==> Dashboard dependency install (npm ci, including build dependencies)"
(
    cd "$dashboard_dir"
    npm ci --include=dev --no-audit --no-fund
)
