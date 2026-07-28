#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# build-release.sh — Build AgentDesk release artifact for GitHub Releases
#
# Usage:
#   ./scripts/build-release.sh              # full build + package
#   ./scripts/build-release.sh --skip-dashboard
#
# Output:
#   dist/agentdesk-{os}-{arch}.tar.gz|zip  +  dist/checksums.txt
#   Contents: agentdesk / agentdesk.exe, dashboard/dist/, policies/, routines/,
#             routine-helpers/, skills/
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"
# shellcheck source=routine-asset-surface.sh
. "$SCRIPT_DIR/routine-asset-surface.sh"
cd "$PROJECT_DIR"

if ! adk_validate_repo_routine_assets "$PROJECT_DIR"; then
  echo "Error: routine asset preflight failed; refusing an incomplete artifact"
  exit 1
fi

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

write_local_build_generation_manifest() {
  local binary="$1"
  local expected_source_sha="$2"
  local expected_inputs_sha="$3"
  local expected_binary_sha="$4"
  local expected_routines_sha="$5"
  local expected_helpers_sha="$6"
  local manifest="$PROJECT_DIR/target/release/agentdesk-generation.json"
  local source_sha binary_sha routines_sha helpers_sha inputs_sha

  adk_require_clean_git_worktree "$PROJECT_DIR" || return 1
  source_sha="$(git -C "$PROJECT_DIR" rev-parse HEAD)" || return 1
  [ "$source_sha" = "$expected_source_sha" ] || {
    echo "Error: repository HEAD changed during the release build" >&2
    return 1
  }
  inputs_sha="$(adk_executable_input_digest "$PROJECT_DIR")" || return 1
  [ "$inputs_sha" = "$expected_inputs_sha" ] || {
    echo "Error: executable build inputs changed during the release build" >&2
    return 1
  }
  binary_sha="$(adk_sha256_file "$binary")" || return 1
  routines_sha="$(adk_sha256_tree "$PROJECT_DIR/routines")" || return 1
  helpers_sha="$(adk_sha256_tree "$PROJECT_DIR/routine-helpers")" || return 1
  [ "$binary_sha" = "$expected_binary_sha" ] \
    && [ "$routines_sha" = "$expected_routines_sha" ] \
    && [ "$helpers_sha" = "$expected_helpers_sha" ] || {
    echo "Error: binary/routine generation changed before manifest publication" >&2
    return 1
  }
  adk_require_clean_git_worktree "$PROJECT_DIR" || return 1
  [ "$(git -C "$PROJECT_DIR" rev-parse HEAD)" = "$expected_source_sha" ] || {
    echo "Error: repository HEAD changed while binding the release manifest" >&2
    return 1
  }
  AGENTDESK_BUILD_SOURCE_SHA="$source_sha" \
  AGENTDESK_BUILD_INPUTS_SHA="$inputs_sha" \
  AGENTDESK_BUILD_BINARY_SHA="$binary_sha" \
  AGENTDESK_BUILD_ROUTINES_SHA="$routines_sha" \
  AGENTDESK_BUILD_HELPERS_SHA="$helpers_sha" \
  python3 - "$manifest" <<'PY'
import json
import os
import tempfile
import sys

manifest = sys.argv[1]
payload = {
    "format": "agentdesk-local-build-v3",
    "worktree_state": "clean",
    "source_git_sha": os.environ["AGENTDESK_BUILD_SOURCE_SHA"],
    "executable_inputs_sha256": os.environ["AGENTDESK_BUILD_INPUTS_SHA"],
    "binary_sha256": os.environ["AGENTDESK_BUILD_BINARY_SHA"],
    "routines_sha256": os.environ["AGENTDESK_BUILD_ROUTINES_SHA"],
    "routine_helpers_sha256": os.environ["AGENTDESK_BUILD_HELPERS_SHA"],
}
fd, temporary = tempfile.mkstemp(
    prefix=".agentdesk-generation.", dir=os.path.dirname(manifest)
)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, sort_keys=True)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, manifest)
finally:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
PY
}

echo "═══ Building AgentDesk v${VERSION} for ${OS}/${ARCH} ═══"
echo ""

export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-10G}"
if setup_sccache_env; then
  echo "▸ sccache cache: ${SCCACHE_DIR} (size ${SCCACHE_CACHE_SIZE})"
else
  echo "⚠ sccache not found in PATH; continuing without rustc wrapper"
  echo "  Install it for faster release builds (for example: brew install sccache)"
  echo "  See docs/ci/sccache-setup.md"
  # Explicitly clear any rustc-wrapper coming from .cargo/config.toml so we
  # don't fail the build when the binary is missing.
  export RUSTC_WRAPPER=""
  export CARGO_BUILD_RUSTC_WRAPPER=""
fi

# ── 1. Build Rust binary ──────────────────────────────────────────────────────
if ! command -v cargo &>/dev/null; then
  echo "Error: cargo not found. Install Rust: https://rustup.rs/"
  exit 1
fi

echo "[1/3] Building Rust binary (release)..."
adk_require_clean_git_worktree "$PROJECT_DIR" \
  || { echo "Error: release builds require a clean worktree" >&2; exit 1; }
BUILD_SOURCE_SHA="$(git -C "$PROJECT_DIR" rev-parse HEAD)" \
  || { echo "Error: could not capture release source HEAD" >&2; exit 1; }
BUILD_EXECUTABLE_INPUT_SHA="$(adk_executable_input_digest "$PROJECT_DIR")" \
  || { echo "Error: could not capture executable build inputs" >&2; exit 1; }
cargo build --release 2>&1 | tail -1
if [ "$(adk_executable_input_digest "$PROJECT_DIR")" \
    != "$BUILD_EXECUTABLE_INPUT_SHA" ]; then
  echo "Error: executable inputs changed while cargo was building" >&2
  exit 1
fi

BINARY="$PROJECT_DIR/target/release/${BINARY_NAME}"
if [ ! -f "$BINARY" ]; then
  echo "Error: Binary not found at $BINARY"
  exit 1
fi
if ! "$BINARY" validate-routines \
    --root "$PROJECT_DIR/routines" \
    --runtime-root "$PROJECT_DIR"; then
  echo "Error: candidate runtime rejected repository routine assets"
  exit 1
fi
BUILD_BINARY_SHA="$(adk_sha256_file "$BINARY")" \
  || { echo "Error: could not bind release binary bytes" >&2; exit 1; }
BUILD_ROUTINES_SHA="$(adk_sha256_tree "$PROJECT_DIR/routines")" \
  || { echo "Error: could not bind release routines" >&2; exit 1; }
BUILD_HELPERS_SHA="$(adk_sha256_tree "$PROJECT_DIR/routine-helpers")" \
  || { echo "Error: could not bind release routine helpers" >&2; exit 1; }
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
adk_verify_file_sha256 "$BINARY" "$BUILD_BINARY_SHA" \
  || { echo "Error: binary changed before artifact copy" >&2; exit 1; }
cp "$BINARY" "$STAGING/"
chmod +x "$STAGING/$BINARY_NAME"
adk_verify_file_sha256 "$STAGING/$BINARY_NAME" "$BUILD_BINARY_SHA" \
  || { echo "Error: staged artifact binary differs from bound generation" >&2; exit 1; }

# Dashboard — rebuild to ensure dist matches current source
if [ -d "dashboard" ] && command -v npm &>/dev/null; then
  echo "▸ Building dashboard..."
  (cd dashboard && npm run build --silent)
fi
adk_verify_tree_sha256 "$PROJECT_DIR/routines" "$BUILD_ROUTINES_SHA" \
  || { echo "Error: routines changed during artifact staging" >&2; exit 1; }
adk_verify_tree_sha256 "$STAGING/routines" "$BUILD_ROUTINES_SHA" \
  || { echo "Error: staged routines differ from bound generation" >&2; exit 1; }
if [ -d "dashboard/dist" ]; then
  mkdir -p "$STAGING/dashboard"
  cp -r dashboard/dist "$STAGING/dashboard/dist"
fi
adk_verify_tree_sha256 "$PROJECT_DIR/routine-helpers" "$BUILD_HELPERS_SHA" \
  || { echo "Error: routine helpers changed during artifact staging" >&2; exit 1; }
adk_verify_tree_sha256 "$STAGING/routine-helpers" "$BUILD_HELPERS_SHA" \
  || { echo "Error: staged routine helpers differ from bound generation" >&2; exit 1; }

# Policies
if [ -d "policies" ]; then
  mkdir -p "$STAGING/policies"
  if command -v rsync &>/dev/null; then
    rsync -a --delete "policies/" "$STAGING/policies/"
  else
    cp -R "policies/." "$STAGING/policies/"
  fi
fi

# Routine scripts. Preflight above makes these required, not optional artifact
# extras that can disappear silently when a checkout is incomplete.
mkdir -p "$STAGING/routines"
if command -v rsync &>/dev/null; then
  rsync -a --delete --exclude='__pycache__/' --exclude='*.pyc' \
    "routines/" "$STAGING/routines/"
else
  cp -R "routines/." "$STAGING/routines/"
  adk_prune_python_bytecode_tree "$STAGING/routines"
fi

# Deterministic Node/Python helpers intentionally live outside the QuickJS
# routine loader root and are packaged as their own release asset surface.
mkdir -p "$STAGING/routine-helpers"
if command -v rsync &>/dev/null; then
  rsync -a --delete --exclude='__pycache__/' --exclude='*.pyc' \
    "routine-helpers/" "$STAGING/routine-helpers/"
else
  cp -R "routine-helpers/." "$STAGING/routine-helpers/"
  adk_prune_python_bytecode_tree "$STAGING/routine-helpers"
fi

# Shared staging primitives are required by install.sh when it consumes this
# extracted artifact rather than a full repository checkout.
mkdir -p "$STAGING/scripts"
cp "scripts/routine-asset-surface.sh" "$STAGING/scripts/routine-asset-surface.sh"
cp "scripts/validate-quickjs-routines.py" "$STAGING/scripts/validate-quickjs-routines.py"

# Launchd-migrated shell entrypoints used by bundled routine prompts.
if [ -d "scripts/launchd-migrated" ]; then
  mkdir -p "$STAGING/scripts/launchd-migrated"
  if command -v rsync &>/dev/null; then
    rsync -a --delete "scripts/launchd-migrated/" "$STAGING/scripts/launchd-migrated/"
  else
    cp -R "scripts/launchd-migrated/." "$STAGING/scripts/launchd-migrated/"
  fi
fi

# Root-level shell entrypoints referenced by bundled migrated routines.
if [ -f "scripts/queue-stability-batch.sh" ]; then
  mkdir -p "$STAGING/scripts"
  cp "scripts/_defaults.sh" "$STAGING/scripts/_defaults.sh"
  cp "scripts/queue-stability-batch.sh" "$STAGING/scripts/queue-stability-batch.sh"
  chmod +x "$STAGING/scripts/queue-stability-batch.sh"
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
adk_verify_file_sha256 "$STAGING/$BINARY_NAME" "$BUILD_BINARY_SHA" \
  && adk_verify_tree_sha256 "$STAGING/routines" "$BUILD_ROUTINES_SHA" \
  && adk_verify_tree_sha256 "$STAGING/routine-helpers" "$BUILD_HELPERS_SHA" \
  || { echo "Error: staged generation changed before archive creation" >&2; exit 1; }
cd "$DIST_DIR"
ARTIFACT_FILE="${ARTIFACT_NAME}.${PACKAGE_EXT}"
create_archive "$ARTIFACT_NAME" "$ARTIFACT_FILE"
rm -rf "$ARTIFACT_NAME"

# Checksum
write_checksum "$ARTIFACT_FILE"
write_local_build_generation_manifest \
  "$BINARY" "$BUILD_SOURCE_SHA" "$BUILD_EXECUTABLE_INPUT_SHA" \
  "$BUILD_BINARY_SHA" "$BUILD_ROUTINES_SHA" "$BUILD_HELPERS_SHA"

echo ""
echo "═══ Build Complete ═══"
echo "  Artifact: $DIST_DIR/${ARTIFACT_FILE}"
echo "  Checksum: $(cat checksums.txt)"
ls -lh "$DIST_DIR/${ARTIFACT_FILE}"
