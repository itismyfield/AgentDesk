#!/bin/bash
# ──────────────────────────────────────────────────────────────────────────────
# install.sh — AgentDesk installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/itismyfield/AgentDesk/main/scripts/install.sh | bash
#
# Or clone and run locally:
#   ./scripts/install.sh
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="itismyfield/AgentDesk"
INSTALL_DIR="${AGENTDESK_HOME:-$HOME/.agentdesk}"
BIN_DIR="$INSTALL_DIR/bin"

# ── Detect OS and arch ────────────────────────────────────────────────────────
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)       ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) echo "Error: Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "Installing AgentDesk for $OS/$ARCH..."
echo ""

# ── Create directories ────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR"
mkdir -p "$INSTALL_DIR/config"
mkdir -p "$INSTALL_DIR/logs"
mkdir -p "$INSTALL_DIR/policies"
mkdir -p "$INSTALL_DIR/role-context"
mkdir -p "$INSTALL_DIR/prompts"
mkdir -p "$INSTALL_DIR/skills"

# ── Build from source ─────────────────────────────────────────────────────────
# TODO: Download pre-built binary from GitHub Releases when available.
# For now, build from source using Rust toolchain.

if ! command -v cargo &>/dev/null; then
  echo "Error: Rust toolchain required. Install from https://rustup.rs/"
  echo ""
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  echo ""
  exit 1
fi

TMPDIR="${TMPDIR:-/tmp}"
BUILD_DIR="$TMPDIR/agentdesk-install-$$"

echo "Building from source..."
if [ -d "$BUILD_DIR" ]; then
  rm -rf "$BUILD_DIR"
fi

git clone --depth 1 "https://github.com/$REPO.git" "$BUILD_DIR"
cd "$BUILD_DIR"
cargo build --release

cp "target/release/agentdesk" "$BIN_DIR/"
echo "Binary installed: $BIN_DIR/agentdesk"

# ── Copy default policies ─────────────────────────────────────────────────────
if [ -d "policies/" ]; then
  for f in policies/*.js; do
    [ -f "$f" ] || continue
    dest="$INSTALL_DIR/policies/$(basename "$f")"
    if [ ! -f "$dest" ]; then
      cp "$f" "$dest"
    fi
  done
  echo "Policies copied to $INSTALL_DIR/policies/"
fi

# ── Copy example config ──────────────────────────────────────────────────────
if [ -f "agentdesk.example.yaml" ] && [ ! -f "$INSTALL_DIR/config/agentdesk.yaml" ]; then
  cp "agentdesk.example.yaml" "$INSTALL_DIR/config/agentdesk.yaml"
  echo "Example config: $INSTALL_DIR/config/agentdesk.yaml"
fi

# ── Cleanup ───────────────────────────────────────────────────────────────────
cd /
rm -rf "$BUILD_DIR"

# ── Build dashboard (optional) ────────────────────────────────────────────────
# Dashboard build is skipped during install — use build-release.sh for full builds.

# ── PATH hint ─────────────────────────────────────────────────────────────────
echo ""
echo "AgentDesk installed to $BIN_DIR/agentdesk"
echo ""

# Check if already in PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$BIN_DIR"; then
  SHELL_NAME=$(basename "${SHELL:-/bin/bash}")
  case "$SHELL_NAME" in
    zsh)  RC_FILE="$HOME/.zshrc" ;;
    bash) RC_FILE="$HOME/.bashrc" ;;
    fish) RC_FILE="$HOME/.config/fish/config.fish" ;;
    *)    RC_FILE="$HOME/.profile" ;;
  esac

  echo "Add to your PATH by adding this to $RC_FILE:"
  echo ""
  if [ "$SHELL_NAME" = "fish" ]; then
    echo "  fish_add_path $BIN_DIR"
  else
    echo "  export PATH=\"$BIN_DIR:\$PATH\""
  fi
  echo ""
fi

echo "Then run setup:"
echo "  agentdesk --init"
echo ""

# ── Migrate hint ──────────────────────────────────────────────────────────────
if [ -d "$HOME/.remotecc" ] || [ -d "$HOME/.local/state/pixel-claw-dashboard" ]; then
  echo "Legacy RCC/PCD data detected. To migrate:"
  echo "  agentdesk migrate  (or ./scripts/migrate-from-legacy.sh)"
  echo ""
fi

echo "Done."
