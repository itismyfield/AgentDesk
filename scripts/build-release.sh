#!/bin/bash
# ──────────────────────────────────────────────────────────────────────────────
# build-release.sh — Build AgentDesk release binary + dashboard
#
# Usage:
#   ./scripts/build-release.sh [--skip-dashboard]
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

SKIP_DASHBOARD=false
for arg in "$@"; do
  case "$arg" in
    --skip-dashboard) SKIP_DASHBOARD=true ;;
  esac
done

echo "Building AgentDesk release..."
echo ""

# ── Rust binary ───────────────────────────────────────────────────────────────
if ! command -v cargo &>/dev/null; then
  echo "Error: cargo not found. Install Rust: https://rustup.rs/"
  exit 1
fi

echo "[1/2] Building Rust binary (release)..."
cargo build --release

BINARY="target/release/agentdesk"
if [ ! -f "$BINARY" ]; then
  echo "Error: Binary not found at $BINARY"
  exit 1
fi

echo "  Binary: $(ls -lh "$BINARY" | awk '{print $5, $9}')"
echo ""

# ── Dashboard ─────────────────────────────────────────────────────────────────
if [ "$SKIP_DASHBOARD" = true ]; then
  echo "[2/2] Dashboard build skipped (--skip-dashboard)"
else
  echo "[2/2] Building dashboard..."

  if [ ! -d "dashboard" ]; then
    echo "  Warning: dashboard/ directory not found — skipping"
  else
    cd dashboard

    if ! command -v pnpm &>/dev/null; then
      echo "  Warning: pnpm not found — trying npm"
      if command -v npm &>/dev/null; then
        npm ci --silent 2>/dev/null || npm install --silent
        npm run build
      else
        echo "  Error: No package manager found (pnpm or npm)"
        exit 1
      fi
    else
      pnpm install --frozen-lockfile 2>/dev/null || pnpm install
      pnpm build
    fi

    cd "$PROJECT_DIR"

    if [ -d "dashboard/dist" ]; then
      echo "  Dashboard: $(du -sh dashboard/dist/ | cut -f1)"
    fi
  fi
fi

echo ""
echo "Release build complete."
echo "  Binary:    $PROJECT_DIR/$BINARY"
if [ -d "dashboard/dist" ]; then
  echo "  Dashboard: $PROJECT_DIR/dashboard/dist/"
fi
