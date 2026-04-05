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
LIBEXEC_DIR="$AD_HOME/libexec"
WRAPPER_BIN="$BIN_DIR/agentdesk"
REAL_BIN="$LIBEXEC_DIR/agentdesk"
HEALTH_PORT="${AGENTDESK_SERVER_PORT:-$ADK_DEFAULT_PORT}"
LABEL="com.agentdesk.release"
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"

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

BACKUP_WRAPPER=""
BACKUP_REAL=""

cleanup_backup() {
  if [ -n "${BACKUP_WRAPPER:-}" ] && [ -f "$BACKUP_WRAPPER" ]; then
    rm -f "$BACKUP_WRAPPER"
  fi
  if [ -n "${BACKUP_REAL:-}" ] && [ -f "$BACKUP_REAL" ]; then
    rm -f "$BACKUP_REAL"
  fi
}

trap cleanup_backup EXIT

print_recent_macos_binary_logs() {
  if [ "$OS" != "darwin" ]; then
    return
  fi

  local log_cmd="/usr/bin/log"
  if [ ! -x "$log_cmd" ]; then
    return
  fi

  echo "  Recent macOS policy logs for $BIN_DIR/agentdesk:"
  "$log_cmd" show --last 2m --style compact \
    --predicate "eventMessage CONTAINS[c] \"$WRAPPER_BIN\" OR process == \"agentdesk\"" \
    2>/dev/null | tail -n 20 || true
}

write_wrapper_script() {
  cat > "$WRAPPER_BIN" <<EOF
#!/bin/bash
exec "$REAL_BIN" "\$@"
EOF
  chmod +x "$WRAPPER_BIN"
}

restore_previous_install() {
  if [ -n "${BACKUP_WRAPPER:-}" ] && [ -f "$BACKUP_WRAPPER" ]; then
    cp "$BACKUP_WRAPPER" "$WRAPPER_BIN"
    chmod +x "$WRAPPER_BIN"
  else
    rm -f "$WRAPPER_BIN"
  fi

  if [ -n "${BACKUP_REAL:-}" ] && [ -f "$BACKUP_REAL" ]; then
    cp "$BACKUP_REAL" "$REAL_BIN"
    chmod +x "$REAL_BIN"
  else
    rm -f "$REAL_BIN"
  fi
}

run_installed_binary_self_check() {
  local stdout_file stderr_file version_line exit_code
  stdout_file="$(mktemp)"
  stderr_file="$(mktemp)"

  if "$WRAPPER_BIN" --version >"$stdout_file" 2>"$stderr_file"; then
    version_line="$(head -n 1 "$stdout_file" | tr -d '\r')"
    rm -f "$stdout_file" "$stderr_file"
    if [ -n "$version_line" ]; then
      ok "Installed binary self-check passed: $version_line"
    else
      ok "Installed binary self-check passed: --version executed successfully"
    fi
    return 0
  else
    exit_code=$?
  fi

  echo "  Installed binary self-check failed (exit $exit_code)"
  if [ -s "$stdout_file" ]; then
    echo "  stdout:"
    sed 's/^/    /' "$stdout_file"
  fi
  if [ -s "$stderr_file" ]; then
    echo "  stderr:"
    sed 's/^/    /' "$stderr_file"
  fi
  rm -f "$stdout_file" "$stderr_file"
  print_recent_macos_binary_logs

  restore_previous_install
  ok "Restored previous install after failed self-check"

  fail "Installed binary self-check failed for $WRAPPER_BIN"
}

sign_binary_with_fallback() {
  local target="$1"
  local identity="${CODESIGN_IDENTITY:--}"

  if [ -n "$identity" ] && [ "$identity" != "-" ] && command -v security >/dev/null 2>&1; then
    if ! security find-identity -v -p codesigning 2>/dev/null | grep -Fq "$identity"; then
      info "Signing identity not found locally; falling back to ad-hoc signature"
      identity="-"
    fi
  fi

  if [ -z "$identity" ]; then
    identity="-"
  fi

  if [ "$identity" = "-" ]; then
    codesign -s "$identity" --identifier "com.itismyfield.agentdesk" --force "$target"
  else
    codesign -s "$identity" --options runtime --identifier "com.itismyfield.agentdesk" --force "$target"
  fi
  if ! codesign -v "$target" 2>/dev/null; then
    fail "Codesign verification failed — aborting"
  fi
}

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
  if [ ${#BUILD_ARGS[@]} -gt 0 ]; then
    "$SCRIPT_DIR/build-release.sh" "${BUILD_ARGS[@]}"
  else
    "$SCRIPT_DIR/build-release.sh"
  fi
fi

# ── Step 2: Copy binary ──────────────────────────────────────────────────────
info "Installing binary..."
mkdir -p "$BIN_DIR"
if [ "$OS" = "darwin" ]; then
  # Previous installs may have been immutable; unlock before backup/replace.
  chflags nouchg "$WRAPPER_BIN" "$REAL_BIN" 2>/dev/null || true
fi
mkdir -p "$LIBEXEC_DIR"
if [ -e "$WRAPPER_BIN" ]; then
  BACKUP_WRAPPER="$(mktemp "$BIN_DIR/agentdesk.wrapper.backup.XXXXXX")"
  cp "$WRAPPER_BIN" "$BACKUP_WRAPPER"
fi
if [ -e "$REAL_BIN" ]; then
  BACKUP_REAL="$(mktemp "$LIBEXEC_DIR/agentdesk.real.backup.XXXXXX")"
  cp "$REAL_BIN" "$BACKUP_REAL"
fi
rm -f "$REAL_BIN"
cp "$PROJECT_DIR/target/release/agentdesk" "$REAL_BIN"
chmod +x "$REAL_BIN"
if [ "$OS" = "darwin" ]; then
  sign_binary_with_fallback "$REAL_BIN"
fi
write_wrapper_script
ok "Binary wrapper: $WRAPPER_BIN -> $REAL_BIN"
run_installed_binary_self_check
rm -f "$BIN_DIR/agentdesk-real"

# Copy dashboard dist if it exists
if [ -d "$PROJECT_DIR/dashboard/dist" ]; then
  mkdir -p "$AD_HOME/dashboard"
  rsync -a --delete "$PROJECT_DIR/dashboard/dist/" "$AD_HOME/dashboard/dist/"
  ok "Dashboard: $AD_HOME/dashboard/dist/"
fi

# ── Step 3: Install/update service ────────────────────────────────────────────
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
  local bootstrap_log
  if [ ! -f "$PLIST" ]; then
    info "Plist not installed — skipping restart"
    return
  fi

  # Unload (ignore errors if not loaded)
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  sleep 1

  # Load
  bootstrap_log="$(mktemp)"
  for attempt in 1 2 3; do
    if launchctl bootstrap "gui/$(id -u)" "$PLIST" >"$bootstrap_log" 2>&1; then
      rm -f "$bootstrap_log"
      ok "Service restarted via launchd"
      return
    fi

    info "launchd bootstrap failed on attempt $attempt/3"
    sed 's/^/  /' "$bootstrap_log" || true
    sleep 2
  done

  rm -f "$bootstrap_log"
  fail "launchd bootstrap failed after retries"
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
