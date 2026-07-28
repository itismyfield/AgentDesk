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

INSTALL_ROUTINE_ASSET_TXN=""
INSTALL_ROUTINE_ASSET_RUNTIME=""
INSTALL_BINARY_LIVE=""
INSTALL_BINARY_STAGE=""
INSTALL_BINARY_BACKUP=""
INSTALL_BINARY_NEW_SHA256=""
INSTALL_BINARY_OLD_SHA256=""
INSTALL_BINARY_HAD_LIVE=0
INSTALL_BINARY_SWAP_ARMED=0
INSTALL_BINARY_PROMOTED=0
INSTALL_COMMIT_INTENT=0
INSTALL_ASSET_FINALIZED=0

_install_sha256_file() {
  local path="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{ print $1 }'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{ print $1 }'
  else
    echo "A SHA-256 tool is required for atomic binary installation" >&2
    return 1
  fi
}

prepare_install_binary_transaction() {
  local source_binary="$1"
  local runtime_root="$2"
  local bin_dir="$runtime_root/bin"
  local stage=""
  local backup=""

  [ -f "$source_binary" ] && [ ! -L "$source_binary" ] \
    && [ -s "$source_binary" ] || {
      echo "Install binary payload is missing, empty, or symlinked: $source_binary" >&2
      return 1
    }
  [ ! -L "$runtime_root" ] && [ ! -L "$bin_dir" ] || {
    echo "Refusing symlinked install binary root: $bin_dir" >&2
    return 1
  }
  mkdir -p "$bin_dir" || return 1

  INSTALL_BINARY_LIVE="$bin_dir/agentdesk"
  [ ! -L "$INSTALL_BINARY_LIVE" ] || {
    echo "Refusing symlinked installed binary: $INSTALL_BINARY_LIVE" >&2
    return 1
  }
  if [ -e "$INSTALL_BINARY_LIVE" ] && [ ! -f "$INSTALL_BINARY_LIVE" ]; then
    echo "Installed binary path is not a regular file: $INSTALL_BINARY_LIVE" >&2
    return 1
  fi

  stage="$(mktemp "$bin_dir/.agentdesk.install.XXXXXXXX")" || return 1
  INSTALL_BINARY_STAGE="$stage"
  if ! cp "$source_binary" "$stage" \
    || ! chmod +x "$stage" \
    || [ ! -f "$stage" ] \
    || [ -L "$stage" ] \
    || [ ! -s "$stage" ] \
    || [ ! -x "$stage" ]; then
    echo "Could not stage a validated install binary" >&2
    return 1
  fi
  INSTALL_BINARY_NEW_SHA256="$(_install_sha256_file "$stage")" || return 1

  if [ -f "$INSTALL_BINARY_LIVE" ]; then
    backup="$(mktemp "$bin_dir/.agentdesk.rollback.XXXXXXXX")" || return 1
    INSTALL_BINARY_BACKUP="$backup"
    if ! cp -p "$INSTALL_BINARY_LIVE" "$backup" \
      || [ ! -f "$backup" ] \
      || [ -L "$backup" ] \
      || [ ! -s "$backup" ]; then
      echo "Could not preserve the installed binary for rollback" >&2
      return 1
    fi
    INSTALL_BINARY_OLD_SHA256="$(_install_sha256_file "$backup")" || return 1
    INSTALL_BINARY_HAD_LIVE=1
  else
    INSTALL_BINARY_HAD_LIVE=0
  fi
}

promote_install_binary_transaction() {
  local phase

  phase="$(adk_routine_asset_transaction_phase \
    "$INSTALL_ROUTINE_ASSET_RUNTIME" "$INSTALL_ROUTINE_ASSET_TXN")" || return 1
  [ "$phase" = "promoted" ] || {
    echo "Refusing binary promotion before matching assets are promoted" >&2
    return 1
  }
  [ -n "$INSTALL_BINARY_STAGE" ] \
    && [ -f "$INSTALL_BINARY_STAGE" ] \
    && [ ! -L "$INSTALL_BINARY_STAGE" ] \
    && [ -s "$INSTALL_BINARY_STAGE" ] \
    && [ -x "$INSTALL_BINARY_STAGE" ] \
    && [ ! -L "$INSTALL_BINARY_LIVE" ] || return 1

  # Arm before the rename. If TERM lands after mv replaced the live binary but
  # before the success assignment, cleanup infers promotion from the missing
  # stage plus the exact staged digest and rolls both binary and assets back.
  INSTALL_BINARY_SWAP_ARMED=1
  mv -f "$INSTALL_BINARY_STAGE" "$INSTALL_BINARY_LIVE" || return 1
  INSTALL_BINARY_PROMOTED=1
  INSTALL_BINARY_SWAP_ARMED=0
}

_install_binary_live_sha256() {
  [ -n "$INSTALL_BINARY_LIVE" ] \
    && [ -f "$INSTALL_BINARY_LIVE" ] \
    && [ ! -L "$INSTALL_BINARY_LIVE" ] || return 1
  _install_sha256_file "$INSTALL_BINARY_LIVE"
}

_install_binary_is_promoted() {
  local live_sha=""

  [ "$INSTALL_BINARY_PROMOTED" = 1 ] && return 0
  [ "$INSTALL_BINARY_SWAP_ARMED" = 1 ] \
    && [ -n "$INSTALL_BINARY_STAGE" ] \
    && [ ! -e "$INSTALL_BINARY_STAGE" ] \
    && live_sha="$(_install_binary_live_sha256)" \
    && [ "$live_sha" = "$INSTALL_BINARY_NEW_SHA256" ]
}

_restore_install_binary_transaction() {
  local restore_stage=""
  local live_sha=""

  _install_binary_is_promoted || return 0
  if [ "$INSTALL_BINARY_HAD_LIVE" = 1 ]; then
    [ -f "$INSTALL_BINARY_BACKUP" ] \
      && [ ! -L "$INSTALL_BINARY_BACKUP" ] \
      && [ ! -L "$INSTALL_BINARY_LIVE" ] || return 1
    restore_stage="$(mktemp "$(dirname "$INSTALL_BINARY_LIVE")/.agentdesk.restore.XXXXXXXX")" \
      || return 1
    if ! cp -p "$INSTALL_BINARY_BACKUP" "$restore_stage"; then
      rm -f "$restore_stage" 2>/dev/null || true
      return 1
    fi
    if ! mv -f "$restore_stage" "$INSTALL_BINARY_LIVE"; then
      # A signal/failure injector can report failure after the atomic rename.
      # Accept that boundary only when the live bytes prove restoration won.
      live_sha="$(_install_binary_live_sha256 2>/dev/null || true)"
      [ "$live_sha" = "$INSTALL_BINARY_OLD_SHA256" ] || return 1
    fi
  else
    rm -f "$INSTALL_BINARY_LIVE" || {
      [ ! -e "$INSTALL_BINARY_LIVE" ] || return 1
    }
  fi
  INSTALL_BINARY_PROMOTED=0
  INSTALL_BINARY_SWAP_ARMED=0
}

prepare_install_routine_asset_surfaces() {
  local source_root="$1"
  local runtime_root="$2"
  local primitives="$source_root/scripts/routine-asset-surface.sh"
  local validator="$source_root/scripts/validate-quickjs-routines.py"
  local lock_file="${AGENTDESK_DEPLOY_LOCK_FILE:-$runtime_root/runtime/deploy-release.lock}"
  local lock_timeout="${AGENTDESK_DEPLOY_LOCK_TIMEOUT_SECS:-1800}"
  local required_function

  if [ ! -f "$primitives" ] \
    || [ ! -f "$validator" ] \
    || [ ! -d "$source_root/routines" ] \
    || [ ! -d "$source_root/routine-helpers" ]; then
    echo "Routine asset payload is incomplete under $source_root" >&2
    return 1
  fi

  # The same preservation, exact-tombstone, and atomic-swap primitives power
  # deploy-release.sh, deploy.sh, source installs, and release-artifact installs.
  # Pin validation to this exact source/artifact instead of inheriting a path
  # from any previously sourced helper in the caller's shell.
  ADK_QUICKJS_VALIDATOR="$validator"
  # shellcheck disable=SC1090
  . "$primitives"

  for required_function in \
    adk_validate_repo_routine_assets \
    adk_acquire_routine_asset_lock \
    adk_begin_routine_asset_transaction \
    adk_stage_routines \
    adk_stage_routine_helpers \
    adk_promote_routine_asset_transaction \
    adk_routine_asset_transaction_phase \
    adk_mark_routine_asset_transaction_committing \
    adk_commit_routine_asset_transaction_forward \
    adk_commit_routine_asset_transaction \
    adk_rollback_routine_asset_transaction \
    adk_release_routine_asset_lock \
    _adk_active_txn; do
    command -v "$required_function" >/dev/null 2>&1 || {
      echo "Routine asset primitive is too old: $required_function missing" >&2
      return 1
    }
  done
  # This preflight is deliberately before the lock, transaction, or binary
  # copy. An old/incomplete artifact therefore leaves existing assets and the
  # installed binary byte-for-byte untouched.
  adk_validate_repo_routine_assets "$source_root" || return 1
  adk_acquire_routine_asset_lock "$lock_file" "$lock_timeout" || return 1
  INSTALL_ROUTINE_ASSET_RUNTIME="$runtime_root"
  INSTALL_ROUTINE_ASSET_TXN="$(
    adk_begin_routine_asset_transaction "$runtime_root" "$lock_file"
  )" || return 1
  adk_stage_routines "$source_root" "$runtime_root" \
    "$INSTALL_ROUTINE_ASSET_TXN" >/dev/null || return 1
  adk_stage_routine_helpers "$source_root" "$runtime_root" \
    "$INSTALL_ROUTINE_ASSET_TXN" >/dev/null || return 1
}

promote_install_routine_asset_surfaces() {
  adk_promote_routine_asset_transaction \
    "$INSTALL_ROUTINE_ASSET_RUNTIME" "$INSTALL_ROUTINE_ASSET_TXN"
}

finalize_install_routine_asset_surfaces() {
  # Commit intent is durable before the in-memory flag. Either side of a TERM
  # boundary therefore makes the same paired decision in cleanup.
  adk_mark_routine_asset_transaction_committing \
    "$INSTALL_ROUTINE_ASSET_RUNTIME" "$INSTALL_ROUTINE_ASSET_TXN" || return 1
  INSTALL_COMMIT_INTENT=1
  if ! adk_commit_routine_asset_transaction \
      "$INSTALL_ROUTINE_ASSET_RUNTIME" "$INSTALL_ROUTINE_ASSET_TXN"; then
    warn "Installed assets are live; durable commit cleanup will resume next run"
    return 0
  fi
  INSTALL_ASSET_FINALIZED=1
}

_install_cleanup() {
  local status=${1:-$?}
  local active_txn=""
  local active_status=1
  local active_phase=""
  local asset_action="none"
  local pair_resolved=0
  local live_sha=""

  trap - EXIT INT TERM
  if command -v _adk_active_txn >/dev/null 2>&1 \
    && [ -n "$INSTALL_ROUTINE_ASSET_RUNTIME" ]; then
    if active_txn="$(_adk_active_txn "$INSTALL_ROUTINE_ASSET_RUNTIME")"; then
      active_status=0
      if ! active_phase="$(adk_routine_asset_transaction_phase \
          "$INSTALL_ROUTINE_ASSET_RUNTIME" "$active_txn")"; then
        active_status=2
        active_txn=""
        echo "Install routine asset phase is corrupt; refusing partial recovery" >&2
        status=1
      fi
    else
      active_status=$?
      active_txn=""
      if [ "$active_status" -ne 1 ]; then
        echo "Install routine asset marker is corrupt; refusing partial recovery" >&2
        status=1
      fi
    fi
  fi

  if [ "$INSTALL_ASSET_FINALIZED" = 1 ]; then
    pair_resolved=1
  elif [ "$active_status" -eq 0 ]; then
    case "$active_phase" in
      committing|committed) asset_action="commit" ;;
      staging|armed|promoted|rolling-back)
        if _install_binary_is_promoted; then
          if _restore_install_binary_transaction; then
            asset_action="rollback"
          else
            live_sha="$(_install_binary_live_sha256 2>/dev/null || true)"
            if [ -n "$INSTALL_BINARY_OLD_SHA256" ] \
              && [ "$live_sha" = "$INSTALL_BINARY_OLD_SHA256" ]; then
              INSTALL_BINARY_PROMOTED=0
              INSTALL_BINARY_SWAP_ARMED=0
              asset_action="rollback"
            elif [ -n "$INSTALL_BINARY_NEW_SHA256" ] \
              && [ "$live_sha" = "$INSTALL_BINARY_NEW_SHA256" ]; then
              echo "Install binary rollback failed; committing matching new assets" >&2
              asset_action="commit"
              status=1
            else
              echo "Install binary state is unknown; preserving asset transaction" >&2
              status=1
            fi
          fi
        else
          asset_action="rollback"
        fi
        ;;
      *)
        echo "Install routine asset phase is invalid: $active_phase" >&2
        status=1
        ;;
    esac
  elif [ "$active_status" -eq 1 ]; then
    if [ "$INSTALL_COMMIT_INTENT" = 1 ]; then
      # A missing marker after durable commit intent means commit cleanup
      # already closed the exact transaction.
      pair_resolved=1
    elif _install_binary_is_promoted; then
      echo "Install asset transaction disappeared before commit intent; refusing binary-only recovery" >&2
      status=1
    else
      pair_resolved=1
    fi
  fi

  if [ -n "$active_txn" ] && [ "$asset_action" = "commit" ]; then
    if adk_commit_routine_asset_transaction_forward \
        "$INSTALL_ROUTINE_ASSET_RUNTIME" "$active_txn"; then
      pair_resolved=1
    else
      echo "Install asset fail-forward failed: $active_txn" >&2
      status=1
    fi
  elif [ -n "$active_txn" ] && [ "$asset_action" = "rollback" ]; then
    if adk_rollback_routine_asset_transaction \
        "$INSTALL_ROUTINE_ASSET_RUNTIME" "$active_txn"; then
      pair_resolved=1
    else
      echo "Install asset rollback failed: $active_txn" >&2
      status=1
    fi
  fi

  if [ "$pair_resolved" = 1 ]; then
    [ -z "$INSTALL_BINARY_STAGE" ] || [ ! -e "$INSTALL_BINARY_STAGE" ] \
      || rm -f "$INSTALL_BINARY_STAGE" \
      || { echo "Could not remove staged install binary" >&2; status=1; }
    [ -z "$INSTALL_BINARY_BACKUP" ] || [ ! -e "$INSTALL_BINARY_BACKUP" ] \
      || rm -f "$INSTALL_BINARY_BACKUP" \
      || { echo "Could not remove install binary rollback copy" >&2; status=1; }
  fi
  if command -v adk_release_routine_asset_lock >/dev/null 2>&1; then
    adk_release_routine_asset_lock \
      || { echo "Install could not release the shared deploy lock" >&2; status=1; }
  fi
  return "$status"
}

_install_cleanup_signal() {
  local status="$1"
  _install_cleanup "$status" || status=$?
  exit "$status"
}

trap _install_cleanup EXIT
trap '_install_cleanup_signal 130' INT
trap '_install_cleanup_signal 143' TERM

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

  TMPDIR_BUILD="${TMPDIR:-/tmp}/agentdesk-install-$$"
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

  # Build dashboard if npm available
  if command -v npm &>/dev/null && [ -d "dashboard" ]; then
    info "Building dashboard..."
    (cd dashboard && npm ci --silent 2>/dev/null && npm run build 2>&1 | tail -1) || true
  fi

  info "Validating and staging routine asset payload..."
  prepare_install_routine_asset_surfaces "$TMPDIR_BUILD" "$INSTALL_DIR" \
    || fail "Routine asset preflight failed before binary installation"
  prepare_install_binary_transaction \
    "$TMPDIR_BUILD/target/release/agentdesk" "$INSTALL_DIR" \
    || fail "Binary staging failed before live installation"

  # Install
  mkdir -p "$INSTALL_DIR"/{bin,config,data,logs,policies,dashboard,skills}

  if [ -d "dashboard/dist" ]; then
    cp -r dashboard/dist "$INSTALL_DIR/dashboard/dist"
  fi

  if [ -d "policies" ]; then
    cp policies/*.js "$INSTALL_DIR/policies/"
  fi

  if [ -d "skills" ]; then
    rsync -a --delete "skills/" "$INSTALL_DIR/skills/"
  fi

  info "Promoting routine entrypoints and helper assets..."
  promote_install_routine_asset_surfaces \
    || fail "Routine asset installation failed"
  promote_install_binary_transaction \
    || fail "Binary installation failed after asset promotion"

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

  info "Validating and staging routine asset payload..."
  prepare_install_routine_asset_surfaces "$TMPDIR_DL/${ARTIFACT}" "$INSTALL_DIR" \
    || fail "Routine asset preflight failed before binary installation"
  prepare_install_binary_transaction \
    "$TMPDIR_DL/${ARTIFACT}/agentdesk" "$INSTALL_DIR" \
    || fail "Binary staging failed before live installation"

  # Install
  mkdir -p "$INSTALL_DIR"/{bin,config,data,logs,skills}

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

  info "Promoting routine entrypoints and helper assets..."
  promote_install_routine_asset_surfaces \
    || fail "Routine asset installation failed"
  promote_install_binary_transaction \
    || fail "Binary installation failed after asset promotion"

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
#   dispatch_poll_sec: 30
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
    warn "Service started but health check pending. Check logs: $INSTALL_DIR/logs/"
  fi
else
  warn "launchd bootstrap failed. Try manually:"
  echo "  launchctl bootstrap $LAUNCHD_DOMAIN $PLIST_PATH"
fi

# The installer does not roll back its binary on a deferred launchd readiness
# warning, so commit the exactly matching asset generation before releasing the
# shared lock. A cleanup failure remains resumable via the `committing` marker.
finalize_install_routine_asset_surfaces

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
