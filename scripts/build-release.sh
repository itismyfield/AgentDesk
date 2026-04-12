#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# build-release.sh — Build AgentDesk release artifact for GitHub Releases
# Official release-only build entrypoint for local deploy/promotion flows
#
# Usage:
#   ./scripts/build-release.sh              # full build + package
#   ./scripts/build-release.sh --skip-dashboard
#
# Output:
#   dist/agentdesk-{os}-{arch}.tar.gz|zip  +  dist/checksums.txt
#   Contents: agentdesk / agentdesk.exe, dashboard/dist/, policies/, skills/
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

RAW_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$RAW_OS" in
  darwin)
    OS="darwin"
    PACKAGE_EXT="tar.gz"
    BINARY_NAME="agentdesk"
    ;;
  linux)
    OS="linux"
    PACKAGE_EXT="tar.gz"
    BINARY_NAME="agentdesk"
    ;;
  msys*|mingw*|cygwin*)
    OS="windows"
    PACKAGE_EXT="zip"
    BINARY_NAME="agentdesk.exe"
    ;;
  *)
    echo "Error: Unsupported operating system: $RAW_OS"
    exit 1
    ;;
esac

ARCH=$(uname -m)
case "$ARCH" in
  x86_64)        ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) echo "Error: Unsupported architecture: $ARCH"; exit 1 ;;
esac

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
ARTIFACT_NAME="agentdesk-${OS}-${ARCH}"

create_archive() {
  local staging_name="$1"
  local artifact_name="$2"

  if [ "$OS" = "windows" ]; then
    if command -v zip &>/dev/null; then
      zip -rq "$artifact_name" "$staging_name"
    else
      echo "Error: zip is required to package Windows release artifacts"
      exit 1
    fi
  else
    tar czf "$artifact_name" "$staging_name"
  fi
}

write_checksum() {
  local artifact_name="$1"

  if command -v shasum &>/dev/null; then
    shasum -a 256 "$artifact_name" > checksums.txt
  elif command -v sha256sum &>/dev/null; then
    sha256sum "$artifact_name" > checksums.txt
  elif command -v certutil &>/dev/null; then
    local digest
    digest=$(certutil -hashfile "$artifact_name" SHA256 | sed -n '2p' | tr -d '\r')
    printf '%s  %s\n' "$digest" "$artifact_name" > checksums.txt
  else
    echo "Error: no SHA-256 checksum tool available"
    exit 1
  fi
}

echo "═══ Building AgentDesk v${VERSION} for ${OS}/${ARCH} ═══"
echo ""

# ── 1. Build Rust binary ──────────────────────────────────────────────────────
if ! command -v cargo &>/dev/null; then
  echo "Error: cargo not found. Install Rust: https://rustup.rs/"
  exit 1
fi

echo "[1/3] Building Rust binary (release)..."
cargo build --release 2>&1 | tail -1

BINARY="target/release/${BINARY_NAME}"
if [ ! -f "$BINARY" ]; then
  echo "Error: Binary not found at $BINARY"
  exit 1
fi
echo "  Binary: $(ls -lh "$BINARY" | awk '{print $5}')"

# ── 2. Verify + build dashboard ──────────────────────────────────────────────
if [ "$SKIP_DASHBOARD" = true ]; then
  echo "[2/3] Dashboard skipped (--skip-dashboard)"
else
  echo "[2/3] Verifying dashboard (install + build + test)..."
  if [ -d "dashboard" ] && [ -f "dashboard/package.json" ]; then
    "$PROJECT_DIR/scripts/verify-dashboard.sh"
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
chmod +x "$STAGING/$BINARY_NAME"

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
  if command -v rsync &>/dev/null; then
    rsync -a --delete "skills/" "$STAGING/skills/"
  else
    cp -R "skills/." "$STAGING/skills/"
  fi
fi

# Version marker
echo "$VERSION" > "$STAGING/VERSION"

# Create tarball
cd "$DIST_DIR"
ARTIFACT_FILE="${ARTIFACT_NAME}.${PACKAGE_EXT}"
create_archive "$ARTIFACT_NAME" "$ARTIFACT_FILE"
rm -rf "$ARTIFACT_NAME"

# Checksum
write_checksum "$ARTIFACT_FILE"

echo ""
echo "═══ Build Complete ═══"
echo "  Artifact: $DIST_DIR/${ARTIFACT_FILE}"
echo "  Checksum: $(cat checksums.txt)"
ls -lh "$DIST_DIR/${ARTIFACT_FILE}"
