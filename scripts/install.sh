#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# install.sh — AgentDesk installer bootstrap
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/itismyfield/AgentDesk/main/scripts/install.sh | bash
#
# What it does on macOS:
#   1. Downloads the latest release from GitHub
#   2. Installs to ~/.adk/release/
#   3. Registers launchd service (auto-start on boot)
#   4. Starts the AgentDesk server
#   5. Opens the web dashboard for onboarding
#
# On Linux/Windows, this script prints the native runtime path and exits.
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="itismyfield/AgentDesk"
INSTALL_DIR="$HOME/.adk/release"
LAUNCHD_LABEL="com.agentdesk.release"
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"

# Read defaults from defaults.json if available (single source of truth)
_read_default() {
  local key="$1" fallback="$2" src="$3"
  if [ -f "$src" ]; then
    local val
    val=$(sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\"\{0,1\}\([^,\"]*\)\"\{0,1\}.*/\1/p" "$src" | head -1)
    [ -n "$val" ] && echo "$val" && return
  fi
  echo "$fallback"
}
# During install, defaults.json may exist in the extracted tarball or cloned repo
_DEFAULTS_SRC="${TMPDIR_BUILD:-${TMPDIR_DL:-}}/defaults.json"
DEFAULT_PORT=$(_read_default port 8791 "$_DEFAULTS_SRC")
DEFAULT_HOST=$(_read_default host "127.0.0.1" "$_DEFAULTS_SRC")
DEFAULT_LOOPBACK=$(_read_default loopback "127.0.0.1" "$_DEFAULTS_SRC")
if [ "$DEFAULT_HOST" = "0.0.0.0" ]; then
  DEFAULT_HOST="$DEFAULT_LOOPBACK"
fi

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${CYAN}▸${NC} $1"; }
ok()    { echo -e "${GREEN}✓${NC} $1"; }
warn()  { echo -e "${YELLOW}⚠${NC} $1"; }
fail()  { echo -e "${RED}✗${NC} $1"; exit 1; }

print_native_runtime_help() {
  local os="$1"
  local docs_url="https://github.com/$REPO#windows-and-linux-native-runtime"

  echo ""
  case "$os" in
    linux)
      warn "Linux uses the native runtime path instead of the one-click bootstrap."
      cat <<EOF
Recommended path:
  1. Download the release tarball or build from source
     cargo build --release
  2. Initialize the runtime
     ./target/release/agentdesk --init
  3. Start the server and run diagnostics
     ./target/release/agentdesk dcserver
     ./target/release/agentdesk doctor

Use the service path printed by \`agentdesk --init\` when registering a systemd --user service.
Docs: $docs_url
EOF
      ;;
    windows)
      warn "Windows uses the native runtime path instead of the macOS launchd bootstrap."
      cat <<EOF
Recommended path:
  1. Download the release zip or build from source
     cargo build --release
  2. Initialize the runtime
     .\\target\\release\\agentdesk.exe --init
  3. Start the server and run diagnostics
     .\\target\\release\\agentdesk.exe dcserver
     .\\target\\release\\agentdesk.exe doctor

Use the NSSM / sc.exe service path printed by \`agentdesk.exe --init\`.
Docs: $docs_url
EOF
      ;;
    *)
      warn "This operating system is not supported by the one-click installer."
      echo "Docs: $docs_url"
      ;;
  esac
}

sign_binary_with_fallback() {
  local target="$1"
  local identity="${CODESIGN_IDENTITY:--}"

  if [ -n "$identity" ] && [ "$identity" != "-" ] && command -v security >/dev/null 2>&1; then
    if ! security find-identity -v -p codesigning 2>/dev/null | grep -Fq "$identity"; then
      warn "Signing identity not found locally; falling back to ad-hoc signature"
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
    fail "Codesign verification failed"
  fi
}

# ── Detect OS and arch ────────────────────────────────────────────────────────
RAW_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$RAW_OS" in
  darwin) OS="darwin" ;;
  linux) OS="linux" ;;
  msys*|mingw*|cygwin*) OS="windows" ;;
  *) fail "Unsupported operating system: $RAW_OS" ;;
esac

ARCH=$(uname -m)
case "$ARCH" in
  x86_64)        ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) fail "Unsupported architecture: $ARCH" ;;
esac

if [ "$OS" != "darwin" ]; then
  print_native_runtime_help "$OS"
  fail "One-click installer is only available on macOS."
fi

echo ""
echo -e "${BOLD}═══ AgentDesk Installer ═══${NC}"
echo ""

# ── Check dependencies ────────────────────────────────────────────────────────
if ! command -v curl &>/dev/null; then
  fail "curl is required but not found"
fi
if ! command -v tar &>/dev/null; then
  fail "tar is required but not found"
fi

# ── Download latest release ───────────────────────────────────────────────────
ARTIFACT="agentdesk-${OS}-${ARCH}"

info "Checking latest release..."
LATEST_TAG=$(curl -sfL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name"' | head -1 | sed 's/.*: *"\(.*\)".*/\1/')

if [ -z "$LATEST_TAG" ]; then
  # No releases yet — fall back to building from source
  warn "No GitHub release found. Falling back to source build..."

  if ! command -v cargo &>/dev/null; then
    echo ""
    echo -e "${YELLOW}Rust toolchain required for source build:${NC}"
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    echo ""
    fail "Install Rust first, then re-run this script"
  fi

  if ! command -v git &>/dev/null; then
    fail "git is required for source build"
  fi

  TMPDIR_BUILD="${TMPDIR:-/tmp}/agentdesk-install-$$"
  info "Cloning repository..."
  git clone --depth 1 "https://github.com/$REPO.git" "$TMPDIR_BUILD"

  info "Building from source (this may take a few minutes)..."
  cd "$TMPDIR_BUILD"
  cargo build --release 2>&1 | tail -3

  # Build dashboard if npm available
  if command -v npm &>/dev/null && [ -d "dashboard" ]; then
    info "Building dashboard..."
    (cd dashboard && npm ci --silent 2>/dev/null && npm run build 2>&1 | tail -1) || true
  fi

  # Install
  mkdir -p "$INSTALL_DIR"/{bin,config,data,logs,policies,dashboard,skills}
  cp target/release/agentdesk "$INSTALL_DIR/bin/"
  chmod +x "$INSTALL_DIR/bin/agentdesk"

  if [ -d "dashboard/dist" ]; then
    cp -r dashboard/dist "$INSTALL_DIR/dashboard/dist"
  fi

  if [ -d "policies" ]; then
    cp policies/*.js "$INSTALL_DIR/policies/"
  fi

  if [ -d "skills" ]; then
    rsync -a --delete "skills/" "$INSTALL_DIR/skills/"
  fi

  cd /
  rm -rf "$TMPDIR_BUILD"
  ok "Built and installed from source"
else
  DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/${ARTIFACT}.tar.gz"
  info "Downloading $LATEST_TAG..."

  TMPDIR_DL="${TMPDIR:-/tmp}/agentdesk-install-$$"
  mkdir -p "$TMPDIR_DL"

  if ! curl -fSL "$DOWNLOAD_URL" -o "$TMPDIR_DL/${ARTIFACT}.tar.gz"; then
    fail "Download failed. URL: $DOWNLOAD_URL"
  fi

  info "Extracting..."
  cd "$TMPDIR_DL"
  tar xzf "${ARTIFACT}.tar.gz"

  # Install
  mkdir -p "$INSTALL_DIR"/{bin,config,data,logs,skills}
  cp "${ARTIFACT}/agentdesk" "$INSTALL_DIR/bin/"
  chmod +x "$INSTALL_DIR/bin/agentdesk"

  if [ -d "${ARTIFACT}/dashboard" ]; then
    rm -rf "$INSTALL_DIR/dashboard"
    cp -r "${ARTIFACT}/dashboard" "$INSTALL_DIR/dashboard"
  fi

  if [ -d "${ARTIFACT}/policies" ]; then
    mkdir -p "$INSTALL_DIR/policies"
    cp "${ARTIFACT}/policies/"*.js "$INSTALL_DIR/policies/"
  fi

  if [ -d "${ARTIFACT}/skills" ]; then
    rsync -a --delete "${ARTIFACT}/skills/" "$INSTALL_DIR/skills/"
  fi

  cd /
  rm -rf "$TMPDIR_DL"
  ok "Installed $LATEST_TAG"
fi

# ── Code signing (macOS) ──────────────────────────────────────────────────────
if [ "$OS" = "darwin" ]; then
  chflags nouchg "$INSTALL_DIR/bin/agentdesk" 2>/dev/null || true
  sign_binary_with_fallback "$INSTALL_DIR/bin/agentdesk"
  chflags uchg "$INSTALL_DIR/bin/agentdesk"

  # Register with firewall
  FW=/usr/libexec/ApplicationFirewall/socketfilterfw
  if [ -f "$FW" ]; then
    sudo "$FW" --add "$INSTALL_DIR/bin/agentdesk" 2>/dev/null || true
  fi
fi

# ── Create default config if not exists ───────────────────────────────────────
if [ ! -f "$INSTALL_DIR/agentdesk.yaml" ]; then
  cat > "$INSTALL_DIR/agentdesk.yaml" << YAML
# AgentDesk Configuration
# Edit this file to add Discord bot tokens and customize settings.
# Run the web onboarding wizard for guided setup: http://${DEFAULT_LOOPBACK}:${DEFAULT_PORT}

server:
  port: ${DEFAULT_PORT}
  host: "${DEFAULT_HOST}"

discord:
  bots: {}

memory:
  backend: auto

# Optional startup baselines for dashboard-managed settings:
# kanban:
#   manager_channel_id: "123456789012345678"
# review:
#   enabled: true
# runtime:
#   dispatch_poll_sec: 30
#   reset_overrides_on_restart: false
# automation:
#   strategy: "squash"
YAML
  ok "Created default config: $INSTALL_DIR/agentdesk.yaml"
fi

# ── Register launchd service ──────────────────────────────────────────────────
info "Setting up launchd service..."

PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST_PATH="$PLIST_DIR/$LAUNCHD_LABEL.plist"
mkdir -p "$PLIST_DIR"
"$INSTALL_DIR/bin/agentdesk" emit-launchd-plist \
  --flavor release \
  --home "$HOME" \
  --root-dir "$INSTALL_DIR" \
  --agentdesk-bin "$INSTALL_DIR/bin/agentdesk" \
  --output "$PLIST_PATH"

ok "Launchd plist: $PLIST_PATH"

# ── Start dcserver ────────────────────────────────────────────────────────────
info "Starting AgentDesk..."

# Stop existing instance if running
launchctl bootout "gui/$(id -u)/$LAUNCHD_LABEL" 2>/dev/null || true
sleep 1

# Remove quarantine flag if present
xattr -d com.apple.quarantine "$PLIST_PATH" 2>/dev/null || true

# Start
if launchctl bootstrap "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null; then
  sleep 3

  # Health check
  if curl -sf --max-time 5 "http://${DEFAULT_LOOPBACK}:$DEFAULT_PORT/api/health" | grep -q '"status":"healthy"'; then
    ok "AgentDesk is running on port $DEFAULT_PORT"
  else
    warn "Service started but health check pending. Check logs: $INSTALL_DIR/logs/"
  fi
else
  warn "launchd bootstrap failed. Try manually:"
  echo "  launchctl bootstrap gui/\$(id -u) $PLIST_PATH"
fi

# ── Open browser ──────────────────────────────────────────────────────────────
DASHBOARD_URL="http://${DEFAULT_LOOPBACK}:$DEFAULT_PORT"

echo ""
echo -e "${BOLD}═══ Installation Complete ═══${NC}"
echo ""
echo -e "  Dashboard:  ${CYAN}$DASHBOARD_URL${NC}"
echo -e "  Config:     $INSTALL_DIR/agentdesk.yaml"
echo -e "  Logs:       $INSTALL_DIR/logs/"
echo -e "  Data:       $INSTALL_DIR/data/"
echo ""

# Auto-open browser
if command -v open &>/dev/null; then
  info "Opening dashboard in browser..."
  open "$DASHBOARD_URL"
fi

echo -e "${GREEN}${BOLD}Complete the setup in the web onboarding wizard.${NC}"
echo ""
