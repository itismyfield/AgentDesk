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

REPO="${AGENTDESK_INSTALL_REPO:-itismyfield/AgentDesk}"
DEFAULT_INSTALL_DIR="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
INSTALL_DIR="${AGENTDESK_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"
LAUNCHD_LABEL="${AGENTDESK_LAUNCHD_LABEL:-}"
INSTALL_PORT="${AGENTDESK_INSTALL_PORT:-}"
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-Developer ID Application: Wonchang Oh (A7LJY7HNGA)}"
CONFIG_PATH="$INSTALL_DIR/config/agentdesk.yaml"
LEGACY_CONFIG_PATH="$INSTALL_DIR/agentdesk.yaml"

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

launchd_domain() {
  local uid domain
  uid="$(id -u 2>/dev/null)" || return 1
  for domain in "gui/$uid" "user/$uid"; do
    if launchctl print "$domain" >/dev/null 2>&1; then
      printf '%s\n' "$domain"
      return 0
    fi
  done
  printf 'gui/%s\n' "$uid"
}

normalize_install_dir() {
  local raw="$1" dir base
  case "$raw" in
    /*) ;;
    *) raw="$(pwd -P)/$raw" ;;
  esac
  dir="$(dirname "$raw")"
  base="$(basename "$raw")"
  mkdir -p "$dir"
  (cd "$dir" 2>/dev/null && printf '%s/%s\n' "$(pwd -P)" "$base") || printf '%s\n' "$raw"
}

install_root_is_default() {
  [ "$INSTALL_DIR" = "$DEFAULT_INSTALL_DIR" ]
}

default_launchd_label() {
  if install_root_is_default; then
    printf '%s\n' "com.agentdesk.release"
    return
  fi

  local base slug checksum
  base="$(basename "$INSTALL_DIR")"
  slug="$(
    printf '%s' "$base" \
      | tr '[:upper:]' '[:lower:]' \
      | tr -cs '[:alnum:]' '-' \
      | sed 's/^-//;s/-$//;s/--*/-/g'
  )"
  [ -n "$slug" ] || slug="sandbox"
  checksum="$(printf '%s' "$INSTALL_DIR" | cksum | awk '{print $1}')"
  printf 'com.agentdesk.release.%s.%s\n' "$slug" "$checksum"
}

default_install_port() {
  if install_root_is_default; then
    printf '%s\n' "$DEFAULT_PORT"
    return
  fi

  local checksum
  checksum="$(printf '%s' "$INSTALL_DIR" | cksum | awk '{print $1}')"
  printf '%s\n' "$((18000 + checksum % 20000))"
}

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
     ./target/release/agentdesk init
  3. Start the server and run diagnostics
     ./target/release/agentdesk dcserver
     ./target/release/agentdesk doctor

Use the service path printed by \`agentdesk init\` when registering a systemd --user service.
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
     .\\target\\release\\agentdesk.exe init
  3. Start the server and run diagnostics
     .\\target\\release\\agentdesk.exe dcserver
     .\\target\\release\\agentdesk.exe doctor

Use the NSSM / sc.exe service path printed by \`agentdesk.exe init\`.
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

agentdesk_supports_emit_launchd_label() {
  "$1" emit-launchd-plist --help 2>&1 | grep -q -- "--label"
}

INSTALL_STAGE=""
INSTALL_SCRATCH=""
BINARY_REPLACED=0
OLD_BINARY_PRESENT=0
OLD_BINARY_LOCKED=0
INSTALL_COMMITTED=0

finish_install() {
  local status="$?" target="$INSTALL_DIR/bin/agentdesk"
  trap - EXIT
  if [ "$INSTALL_COMMITTED" = 0 ] && [ -n "$INSTALL_STAGE" ]; then
    if [ "$OLD_BINARY_PRESENT" = 1 ]; then
      if [ "$BINARY_REPLACED" = 1 ] && [ ! -f "$INSTALL_STAGE/agentdesk" ]; then
        [ ! -f "$target" ] || chflags nouchg "$target" || status=1
        if ! mv -f "$INSTALL_STAGE/previous" "$target"; then
          [ "$OLD_BINARY_LOCKED" = 0 ] || chflags uchg "$INSTALL_STAGE/previous"
          warn "Binary recovery failed; previous binary retained at $INSTALL_STAGE/previous"
          exit 1
        fi
      fi
      if [ "$OLD_BINARY_LOCKED" = 1 ]; then
        chflags uchg "$target" || status=1
      fi
    elif [ "$BINARY_REPLACED" = 1 ] && [ -f "$target" ]; then
      chflags nouchg "$target" || status=1
      rm -f "$target" || status=1
    fi
  fi
  if [ -n "$INSTALL_STAGE" ]; then
    # Only installer-created staging files are removed.
    rm -f "$INSTALL_STAGE/agentdesk" "$INSTALL_STAGE/previous" || status=1
    rmdir "$INSTALL_STAGE" || status=1
  fi
  if [ -n "$INSTALL_SCRATCH" ]; then
    cd /
    rm -r "$INSTALL_SCRATCH" || status=1
  fi
  exit "$status"
}

install_runtime() {
  local source="$1" binary="$2" target="$INSTALL_DIR/bin/agentdesk" flags
  mkdir -p "$INSTALL_DIR"/{bin,config,data,logs}
  INSTALL_STAGE=$(mktemp -d "$INSTALL_DIR/bin/.agentdesk-install.XXXXXX")
  if [ -L "$target" ] || { [ -e "$target" ] && [ ! -f "$target" ]; }; then
    fail "Existing agentdesk binary is not a regular file"
  fi
  cp "$binary" "$INSTALL_STAGE/agentdesk"
  chmod +x "$INSTALL_STAGE/agentdesk"
  sign_binary_with_fallback "$INSTALL_STAGE/agentdesk"
  "$INSTALL_STAGE/agentdesk" --version
  python3 "$source/scripts/install_runtime_assets.py" "$source" "$INSTALL_DIR"
  if [ -d "$source/dashboard/dist" ]; then
    mkdir -p "$INSTALL_DIR/dashboard/dist"
    cp -R "$source/dashboard/dist/." "$INSTALL_DIR/dashboard/dist/"
  fi
  if [ -f "$target" ]; then
    flags=$(stat -f '%Sf' "$target")
    case ",$flags," in *,uchg,*) OLD_BINARY_LOCKED=1 ;; esac
    cp -p "$target" "$INSTALL_STAGE/previous"
    chflags nouchg "$INSTALL_STAGE/previous"
    OLD_BINARY_PRESENT=1
    chflags nouchg "$target"
  fi
  # Rename replaces the directory entry atomically; the saved binary remains available for rollback.
  BINARY_REPLACED=1
  mv -f "$INSTALL_STAGE/agentdesk" "$target"
  chflags uchg "$target"
}

verify_installed_policies() {
  local response
  response=$(curl -fsS --max-time 5 \
    -H "Origin: http://${DEFAULT_LOOPBACK}:$INSTALL_PORT" \
    "http://${DEFAULT_LOOPBACK}:$INSTALL_PORT/api/policies") || return 1
  python3 -c '
import json, pathlib, sys
root = pathlib.Path(sys.argv[1]).resolve() / "policies"
policies = json.load(sys.stdin)["policies"]
for name in ("auto-queue", "timeouts", "kanban-rules"):
    expected = str(root / (name + ".js"))
    if not any(p.get("name") == name and pathlib.Path(p.get("file", "")).is_absolute()
               and str(pathlib.Path(p["file"]).resolve()) == expected for p in policies):
        sys.exit("Required policy did not register from installation: " + name)
' "$INSTALL_DIR" <<< "$response"
}

read_boot_incarnation() {
  local response
  response=$(curl -sS --max-time 5 \
    -H "Origin: http://${DEFAULT_LOOPBACK}:$INSTALL_PORT" \
    "http://${DEFAULT_LOOPBACK}:$INSTALL_PORT/api/health/detail") || return 1
  python3 -c '
import json, sys
value = json.load(sys.stdin).get("boot_incarnation")
if not isinstance(value, str) or not value:
    sys.exit(1)
print(value)
' <<< "$response"
}

emit_launchd_plist() {
  local agentdesk_bin="$1" plist_path="$2"
  if [ "$LAUNCHD_LABEL" = "com.agentdesk.release" ]; then
    "$agentdesk_bin" emit-launchd-plist \
      --flavor release \
      --home "$HOME" \
      --root-dir "$INSTALL_DIR" \
      --agentdesk-bin "$agentdesk_bin" \
      --output "$plist_path"
    return
  fi

  if ! agentdesk_supports_emit_launchd_label "$agentdesk_bin"; then
    fail "Installed agentdesk binary does not support custom launchd labels; use the default install root or install a newer AgentDesk build."
  fi

  "$agentdesk_bin" emit-launchd-plist \
    --flavor release \
    --label "$LAUNCHD_LABEL" \
    --home "$HOME" \
    --root-dir "$INSTALL_DIR" \
    --agentdesk-bin "$agentdesk_bin" \
    --output "$plist_path"
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

DEFAULT_INSTALL_DIR="$(normalize_install_dir "$DEFAULT_INSTALL_DIR")"
INSTALL_DIR="$(normalize_install_dir "$INSTALL_DIR")"
CONFIG_PATH="$INSTALL_DIR/config/agentdesk.yaml"
LEGACY_CONFIG_PATH="$INSTALL_DIR/agentdesk.yaml"

if [ -z "$LAUNCHD_LABEL" ]; then
  LAUNCHD_LABEL="$(default_launchd_label)"
fi
if [ -z "$INSTALL_PORT" ]; then
  INSTALL_PORT="$(default_install_port)"
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
if ! command -v python3 &>/dev/null; then
  fail "python3 is required for runtime asset installation and policy verification"
fi
trap finish_install EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ── Download latest release ───────────────────────────────────────────────────
ARTIFACT="agentdesk-${OS}-${ARCH}"

info "Checking latest release..."
LATEST_TAG=$(
  curl -sfL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
    | grep '"tag_name"' \
    | head -1 \
    | sed 's/.*: *"\(.*\)".*/\1/' \
    || true
)

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

  INSTALL_SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-install.XXXXXX")
  TMPDIR_BUILD="$INSTALL_SCRATCH/source"
  info "Cloning repository..."
  git clone --depth 1 "https://github.com/$REPO.git" "$TMPDIR_BUILD"

  info "Building from source (this may take a few minutes)..."
  cd "$TMPDIR_BUILD"
  # #2301 follow-up: opt into sccache if installed so the bootstrap build
  # doesn't pay the ~5–10 min cold-cache cost on every install. The
  # repo's `.cargo/config.toml` deliberately sets `rustc-wrapper = ""`
  # (so bare `cargo` works without sccache installed), so we must export
  # the env explicitly when the helper is available. Safe to skip when
  # sccache is missing — `setup_sccache_env` returns non-zero and the
  # `if` guard falls through.
  if [ -f "$TMPDIR_BUILD/scripts/_defaults.sh" ]; then
    # shellcheck disable=SC1091
    . "$TMPDIR_BUILD/scripts/_defaults.sh"
    if command -v setup_sccache_env >/dev/null 2>&1; then
      setup_sccache_env >/dev/null 2>&1 || true
    fi
  fi
  cargo build --release 2>&1 | tail -3

  # A source install must include the dashboard. Fail closed when the checked-in
  # Node/lockfile contract cannot be satisfied instead of reporting success
  # with no dashboard/dist.
  if [ -d "dashboard" ]; then
    info "Building dashboard..."
    bash scripts/check-dashboard-toolchain.sh "$PWD"
    bash scripts/install-dashboard-dependencies.sh "$PWD/dashboard"
    (cd dashboard && npm run build)
    if [ ! -f "dashboard/dist/index.html" ]; then
      fail "Dashboard build completed without dashboard/dist/index.html"
    fi
  fi

  install_runtime "$TMPDIR_BUILD" "$TMPDIR_BUILD/target/release/agentdesk"
  cd /
  ok "Built and installed from source"
else
  DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/${ARTIFACT}.tar.gz"
  info "Downloading $LATEST_TAG..."

  INSTALL_SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-install.XXXXXX")
  TMPDIR_DL="$INSTALL_SCRATCH"

  if ! curl -fSL "$DOWNLOAD_URL" -o "$TMPDIR_DL/${ARTIFACT}.tar.gz"; then
    fail "Download failed. URL: $DOWNLOAD_URL"
  fi

  info "Extracting..."
  cd "$TMPDIR_DL"
  tar xzf "${ARTIFACT}.tar.gz"

  install_runtime "$TMPDIR_DL/$ARTIFACT" "$TMPDIR_DL/$ARTIFACT/agentdesk"
  cd /
  ok "Installed $LATEST_TAG"
fi

# ── Code signing (macOS) ──────────────────────────────────────────────────────
if [ "$OS" = "darwin" ]; then
  # Register with firewall
  FW=/usr/libexec/ApplicationFirewall/socketfilterfw
  if [ -f "$FW" ]; then
    sudo "$FW" --add "$INSTALL_DIR/bin/agentdesk" 2>/dev/null || true
  fi
fi

# ── Create default config if not exists ───────────────────────────────────────
if [ ! -f "$CONFIG_PATH" ] && [ ! -f "$LEGACY_CONFIG_PATH" ]; then
  mkdir -p "$(dirname "$CONFIG_PATH")"
  cat > "$CONFIG_PATH" << YAML
# AgentDesk Configuration
# Edit this file to add Discord bot tokens and customize settings.
# Run the web onboarding wizard for guided setup: http://${DEFAULT_LOOPBACK}:${INSTALL_PORT}

server:
  port: ${INSTALL_PORT}
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
#   max_retries: 3
#   reset_overrides_on_restart: false
# automation:
#   strategy: "squash"
YAML
  ok "Created default config: $CONFIG_PATH"
elif [ -f "$CONFIG_PATH" ]; then
  ok "Using existing config: $CONFIG_PATH"
else
  ok "Using existing legacy config: $LEGACY_CONFIG_PATH"
fi

# ── Register launchd service ──────────────────────────────────────────────────
info "Setting up launchd service..."

PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST_PATH="$PLIST_DIR/$LAUNCHD_LABEL.plist"
mkdir -p "$PLIST_DIR"
emit_launchd_plist "$INSTALL_DIR/bin/agentdesk" "$PLIST_PATH"

ok "Launchd plist: $PLIST_PATH"

# ── Start dcserver ────────────────────────────────────────────────────────────
info "Starting AgentDesk..."
LAUNCHD_DOMAIN="$(launchd_domain)"
PREVIOUS_BOOT_INCARNATION=$(read_boot_incarnation 2>/dev/null || true)

# Stop existing instance if running
launchctl bootout "$LAUNCHD_DOMAIN/$LAUNCHD_LABEL" 2>/dev/null || true
sleep 1

# Remove quarantine flag if present
xattr -d com.apple.quarantine "$PLIST_PATH" 2>/dev/null || true

# Start
if launchctl bootstrap "$LAUNCHD_DOMAIN" "$PLIST_PATH" 2>/dev/null; then
  sleep 3

  # Health check
  if curl -sf --max-time 5 "http://${DEFAULT_LOOPBACK}:$INSTALL_PORT/api/health" | grep -q '"status":"healthy"'; then
    ok "AgentDesk is running on port $INSTALL_PORT"
  else
    fail "Service health check failed. Check logs: $INSTALL_DIR/logs/"
  fi
else
  fail "launchd bootstrap failed. Check logs: $INSTALL_DIR/logs/"
fi

CURRENT_BOOT_INCARNATION=$(read_boot_incarnation) || fail "Cannot verify the started AgentDesk process"
if [ "$CURRENT_BOOT_INCARNATION" = "$PREVIOUS_BOOT_INCARNATION" ]; then
  fail "AgentDesk process did not restart; refusing stale policy registration"
fi
verify_installed_policies || fail "Required policies failed to load. Check logs: $INSTALL_DIR/logs/"
ok "Required policies registered: auto-queue, timeouts, kanban-rules"
INSTALL_COMMITTED=1

# ── Open browser ──────────────────────────────────────────────────────────────
DASHBOARD_URL="http://${DEFAULT_LOOPBACK}:$INSTALL_PORT"

echo ""
echo -e "${BOLD}═══ Installation Complete ═══${NC}"
echo ""
echo -e "  Dashboard:  ${CYAN}$DASHBOARD_URL${NC}"
if [ -f "$CONFIG_PATH" ]; then
  DISPLAY_CONFIG_PATH="$CONFIG_PATH"
else
  DISPLAY_CONFIG_PATH="$LEGACY_CONFIG_PATH"
fi
echo -e "  Config:     $DISPLAY_CONFIG_PATH"
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
