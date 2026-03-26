#!/bin/bash
# ──────────────────────────────────────────────────────────────────────────────
# deploy.sh — Build, install, and restart AgentDesk
#
# Steps:
#   1. Build release binary (+ dashboard)
#   2. Copy binary to ~/.adk/release/bin/
#   3. Install/update launchd plist (macOS) or systemd unit (Linux)
#   4. Restart service
#   5. Smoke test (health check)
#
# Usage:
#   ./scripts/deploy.sh [--skip-dashboard] [--skip-build]
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"

AD_HOME="${AGENTDESK_HOME:-$HOME/.adk/release}"
BIN_DIR="$AD_HOME/bin"
HEALTH_PORT="${AGENTDESK_SERVER_PORT:-$ADK_DEFAULT_PORT}"
LABEL="com.agentdesk.release"

SKIP_BUILD=false
SKIP_DASHBOARD=false

for arg in "$@"; do
  case "$arg" in
    --skip-build)     SKIP_BUILD=true ;;
    --skip-dashboard) SKIP_DASHBOARD=true ;;
  esac
done

info()  { printf "\033[1;34m[deploy]\033[0m %s\n" "$*"; }
ok()    { printf "\033[1;32m[deploy]\033[0m %s\n" "$*"; }
fail()  { printf "\033[1;31m[deploy]\033[0m %s\n" "$*"; exit 1; }

# ── Step 1: Build ─────────────────────────────────────────────────────────────
if [ "$SKIP_BUILD" = true ]; then
  info "Build skipped (--skip-build)"
  if [ ! -f "$PROJECT_DIR/target/release/agentdesk" ]; then
    fail "No existing binary at target/release/agentdesk — cannot skip build"
  fi
else
  info "Building release..."
  BUILD_ARGS=()
  if [ "$SKIP_DASHBOARD" = true ]; then
    BUILD_ARGS+=("--skip-dashboard")
  fi
  "$SCRIPT_DIR/build-release.sh" "${BUILD_ARGS[@]}"
fi

# ── Step 2: Copy binary ──────────────────────────────────────────────────────
info "Installing binary..."
mkdir -p "$BIN_DIR"
cp "$PROJECT_DIR/target/release/agentdesk" "$BIN_DIR/agentdesk"
chmod +x "$BIN_DIR/agentdesk"
codesign -s "Developer ID Application: Wonchang Oh (A7LJY7HNGA)" --options runtime --identifier "com.itismyfield.agentdesk" --force "$BIN_DIR/agentdesk" 2>/dev/null || true
ok "Binary: $BIN_DIR/agentdesk (signed as com.itismyfield.agentdesk)"

# Copy dashboard dist if it exists
if [ -d "$PROJECT_DIR/dashboard/dist" ]; then
  mkdir -p "$AD_HOME/dashboard"
  rsync -a --delete "$PROJECT_DIR/dashboard/dist/" "$AD_HOME/dashboard/dist/"
  ok "Dashboard: $AD_HOME/dashboard/dist/"
fi

# ── Step 3: Install/update service ────────────────────────────────────────────
OS=$(uname -s | tr '[:upper:]' '[:lower:]')

install_launchd() {
  local PLIST_SRC="$SCRIPT_DIR/com.agentdesk.release.plist"
  local PLIST_DST="$HOME/Library/LaunchAgents/com.agentdesk.release.plist"

  # Migrate: remove legacy com.agentdesk plist if present
  local LEGACY_PLIST="$HOME/Library/LaunchAgents/com.agentdesk.plist"
  if [ -f "$LEGACY_PLIST" ]; then
    launchctl bootout "gui/$(id -u)/com.agentdesk" 2>/dev/null || true
    rm -f "$LEGACY_PLIST"
    info "Removed legacy plist: $LEGACY_PLIST"
  fi

  if [ ! -f "$PLIST_SRC" ]; then
    fail "Plist template not found: $PLIST_SRC"
  fi

  mkdir -p "$HOME/Library/LaunchAgents"
  mkdir -p "$AD_HOME/logs"

  # Replace placeholders with actual paths
  sed \
    -e "s|AGENTDESK_BIN|$BIN_DIR/agentdesk|g" \
    -e "s|AGENTDESK_HOME|$AD_HOME|g" \
    "$PLIST_SRC" > "$PLIST_DST"

  ok "Plist installed: $PLIST_DST"
}

install_systemd() {
  local UNIT_SRC="$SCRIPT_DIR/agentdesk-dcserver.service"
  local UNIT_DIR="$HOME/.config/systemd/user"
  local UNIT_DST="$UNIT_DIR/agentdesk-dcserver.service"

  if [ ! -f "$UNIT_SRC" ]; then
    fail "Systemd unit template not found: $UNIT_SRC"
  fi

  # Migrate: disable and remove legacy agentdesk.service if present
  local LEGACY_UNIT="$UNIT_DIR/agentdesk.service"
  if [ -f "$LEGACY_UNIT" ]; then
    systemctl --user disable --now agentdesk.service 2>/dev/null || true
    rm -f "$LEGACY_UNIT"
    info "Removed legacy unit: $LEGACY_UNIT"
  fi

  mkdir -p "$UNIT_DIR"
  mkdir -p "$AD_HOME/logs"
  cp "$UNIT_SRC" "$UNIT_DST"

  systemctl --user daemon-reload
  systemctl --user enable agentdesk-dcserver.service

  ok "Systemd unit installed: $UNIT_DST"
}

case "$OS" in
  darwin) install_launchd ;;
  linux)  install_systemd ;;
  *)      info "Unknown OS ($OS) — skipping service install" ;;
esac

# ── Step 4: Restart service ───────────────────────────────────────────────────
info "Restarting service..."

restart_launchd() {
  local PLIST="$HOME/Library/LaunchAgents/com.agentdesk.release.plist"
  if [ ! -f "$PLIST" ]; then
    info "Plist not installed — skipping restart"
    return
  fi

  # Unload (ignore errors if not loaded)
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  sleep 1

  # Load
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
  ok "Service restarted via launchd"
}

restart_systemd() {
  systemctl --user restart agentdesk-dcserver.service
  ok "Service restarted via systemd"
}

case "$OS" in
  darwin) restart_launchd ;;
  linux)  restart_systemd ;;
  *)      info "Restart manually" ;;
esac

# ── Step 5: Smoke test ────────────────────────────────────────────────────────
info "Waiting for health check (port $HEALTH_PORT)..."

RETRIES=10
DELAY=2
HEALTHY=false

for i in $(seq 1 $RETRIES); do
  sleep "$DELAY"
  HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://${ADK_DEFAULT_LOOPBACK}:$HEALTH_PORT/api/health" 2>/dev/null || echo "000")
  if [ "$HTTP_CODE" = "200" ]; then
    HEALTHY=true
    break
  fi
  info "  Attempt $i/$RETRIES — HTTP $HTTP_CODE"
done

if [ "$HEALTHY" = true ]; then
  ok "Health check passed (HTTP 200 on :$HEALTH_PORT/api/health)"
else
  fail "Health check failed after $RETRIES attempts. Check logs:"
  echo "  $AD_HOME/logs/dcserver.stdout.log"
  echo "  $AD_HOME/logs/dcserver.stderr.log"
fi

echo ""
ok "Deploy complete."
