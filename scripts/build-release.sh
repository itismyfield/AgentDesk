#!/usr/bin/env bash
# Build the common native hub/runner artifact. Python >= 3.11 is required.
# Usage: build-release.sh [--target <rust-target>]
#        [--profile release|release-fast]
#        [--skip-dashboard | --prebuilt-dashboard]
# --skip-dashboard excludes UI assets; --prebuilt-dashboard packages an already
# verified dashboard/dist. The default verifies/builds the dashboard once.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"
cd "$PROJECT_DIR"

DASHBOARD_MODE=build
TARGET=""
BUILD_PROFILE=release
PYTHON="${AGENTDESK_PYTHON:-python3}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --skip-dashboard|--prebuilt-dashboard)
      if [ "$DASHBOARD_MODE" != build ]; then
        echo "Error: choose exactly one dashboard mode" >&2
        exit 2
      fi
      DASHBOARD_MODE="$1"
      shift
      ;;
    --target)
      if [ "$#" -lt 2 ] || [ -n "$TARGET" ]; then
        echo "Error: --target requires one Rust target" >&2
        exit 2
      fi
      TARGET="$2"
      shift 2
      ;;
    --profile)
      if [ "$#" -lt 2 ] || { [ "$2" != release ] && [ "$2" != release-fast ]; }; then
        echo "Error: --profile requires release or release-fast" >&2
        exit 2
      fi
      BUILD_PROFILE="$2"
      shift 2
      ;;
    *) echo "Error: unknown argument: $1" >&2; exit 2 ;;
  esac
done

"$PYTHON" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else "Python >= 3.11 is required")'
command -v cargo >/dev/null || { echo "Error: cargo is required" >&2; exit 1; }
# SQLx embeds byte-exact migration checksums. Older Windows checkouts may still
# contain CRLF after .gitattributes changed; reject them before compilation.
"$PYTHON" "$SCRIPT_DIR/check_postgres_migration_checksums.py"
HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}"
CARGO_TARGET_ARGS=()
if [ -n "$TARGET" ]; then
  CARGO_TARGET_ARGS+=(--target "$TARGET")
  BINARY_DIR="$TARGET_DIR/$TARGET/$BUILD_PROFILE"
else
  TARGET="$HOST_TARGET"
  BINARY_DIR="$TARGET_DIR/$BUILD_PROFILE"
fi
case "$TARGET" in
  *-pc-windows-msvc) BINARY_NAME=agentdesk.exe ;;
  *-apple-darwin|*-unknown-linux-gnu) BINARY_NAME=agentdesk ;;
  *) echo "Error: unsupported release target: $TARGET" >&2; exit 2 ;;
esac

export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-40G}"
if ! setup_sccache_env; then
  export RUSTC_WRAPPER=""
  export CARGO_BUILD_RUSTC_WRAPPER=""
fi

echo "[1/3] Building common AgentDesk binary for $TARGET ($BUILD_PROFILE)"
# Keep the shared build-token contract and its separate contention diagnostics.
ADK_BUILD_TOKEN_DIAG_FD=3 "$PYTHON" "$SCRIPT_DIR/build_token.py" -- \
  cargo build --locked --profile "$BUILD_PROFILE" --bin agentdesk "${CARGO_TARGET_ARGS[@]}" 3>&2

echo "[2/3] Dashboard ($DASHBOARD_MODE)"
case "$DASHBOARD_MODE" in
  build) bash "$SCRIPT_DIR/verify-dashboard.sh" ;;
  --prebuilt-dashboard) test -f dashboard/dist/index.html || { echo "Missing prebuilt dashboard" >&2; exit 1; } ;;
esac

echo "[3/3] Packaging binary and common runtime assets"
# package_release.py owns policies, routines, managed skills and entrypoints
# (scripts/queue-stability-batch.sh and scripts/_defaults.sh), plus checksums.
PACKAGE_ARGS=(--binary "$BINARY_DIR/$BINARY_NAME" --target "$TARGET" --profile "$BUILD_PROFILE")
if [ "$DASHBOARD_MODE" = --skip-dashboard ]; then
  PACKAGE_ARGS+=(--without-dashboard)
fi
"$PYTHON" "$SCRIPT_DIR/package_release.py" "${PACKAGE_ARGS[@]}"
