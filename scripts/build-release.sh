#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# build-release.sh — Build AgentDesk release artifact for GitHub Releases
#
# Usage:
#   ./scripts/build-release.sh              # full build + package
#   ./scripts/build-release.sh --skip-dashboard
#
# Output:
#   dist/agentdesk-{os}-{arch}.tar.gz  +  dist/checksums.txt
#   Contents: agentdesk (binary), dashboard/dist/, policies/, skills/
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

SKIP_DASHBOARD=false
for arg in "$@"; do
  case "$arg" in
    --skip-dashboard) SKIP_DASHBOARD=true ;;
  esac
done

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)        ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) echo "Error: Unsupported architecture: $ARCH"; exit 1 ;;
esac

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
ARTIFACT_NAME="agentdesk-${OS}-${ARCH}"

echo "═══ Building AgentDesk v${VERSION} for ${OS}/${ARCH} ═══"
echo ""

# ── 1. Build Rust binary ──────────────────────────────────────────────────────
if ! command -v cargo &>/dev/null; then
  echo "Error: cargo not found. Install Rust: https://rustup.rs/"
  exit 1
fi

echo "[1/3] Building Rust binary (release)..."
cargo build --release 2>&1 | tail -1

BINARY="target/release/agentdesk"
if [ ! -f "$BINARY" ]; then
  echo "Error: Binary not found at $BINARY"
  exit 1
fi
echo "  Binary: $(ls -lh "$BINARY" | awk '{print $5}')"

# ── 2. Build dashboard ───────────────────────────────────────────────────────
if [ "$SKIP_DASHBOARD" = true ]; then
  echo "[2/3] Dashboard skipped (--skip-dashboard)"
else
  echo "[2/3] Building dashboard..."
  if [ -d "dashboard" ] && [ -f "dashboard/package.json" ]; then
    cd dashboard
    if command -v pnpm &>/dev/null; then
      pnpm install --frozen-lockfile 2>/dev/null || pnpm install
      pnpm build
    elif command -v npm &>/dev/null; then
      npm ci --silent 2>/dev/null || npm install --silent
      npm run build
    else
      echo "  Error: No package manager (npm or pnpm)"
      exit 1
    fi
    cd "$PROJECT_DIR"
    echo "  Dashboard: $(du -sh dashboard/dist/ | cut -f1)"
  else
    echo "  [SKIP] No dashboard directory"
  fi
fi

# ── 3. Package artifact ──────────────────────────────────────────────────────
echo "[3/3] Packaging artifact..."

DIST_DIR="$PROJECT_DIR/dist"
STAGING="$DIST_DIR/$ARTIFACT_NAME"
rm -rf "$STAGING"
mkdir -p "$STAGING"

# Binary
cp "$BINARY" "$STAGING/"
chmod +x "$STAGING/agentdesk"

# Dashboard
if [ -d "dashboard/dist" ]; then
  mkdir -p "$STAGING/dashboard"
  cp -r dashboard/dist "$STAGING/dashboard/dist"
fi

# Policies
if [ -d "policies" ]; then
  mkdir -p "$STAGING/policies"
  cp policies/*.js "$STAGING/policies/"
fi

# Managed skills
if [ -d "skills" ]; then
  mkdir -p "$STAGING/skills"
  rsync -a --delete "skills/" "$STAGING/skills/"
fi

# Version marker
echo "$VERSION" > "$STAGING/VERSION"

# Create tarball
cd "$DIST_DIR"
tar czf "${ARTIFACT_NAME}.tar.gz" "$ARTIFACT_NAME"
rm -rf "$ARTIFACT_NAME"

# Checksum
shasum -a 256 "${ARTIFACT_NAME}.tar.gz" > checksums.txt

echo ""
echo "═══ Build Complete ═══"
echo "  Artifact: $DIST_DIR/${ARTIFACT_NAME}.tar.gz"
echo "  Checksum: $(cat checksums.txt)"
ls -lh "$DIST_DIR/${ARTIFACT_NAME}.tar.gz"
