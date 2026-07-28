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
#   ./scripts/deploy.sh [--skip-dashboard] [--skip-build] \
#     [--codesign-mode=auto|developer-id|adhoc|skip] \
#     [--codesign-identity="Developer ID Application: ..."]
#   If no codesign identity is provided, the first available Developer ID
#   identity will be used automatically when needed.
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=_defaults.sh
. "$SCRIPT_DIR/_defaults.sh"
# shellcheck source=routine-asset-surface.sh
. "$SCRIPT_DIR/routine-asset-surface.sh"

AD_HOME="${AGENTDESK_HOME:-$HOME/.adk/release}"
BIN_DIR="$AD_HOME/bin"
LIBEXEC_DIR="$AD_HOME/libexec"
WRAPPER_BIN="$BIN_DIR/agentdesk"
REAL_BIN="$LIBEXEC_DIR/agentdesk"
HEALTH_PORT="${AGENTDESK_SERVER_PORT:-$ADK_DEFAULT_PORT}"
LABEL="com.agentdesk.release"
OS=$(uname -s | tr '[:upper:]' '[:lower:]')

SKIP_BUILD=false
SKIP_DASHBOARD=false
CODESIGN_MODE="${AGENTDESK_CODESIGN_MODE:-auto}"
CODESIGN_IDENTITY="${AGENTDESK_CODESIGN_IDENTITY:-}"
CODESIGN_IDENTIFIER="${AGENTDESK_CODESIGN_IDENTIFIER:-com.itismyfield.agentdesk}"
RAW_CODESIGN_MODE="$CODESIGN_MODE"
RESOLVED_CODESIGN_MODE=""
RESOLVED_CODESIGN_IDENTITY=""

for arg in "$@"; do
  case "$arg" in
    --skip-build)     SKIP_BUILD=true ;;
    --skip-dashboard) SKIP_DASHBOARD=true ;;
    --codesign-mode=*) CODESIGN_MODE="${arg#*=}"; RAW_CODESIGN_MODE="$CODESIGN_MODE" ;;
    --codesign-identity=*) CODESIGN_IDENTITY="${arg#*=}" ;;
  esac
done

info()  { printf "\033[1;34m[deploy]\033[0m %s\n" "$*"; }
ok()    { printf "\033[1;32m[deploy]\033[0m %s\n" "$*"; }
error() { printf "\033[1;31m[deploy]\033[0m %s\n" "$*" >&2; }
fail()  { error "$*"; exit 1; }

adk_validate_repo_routine_assets "$PROJECT_DIR" \
  || fail "Routine asset preflight failed; refusing an incomplete deploy"

normalize_codesign_mode() {
  local raw_mode="${1:-}"
  raw_mode="$(printf '%s' "$raw_mode" | tr '[:upper:]' '[:lower:]')"
  case "$raw_mode" in
    auto|"")
      printf 'auto\n'
      ;;
    developer-id|developer_id|developerid|developer)
      printf 'developer-id\n'
      ;;
    adhoc|ad-hoc|ad_hoc)
      printf 'adhoc\n'
      ;;
    skip|none|preserve|existing)
      printf 'skip\n'
      ;;
    *)
      return 1
      ;;
  esac
}

codesign_identity_available() {
  local identity="$1"
  if [ "$OS" != "darwin" ] || [ -z "$identity" ]; then
    return 1
  fi

  security find-identity -v -p codesigning 2>/dev/null | grep -F -- "$identity" >/dev/null
}

find_first_developer_id_identity() {
  local identity
  if [ "$OS" != "darwin" ]; then
    return 1
  fi

  identity="$(
    security find-identity -v -p codesigning 2>/dev/null |
      sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' |
      head -n 1
  )"

  [ -n "$identity" ] || return 1
  printf '%s\n' "$identity"
}

resolve_developer_id_identity() {
  if [ "$OS" != "darwin" ] || [ "$CODESIGN_IDENTITY" = "-" ]; then
    return 1
  fi

  if [ -n "$CODESIGN_IDENTITY" ]; then
    codesign_identity_available "$CODESIGN_IDENTITY" || return 1
    printf '%s\n' "$CODESIGN_IDENTITY"
    return 0
  fi

  find_first_developer_id_identity
}

resolve_macos_codesign_mode() {
  RESOLVED_CODESIGN_MODE=""
  RESOLVED_CODESIGN_IDENTITY=""
  case "$CODESIGN_MODE" in
    developer-id)
      RESOLVED_CODESIGN_IDENTITY="$(resolve_developer_id_identity)" || return 1
      RESOLVED_CODESIGN_MODE="developer-id"
      ;;
    adhoc|skip)
      RESOLVED_CODESIGN_MODE="$CODESIGN_MODE"
      ;;
    auto)
      if [ "$CODESIGN_IDENTITY" = "-" ]; then
        RESOLVED_CODESIGN_MODE="adhoc"
      elif [ -n "$CODESIGN_IDENTITY" ]; then
        RESOLVED_CODESIGN_IDENTITY="$(resolve_developer_id_identity)" || return 1
        RESOLVED_CODESIGN_MODE="developer-id"
      elif RESOLVED_CODESIGN_IDENTITY="$(resolve_developer_id_identity 2>/dev/null)"; then
        RESOLVED_CODESIGN_MODE="developer-id"
      else
        RESOLVED_CODESIGN_MODE="adhoc"
      fi
      ;;
    *)
      return 1
      ;;
  esac
}

binary_has_valid_codesign() {
  local path="$1"
  if [ "$OS" != "darwin" ] || [ ! -f "$path" ]; then
    return 1
  fi

  codesign -v "$path" >/dev/null 2>&1
}

detect_binary_signature_mode() {
  local path="$1" info
  if [ "$OS" != "darwin" ] || [ ! -f "$path" ]; then
    printf 'unsigned\n'
    return 0
  fi

  if ! info="$(codesign -dv --verbose=4 "$path" 2>&1)"; then
    printf 'unsigned\n'
    return 0
  fi

  if printf '%s\n' "$info" | grep -F 'Signature=adhoc' >/dev/null; then
    printf 'adhoc\n'
  elif printf '%s\n' "$info" | grep -F 'Authority=Developer ID Application:' >/dev/null; then
    printf 'developer-id\n'
  elif binary_has_valid_codesign "$path"; then
    printf 'signed\n'
  else
    printf 'unsigned\n'
  fi
}

codesign_binary() {
  local mode="$1"
  local target="$2"
  if [ "$OS" != "darwin" ]; then
    return 0
  fi

  case "$mode" in
    developer-id)
      [ -n "$RESOLVED_CODESIGN_IDENTITY" ] || {
        error "Developer ID signing requested but no usable identity was resolved"
        return 1
      }
      codesign_identity_available "$RESOLVED_CODESIGN_IDENTITY" \
        || {
          error "Developer ID identity not found in keychain: $RESOLVED_CODESIGN_IDENTITY"
          return 1
        }
      info "Signing $target with Developer ID identity: $RESOLVED_CODESIGN_IDENTITY"
      codesign \
        -s "$RESOLVED_CODESIGN_IDENTITY" \
        --options runtime \
        --identifier "$CODESIGN_IDENTIFIER" \
        --force \
        "$target" || {
          error "Developer ID codesign failed — aborting"
          return 1
        }
      codesign -v "$target" 2>/dev/null \
        || {
          error "Developer ID codesign verification failed — aborting"
          return 1
        }
      ;;
    adhoc)
      info "Signing $target with ad-hoc identity"
      codesign \
        -s - \
        --identifier "$CODESIGN_IDENTIFIER" \
        --force \
        "$target" || {
          error "Ad-hoc codesign failed — aborting"
          return 1
        }
      codesign -v "$target" 2>/dev/null \
        || {
          error "Ad-hoc codesign verification failed — aborting"
          return 1
        }
      ;;
    *)
      error "Unsupported codesign mode: $mode"
      return 1
      ;;
  esac
}

preserve_previous_signature_state_if_needed() {
  local previous_binary="$1"
  local target="${2:-$REAL_BIN}"
  local previous_mode

  if [ "$OS" != "darwin" ]; then
    return 0
  fi

  if binary_has_valid_codesign "$target"; then
    info "Copied binary already has a valid code signature; leaving it unchanged"
    return 0
  fi

  if [ -z "$previous_binary" ] || [ ! -f "$previous_binary" ]; then
    info "No previous signature state found; leaving $target unsigned"
    return 0
  fi

  previous_mode="$(detect_binary_signature_mode "$previous_binary")"
  case "$previous_mode" in
    adhoc)
      info "Previous install used ad-hoc signing; preserving that mode"
      codesign_binary adhoc "$target"
      ;;
    developer-id)
      if RESOLVED_CODESIGN_IDENTITY="$(resolve_developer_id_identity 2>/dev/null)"; then
        info "Previous install used Developer ID signing; preserving that mode"
        codesign_binary developer-id "$target"
      else
        error "Previous install used Developer ID signing, but no usable Developer ID identity is available to preserve it. Provide --codesign-identity or use --codesign-mode=adhoc."
        return 1
      fi
      ;;
    signed)
      error "Previous install used a non-standard code signature that cannot be preserved automatically. Use an explicit --codesign-mode."
      return 1
      ;;
    unsigned)
      info "Previous install was unsigned; leaving $target unsigned"
      ;;
  esac
}

codesign_real_binary_if_needed() {
  local resolved_mode="$1"
  local target="${2:-$REAL_BIN}"

  if [ "$OS" != "darwin" ]; then
    return 0
  fi

  case "$resolved_mode" in
    developer-id)
      codesign_binary developer-id "$target"
      ;;
    adhoc)
      codesign_binary adhoc "$target"
      ;;
    skip)
      preserve_previous_signature_state_if_needed "${REAL_BIN:-}" "$target"
      ;;
    *)
      error "Unsupported resolved codesign mode: $resolved_mode"
      return 1
      ;;
  esac
}

if ! CODESIGN_MODE="$(normalize_codesign_mode "$CODESIGN_MODE")"; then
  fail "Unsupported --codesign-mode: $RAW_CODESIGN_MODE"
fi

if [ "$CODESIGN_IDENTITY" = "-" ] && [ "$CODESIGN_MODE" = "developer-id" ]; then
  fail "Developer ID mode cannot use '-' identity; use --codesign-mode=adhoc instead"
fi

BACKUP_WRAPPER=""
BACKUP_REAL=""
DEPLOY_BINARY_STAGE=""
ROUTINE_ASSET_TXN=""
DEPLOY_BINARY_PROMOTED=0
DEPLOY_RESTART_ARMED=0
DEPLOY_SERVICE_STOP_ATTEMPTED=0
DEPLOY_SERVICE_START_ATTEMPTED=0
DEPLOY_SERVICE_START_CONFIRMED=0
DEPLOY_SERVICE_STOP_CONFIRMED=0
DEPLOY_SERVICE_WAS_RUNNING=0
DEPLOY_SERVICE_OLD_PID=""
DEPLOY_SERVICE_OLD_IDENTITY=""
DEPLOY_SERVICE_CANDIDATE_PID=""
DEPLOY_SERVICE_CANDIDATE_IDENTITY=""
DEPLOY_WRAPPER_OLD_IMMUTABLE=0
DEPLOY_REAL_OLD_IMMUTABLE=0
DEPLOY_IMMUTABLE_SNAPSHOT_TAKEN=0
DEPLOY_IMMUTABLE_MUTATION_ARMED=0
DEPLOY_HEALTH_OK=0
DEPLOY_LOCK_HELD=0
DEPLOY_LOCK_FILE="${AGENTDESK_DEPLOY_LOCK_FILE:-$AD_HOME/runtime/deploy-release.lock}"
DEPLOY_LOCK_TIMEOUT_SECS="${AGENTDESK_DEPLOY_LOCK_TIMEOUT_SECS:-1800}"

cleanup_backup() {
  if [ -n "${DEPLOY_BINARY_STAGE:-}" ] && [ -f "$DEPLOY_BINARY_STAGE" ]; then
    adk_durable_remove_path "$DEPLOY_BINARY_STAGE" missing-ok \
      deploy-candidate-stage
  fi
  if [ -n "${BACKUP_WRAPPER:-}" ] && [ -f "$BACKUP_WRAPPER" ]; then
    adk_durable_remove_path "$BACKUP_WRAPPER" missing-ok \
      deploy-wrapper-backup
  fi
  if [ -n "${BACKUP_REAL:-}" ] && [ -f "$BACKUP_REAL" ]; then
    adk_durable_remove_path "$BACKUP_REAL" missing-ok deploy-real-backup
  fi
}

validate_local_build_generation_manifest() {
  local binary="$PROJECT_DIR/target/release/agentdesk"
  local manifest="$PROJECT_DIR/target/release/agentdesk-generation.json"
  local source_sha binary_sha routines_sha helpers_sha inputs_sha

  [ -f "$manifest" ] && [ ! -L "$manifest" ] \
    && [ -f "$binary" ] && [ ! -L "$binary" ] || {
    error "Local build generation manifest is missing or unsafe: $manifest"
    return 1
  }
  adk_require_clean_git_worktree "$PROJECT_DIR" || {
    error "Local build generation cannot be reused from a dirty worktree"
    return 1
  }
  source_sha="$(git -C "$PROJECT_DIR" rev-parse HEAD 2>/dev/null)" || return 1
  inputs_sha="$(adk_executable_input_digest "$PROJECT_DIR")" || return 1
  binary_sha="$(adk_sha256_file "$binary")" || return 1
  routines_sha="$(adk_sha256_tree "$PROJECT_DIR/routines")" || return 1
  helpers_sha="$(adk_sha256_tree "$PROJECT_DIR/routine-helpers")" || return 1
  AGENTDESK_BUILD_SOURCE_SHA="$source_sha" \
  AGENTDESK_BUILD_INPUTS_SHA="$inputs_sha" \
  AGENTDESK_BUILD_BINARY_SHA="$binary_sha" \
  AGENTDESK_BUILD_ROUTINES_SHA="$routines_sha" \
  AGENTDESK_BUILD_HELPERS_SHA="$helpers_sha" \
  python3 - "$manifest" <<'PY' || return 1
import json
import os
import re
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception as exc:
    print(f"Invalid local build generation manifest: {exc}", file=sys.stderr)
    raise SystemExit(1)
expected = {
    "source_git_sha": os.environ["AGENTDESK_BUILD_SOURCE_SHA"],
    "executable_inputs_sha256": os.environ["AGENTDESK_BUILD_INPUTS_SHA"],
    "binary_sha256": os.environ["AGENTDESK_BUILD_BINARY_SHA"],
    "routines_sha256": os.environ["AGENTDESK_BUILD_ROUTINES_SHA"],
    "routine_helpers_sha256": os.environ["AGENTDESK_BUILD_HELPERS_SHA"],
}
if data.get("format") != "agentdesk-local-build-v3":
    raise SystemExit(1)
if data.get("worktree_state") != "clean":
    raise SystemExit(1)
for key, value in expected.items():
    recorded = data.get(key)
    if not isinstance(recorded, str) or not re.fullmatch(r"[0-9a-f]+", recorded):
        raise SystemExit(1)
    if recorded != value:
        print(f"Local build generation mismatch: {key}", file=sys.stderr)
        raise SystemExit(1)
PY
  DEPLOY_EXPECTED_SOURCE_SHA="$source_sha"
  DEPLOY_EXPECTED_INPUTS_SHA="$inputs_sha"
  DEPLOY_EXPECTED_BINARY_SHA="$binary_sha"
  DEPLOY_EXPECTED_ROUTINES_SHA="$routines_sha"
  DEPLOY_EXPECTED_HELPERS_SHA="$helpers_sha"
}

_deploy_immutable_flag_state() {
  local path="$1"
  local flags

  [ "$OS" = darwin ] || { printf '0\n'; return 0; }
  [ -e "$path" ] || { printf '0\n'; return 0; }
  flags="$(stat -f '%Sf' "$path" 2>/dev/null)" || return 1
  case ",$flags," in
    *,uchg,*|*,uimmutable,*) printf '1\n' ;;
    *) printf '0\n' ;;
  esac
}

_deploy_apply_immutable_flag_state() {
  local path="$1"
  local expected="$2"
  local actual

  [ "$OS" = darwin ] || return 0
  [ -e "$path" ] || return 1
  case "$expected" in
    0) chflags nouchg "$path" || return 1 ;;
    1) chflags uchg "$path" || return 1 ;;
    *) return 1 ;;
  esac
  actual="$(_deploy_immutable_flag_state "$path")" || return 1
  [ "$actual" = "$expected" ]
}

clear_deploy_immutable_flag() {
  local path="$1"

  [ "$OS" = darwin ] || return 0
  [ -e "$path" ] || return 0
  _deploy_apply_immutable_flag_state "$path" 0
}

snapshot_deploy_immutable_flags() {
  [ "$OS" = darwin ] || { DEPLOY_IMMUTABLE_SNAPSHOT_TAKEN=1; return 0; }
  DEPLOY_WRAPPER_OLD_IMMUTABLE="$(_deploy_immutable_flag_state "$WRAPPER_BIN")" \
    || return 1
  DEPLOY_REAL_OLD_IMMUTABLE="$(_deploy_immutable_flag_state "$REAL_BIN")" \
    || return 1
  DEPLOY_IMMUTABLE_SNAPSHOT_TAKEN=1
}

restore_deploy_snapshot_immutable_flags() {
  [ "$OS" = darwin ] || { DEPLOY_IMMUTABLE_MUTATION_ARMED=0; return 0; }
  [ "$DEPLOY_IMMUTABLE_SNAPSHOT_TAKEN" = 1 ] || return 1
  if [ -e "$WRAPPER_BIN" ]; then
    _deploy_apply_immutable_flag_state \
      "$WRAPPER_BIN" "$DEPLOY_WRAPPER_OLD_IMMUTABLE" || return 1
  fi
  if [ -e "$REAL_BIN" ]; then
    _deploy_apply_immutable_flag_state \
      "$REAL_BIN" "$DEPLOY_REAL_OLD_IMMUTABLE" || return 1
  fi
  DEPLOY_IMMUTABLE_MUTATION_ARMED=0
}

apply_deploy_committed_immutable_flags() {
  [ "$OS" = darwin ] || return 0
  [ "$DEPLOY_IMMUTABLE_SNAPSHOT_TAKEN" = 1 ] || return 1
  _deploy_apply_immutable_flag_state \
    "$WRAPPER_BIN" "$DEPLOY_WRAPPER_OLD_IMMUTABLE" || return 1
  _deploy_apply_immutable_flag_state "$REAL_BIN" 1
}

finalize_healthy_deploy_generation() {
  apply_deploy_committed_immutable_flags || return 1
  adk_mark_routine_asset_transaction_committing "$AD_HOME" "$ROUTINE_ASSET_TXN" \
    || return 1
  DEPLOY_HEALTH_OK=1
  DEPLOY_IMMUTABLE_MUTATION_ARMED=0
  if ! adk_commit_routine_asset_transaction "$AD_HOME" "$ROUTINE_ASSET_TXN"; then
    error "Healthy deploy retained a committing routine asset transaction"
  fi
}

_deploy_service_job_is_running() {
  case "$OS" in
    darwin)
      launchctl print "$(_launchd_domain)/$LABEL" >/dev/null 2>&1
      ;;
    linux)
      systemctl --user is-active --quiet agentdesk-dcserver.service \
        >/dev/null 2>&1
      ;;
    *)
      return 1
      ;;
  esac
}

_deploy_current_service_pid() {
  case "$OS" in
    darwin)
      launchctl print "$(_launchd_domain)/$LABEL" 2>/dev/null \
        | awk '$1 == "pid" && $2 == "=" { print $3; exit }'
      ;;
    linux)
      systemctl --user show agentdesk-dcserver.service \
        --property MainPID --value 2>/dev/null
      ;;
  esac
}

_capture_deploy_service_process() {
  DEPLOY_SERVICE_OLD_PID="$(_deploy_current_service_pid 2>/dev/null || true)"
  DEPLOY_SERVICE_OLD_IDENTITY="$(
    adk_process_identity "$DEPLOY_SERVICE_OLD_PID" 2>/dev/null || true
  )"
}

_capture_deploy_candidate_process() {
  local pid identity

  pid="$(_deploy_current_service_pid 2>/dev/null)" || return 1
  case "$pid" in
    ''|*[!0-9]*|0) return 1 ;;
  esac
  identity="$(adk_process_identity "$pid" 2>/dev/null)" || return 1
  [ -n "$identity" ] || return 1
  DEPLOY_SERVICE_CANDIDATE_PID="$pid"
  DEPLOY_SERVICE_CANDIDATE_IDENTITY="$identity"
}

_persist_deploy_candidate_drain_authority() {
  local pid="${1:-}"
  local identity="${2:-}"
  local supervisor

  case "$OS" in
    darwin) supervisor="$(_launchd_domain)/$LABEL" ;;
    linux) supervisor='systemd-user/agentdesk-dcserver.service' ;;
    *) return 1 ;;
  esac
  adk_persist_routine_asset_candidate_drain_authority \
    "$AD_HOME" "$ROUTINE_ASSET_TXN" deploy "$pid" "$identity" \
    "$HEALTH_PORT" "$supervisor"
}

capture_deploy_candidate_process_after_start() {
  local attempt=0
  local max_attempts="${1:-15}"

  until _capture_deploy_candidate_process; do
    [ "$attempt" -lt "$max_attempts" ] || return 1
    sleep 1
    attempt=$((attempt + 1))
  done
  _persist_deploy_candidate_drain_authority \
    "$DEPLOY_SERVICE_CANDIDATE_PID" "$DEPLOY_SERVICE_CANDIDATE_IDENTITY"
}

_deploy_candidate_process_is_alive() {
  adk_process_instance_alive \
    "${DEPLOY_SERVICE_CANDIDATE_PID:-}" \
    "${DEPLOY_SERVICE_CANDIDATE_IDENTITY:-}"
}

_deploy_candidate_port_refuses_connections() {
  local curl_status

  if curl -sS --connect-timeout 1 --max-time 1 -o /dev/null \
      "http://${ADK_DEFAULT_LOOPBACK:-127.0.0.1}:${HEALTH_PORT}/" \
      >/dev/null 2>&1; then
    return 1
  else
    curl_status=$?
  fi
  [ "$curl_status" -eq 7 ]
}

_deploy_candidate_drain_is_proven() {
  ! _deploy_service_job_is_running \
    && ! _deploy_candidate_process_is_alive \
    && _deploy_candidate_port_refuses_connections
}

wait_for_deploy_candidate_stop() {
  local attempt=0
  local max_attempts="${1:-15}"

  until _deploy_candidate_drain_is_proven; do
    [ "$attempt" -lt "$max_attempts" ] || return 1
    sleep 1
    attempt=$((attempt + 1))
  done
}

_force_stop_deploy_candidate() {
  local attempt=0

  if _deploy_candidate_process_is_alive; then
    kill -TERM "$DEPLOY_SERVICE_CANDIDATE_PID" 2>/dev/null || return 1
    while _deploy_candidate_process_is_alive && [ "$attempt" -lt 5 ]; do
      sleep 1
      attempt=$((attempt + 1))
    done
  fi
  if _deploy_candidate_process_is_alive; then
    kill -KILL "$DEPLOY_SERVICE_CANDIDATE_PID" 2>/dev/null || return 1
  fi
  wait_for_deploy_candidate_stop 5
}

deploy_service_is_running() {
  _deploy_service_job_is_running && return 0
  adk_process_instance_alive \
    "${DEPLOY_SERVICE_OLD_PID:-}" "${DEPLOY_SERVICE_OLD_IDENTITY:-}"
}

wait_for_deploy_service_stop() {
  local attempt=0
  local max_attempts="${1:-15}"

  while deploy_service_is_running; do
    [ "$attempt" -lt "$max_attempts" ] || return 1
    sleep 1
    attempt=$((attempt + 1))
  done
}

stop_deploy_service_for_promotion() {
  DEPLOY_RESTART_ARMED=1
  case "$OS" in
    darwin)
      if _deploy_service_job_is_running; then
        DEPLOY_SERVICE_WAS_RUNNING=1
        _capture_deploy_service_process
      fi
      DEPLOY_SERVICE_STOP_ATTEMPTED=1
      launchctl bootout "$(_launchd_domain)/$LABEL" 2>/dev/null || true
      if ! wait_for_deploy_service_stop; then
        error "Existing launchd process is still loaded; refusing live promotion"
        return 1
      fi
      DEPLOY_SERVICE_STOP_CONFIRMED=1
      ;;
    linux)
      if _deploy_service_job_is_running; then
        DEPLOY_SERVICE_WAS_RUNNING=1
        _capture_deploy_service_process
      fi
      DEPLOY_SERVICE_STOP_ATTEMPTED=1
      systemctl --user stop agentdesk-dcserver.service || return 1
      if ! wait_for_deploy_service_stop; then
        error "Existing systemd process is still active; refusing live promotion"
        return 1
      fi
      DEPLOY_SERVICE_STOP_CONFIRMED=1
      ;;
  esac
}

start_previous_deploy_service() {
  local plist="$HOME/Library/LaunchAgents/com.agentdesk.release.plist"

  DEPLOY_SERVICE_START_ATTEMPTED=1
  case "$OS" in
    darwin)
      [ -f "$plist" ] || return 1
      launchctl bootstrap "$(_launchd_domain)" "$plist" >/dev/null 2>&1 \
        || return 1
      _kickstart_launchd_job_if_needed "$LABEL" || true
      launchctl print "$(_launchd_domain)/$LABEL" >/dev/null 2>&1 \
        || return 1
      ;;
    linux)
      systemctl --user start agentdesk-dcserver.service || return 1
      systemctl --user is-active --quiet agentdesk-dcserver.service \
        >/dev/null 2>&1 || return 1
      ;;
    *) return 0 ;;
  esac
  DEPLOY_SERVICE_START_CONFIRMED=1
}

stop_deploy_service_for_rollback() {
  local candidate_was_loaded=0
  local candidate_capture_failed=0

  if _deploy_service_job_is_running; then
    candidate_was_loaded=1
    if [ -z "${DEPLOY_SERVICE_CANDIDATE_PID:-}" ] \
      || [ -z "${DEPLOY_SERVICE_CANDIDATE_IDENTITY:-}" ]; then
      _capture_deploy_candidate_process || candidate_capture_failed=1
    fi
  fi
  case "$OS" in
    darwin)
      launchctl bootout "$(_launchd_domain)/$LABEL" 2>/dev/null || true
      if launchctl print "$(_launchd_domain)/$LABEL" >/dev/null 2>&1; then
        error "New launchd process is still loaded; refusing in-place binary rollback"
        return 1
      fi
      ;;
    linux)
      systemctl --user stop agentdesk-dcserver.service 2>/dev/null || true
      if systemctl --user is-active --quiet agentdesk-dcserver.service \
        >/dev/null 2>&1; then
        error "New systemd process is still active; refusing in-place binary rollback"
        return 1
      fi
      ;;
  esac
  if [ -n "${DEPLOY_SERVICE_CANDIDATE_PID:-}" ] \
    && [ -n "${DEPLOY_SERVICE_CANDIDATE_IDENTITY:-}" ]; then
    if ! wait_for_deploy_candidate_stop \
      && ! _force_stop_deploy_candidate; then
      error "New service process did not drain; refusing in-place binary rollback"
      return 1
    fi
  elif [ "$candidate_was_loaded" = 1 ] \
    || [ "$candidate_capture_failed" = 1 ] \
    || [ "$DEPLOY_SERVICE_START_ATTEMPTED" = 1 ] \
    || [ "$DEPLOY_SERVICE_START_CONFIRMED" = 1 ]; then
    error "New service identity is unknown; refusing in-place binary rollback"
    return 1
  fi
  if ! _deploy_candidate_drain_is_proven; then
    error "New service supervisor/process/port drain is not proven; refusing rollback"
    return 1
  fi
  if [ -n "${ROUTINE_ASSET_TXN:-}" ] \
    && adk_routine_asset_candidate_drain_authority_exists "$ROUTINE_ASSET_TXN" \
    && ! adk_clear_routine_asset_candidate_drain_authority \
      "$AD_HOME" "$ROUTINE_ASSET_TXN"; then
    error "Candidate drained, but its durable drain authority could not be cleared"
    return 1
  fi
}

cleanup_deploy_transaction() {
  local status=${1:-$?}
  local active_txn=""
  local active_status=1
  local active_phase=""
  local restore_ok=1
  local assets_ok=1
  local asset_action="rollback"
  local service_stopped=0
  local lock_owned=0
  local pair_resolved="${DEPLOY_HEALTH_OK:-0}"
  local candidate_drain_guard=0

  trap - EXIT INT TERM
  if [ "$DEPLOY_LOCK_HELD" = 1 ] \
    && adk_routine_asset_lock_owned "$DEPLOY_LOCK_FILE"; then
    lock_owned=1
  fi
  if [ "$DEPLOY_HEALTH_OK" != 1 ] && [ "$lock_owned" = 1 ]; then
    if active_txn="$(_adk_active_txn "$AD_HOME")"; then
      active_status=0
      if ! active_phase="$(
        adk_routine_asset_transaction_phase "$AD_HOME" "$active_txn"
      )"; then
        active_status=2
        active_txn=""
        status=1
        assets_ok=0
        asset_action="none"
        error "Routine asset transaction phase is corrupt; refusing binary-only rollback"
      fi
    else
      active_status=$?
      active_txn=""
      if [ "$active_status" -ne 1 ]; then
        status=1
        assets_ok=0
        asset_action="none"
        error "Routine asset transaction marker is corrupt; refusing binary-only rollback"
      fi
    fi
    if [ -n "$active_txn" ] \
      && adk_routine_asset_candidate_drain_authority_exists "$active_txn" \
      && [ "${DEPLOY_SERVICE_START_ATTEMPTED:-0}" != 1 ]; then
      candidate_drain_guard=1
      asset_action="none"
      restore_ok=0
      status=1
      error "Exact candidate drain is unresolved; retaining binary/assets and rollback material"
    fi
    if [ "$active_status" -eq 1 ] \
      && [ "${DEPLOY_BINARY_PROMOTED:-0}" != 1 ]; then
      pair_resolved=1
    fi
    if [ "${DEPLOY_BINARY_PROMOTED:-0}" != 1 ] \
      && [ "${DEPLOY_IMMUTABLE_MUTATION_ARMED:-0}" = 1 ] \
      && ! restore_deploy_snapshot_immutable_flags; then
      restore_ok=0
      status=1
      error "Previous immutable flags could not be restored; leaving the service stopped"
    fi
    if [ "$candidate_drain_guard" = 1 ]; then
      :
    elif [ "$active_phase" = "committing" ] || [ "$active_phase" = "committed" ]; then
      asset_action="commit"
    elif [ "$DEPLOY_BINARY_PROMOTED" = 1 ] && [ "$active_status" -le 1 ]; then
      if [ "$DEPLOY_RESTART_ARMED" = 1 ] \
        || [ "$DEPLOY_SERVICE_STOP_ATTEMPTED" = 1 ] \
        || [ "$DEPLOY_SERVICE_START_ATTEMPTED" = 1 ] \
        || [ "$DEPLOY_SERVICE_START_CONFIRMED" = 1 ]; then
        if stop_deploy_service_for_rollback; then
          service_stopped=1
        else
          restore_ok=0
          asset_action="none"
          status=1
          error "Service could not be stopped; retaining the uncommitted new generation and rollback material"
        fi
      fi
      if [ "$restore_ok" = 1 ] && ! restore_previous_install; then
        restore_ok=0
        asset_action="none"
        status=1
        error "Binary rollback failed; retaining the transaction and rollback material"
      fi
    fi
    if [ -n "$active_txn" ] && [ "$asset_action" != "none" ]; then
      if [ "$asset_action" = "commit" ]; then
        if adk_commit_routine_asset_transaction_forward "$AD_HOME" "$active_txn"; then
          pair_resolved=1
        else
          error "Routine asset fail-forward failed: $active_txn"
          assets_ok=0
          status=1
        fi
      else
        if adk_rollback_routine_asset_transaction "$AD_HOME" "$active_txn"; then
          pair_resolved=1
        else
          error "Routine asset rollback failed: $active_txn"
          assets_ok=0
          status=1
        fi
      fi
    fi
    if [ "$DEPLOY_SERVICE_STOP_CONFIRMED" = 1 ]; then
      service_stopped=1
    elif [ "$DEPLOY_SERVICE_STOP_ATTEMPTED" = 1 ] \
      && [ "$DEPLOY_SERVICE_WAS_RUNNING" = 1 ] \
      && wait_for_deploy_service_stop; then
      # A signal can be delivered after bootout/stop returns but before the
      # caller records STOP_CONFIRMED. Reconcile the durable service state in
      # the trap so that exact boundary cannot strand the previous release.
      DEPLOY_SERVICE_STOP_CONFIRMED=1
      service_stopped=1
    fi
    if [ "$service_stopped" = 1 ] \
      && [ "$DEPLOY_SERVICE_WAS_RUNNING" = 1 ] \
      && [ "$asset_action" = "rollback" ] \
      && [ "$pair_resolved" = 1 ]; then
      if [ "$restore_ok" = 1 ] && [ "$assets_ok" = 1 ]; then
        start_previous_deploy_service >/dev/null 2>&1 || status=1
      fi
    fi
  fi
  if [ "$lock_owned" = 1 ]; then
    adk_release_routine_asset_lock \
      || { error "Failed to release shared routine asset deploy lock"; status=1; }
    DEPLOY_LOCK_HELD=0
  fi
  if [ "$pair_resolved" = 1 ] || [ "${DEPLOY_BINARY_PROMOTED:-0}" != 1 ]; then
    cleanup_backup
  elif [ -n "${DEPLOY_BINARY_STAGE:-}" ] && [ -f "$DEPLOY_BINARY_STAGE" ]; then
    rm -f "$DEPLOY_BINARY_STAGE" \
      || { error "Failed to remove abandoned binary stage"; status=1; }
  fi
  return "$status"
}

_deploy_cleanup_signal() {
  local status="$1"
  cleanup_deploy_transaction "$status" || status=$?
  exit "$status"
}

trap 'cleanup_deploy_transaction "$?"' EXIT
trap '_deploy_cleanup_signal 130' INT
trap '_deploy_cleanup_signal 143' TERM

adk_acquire_routine_asset_lock "$DEPLOY_LOCK_FILE" "$DEPLOY_LOCK_TIMEOUT_SECS" \
  || fail "Could not acquire shared routine asset deploy lock"
adk_routine_asset_lock_owned "$DEPLOY_LOCK_FILE" \
  || fail "Shared routine asset deploy lock ownership verification failed"
DEPLOY_LOCK_HELD=1

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
  local tmp_wrapper
  tmp_wrapper="$(mktemp "$WRAPPER_BIN.new.XXXXXX")"
  cat > "$tmp_wrapper" <<EOF
#!/bin/bash
exec "$REAL_BIN" "\$@"
EOF
  chmod +x "$tmp_wrapper"
  adk_durable_rename_path "$tmp_wrapper" "$WRAPPER_BIN" wrapper-publish
}

install_file_atomically() {
  local src="$1"
  local dest="$2"
  local mode="${3:-755}"
  local tmp_dest

  tmp_dest="$(mktemp "$dest.new.XXXXXX")"
  cp "$src" "$tmp_dest"
  chmod "$mode" "$tmp_dest"
  adk_durable_rename_path "$tmp_dest" "$dest" install-file-publish
}

restore_previous_install() {
  clear_deploy_immutable_flag "$WRAPPER_BIN" || return 1
  clear_deploy_immutable_flag "$REAL_BIN" || return 1

  if [ -n "${BACKUP_WRAPPER:-}" ] && [ -f "$BACKUP_WRAPPER" ]; then
    install_file_atomically "$BACKUP_WRAPPER" "$WRAPPER_BIN" 755 || return 1
  else
    adk_durable_remove_path "$WRAPPER_BIN" missing-ok wrapper-rollback || return 1
  fi

  if [ -n "${BACKUP_REAL:-}" ] && [ -f "$BACKUP_REAL" ]; then
    install_file_atomically "$BACKUP_REAL" "$REAL_BIN" 755 || return 1
  else
    adk_durable_remove_path "$REAL_BIN" missing-ok real-binary-rollback || return 1
  fi

  restore_deploy_snapshot_immutable_flags
}

restore_previous_install_and_fail() {
  local message="$1"
  local restore_message="${2:-Restored previous install after failed binary update}"
  local restore_status

  set +e
  restore_previous_install
  restore_status=$?
  set -e

  if [ "$restore_status" -eq 0 ]; then
    ok "$restore_message"
    fail "$message"
  fi

  fail "$message (previous install restore also failed)"
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

  restore_previous_install_and_fail \
    "Installed binary self-check failed for $WRAPPER_BIN" \
    "Restored previous install after failed self-check"
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

validate_local_build_generation_manifest \
  || fail "Local binary is not bound to the current source/assets generation"

# Validate and stage both asset surfaces before the binary is touched. Stages
# live under this transaction's unique directory, so no concurrent entrypoint
# can promote a payload prepared by another process.
ROUTINE_ASSET_TXN="$(
  adk_begin_routine_asset_transaction "$AD_HOME" "$DEPLOY_LOCK_FILE"
)" || fail "Could not begin durable routine asset transaction"
adk_verify_tree_sha256 "$PROJECT_DIR/routines" "$DEPLOY_EXPECTED_ROUTINES_SHA" \
  || fail "Routines changed after build-manifest validation"
adk_stage_routines "$PROJECT_DIR" "$AD_HOME" "$ROUTINE_ASSET_TXN" >/dev/null \
  || fail "Routine staging failed; refusing to install the binary"
adk_verify_tree_sha256 "$PROJECT_DIR/routines" "$DEPLOY_EXPECTED_ROUTINES_SHA" \
  && adk_verify_staged_tree_projection \
    "$PROJECT_DIR/routines" \
    "$ROUTINE_ASSET_TXN/staged/release-root/routines" \
    "$DEPLOY_EXPECTED_ROUTINES_SHA" \
  || fail "Staged routines differ from the manifest-bound generation"
adk_verify_tree_sha256 \
  "$PROJECT_DIR/routine-helpers" "$DEPLOY_EXPECTED_HELPERS_SHA" \
  || fail "Routine helpers changed after build-manifest validation"
adk_stage_routine_helpers "$PROJECT_DIR" "$AD_HOME" "$ROUTINE_ASSET_TXN" >/dev/null \
  || fail "Routine helper staging failed; refusing to install the binary"
adk_verify_tree_sha256 \
  "$PROJECT_DIR/routine-helpers" "$DEPLOY_EXPECTED_HELPERS_SHA" \
  && adk_verify_staged_tree_projection \
    "$PROJECT_DIR/routine-helpers" \
    "$ROUTINE_ASSET_TXN/staged/release-root/routine-helpers" \
    "$DEPLOY_EXPECTED_HELPERS_SHA" \
  || fail "Staged routine helpers differ from the manifest-bound generation"

# Prepare the exact bytes that will become live, including their final
# signature, before any live binary or asset path changes. The candidate CLI
# evaluates the transaction's staged release-root without config/DB input.
info "Staging candidate binary..."
mkdir -p "$LIBEXEC_DIR"
DEPLOY_BINARY_STAGE="$(mktemp "$LIBEXEC_DIR/.agentdesk.deploy.XXXXXXXX")" \
  || fail "Could not create private binary stage"
if ! adk_verify_file_sha256 \
    "$PROJECT_DIR/target/release/agentdesk" "$DEPLOY_EXPECTED_BINARY_SHA" \
  || ! cp "$PROJECT_DIR/target/release/agentdesk" "$DEPLOY_BINARY_STAGE" \
  || ! chmod 755 "$DEPLOY_BINARY_STAGE" \
  || [ ! -f "$DEPLOY_BINARY_STAGE" ] \
  || [ -L "$DEPLOY_BINARY_STAGE" ] \
  || [ ! -s "$DEPLOY_BINARY_STAGE" ] \
  || [ ! -x "$DEPLOY_BINARY_STAGE" ]; then
  fail "Could not stage the candidate binary"
fi
adk_verify_file_sha256 "$DEPLOY_BINARY_STAGE" "$DEPLOY_EXPECTED_BINARY_SHA" \
  && adk_verify_file_sha256 \
    "$PROJECT_DIR/target/release/agentdesk" "$DEPLOY_EXPECTED_BINARY_SHA" \
  || fail "Candidate copy differs from the manifest-bound binary"
if [ "$OS" = "darwin" ]; then
  resolve_macos_codesign_mode \
    || fail "Could not resolve macOS codesign mode from: $CODESIGN_MODE"
  info "Resolved macOS codesign mode: $RESOLVED_CODESIGN_MODE"
  if [ "$RESOLVED_CODESIGN_MODE" = "developer-id" ] \
    && [ -n "$RESOLVED_CODESIGN_IDENTITY" ]; then
    info "Resolved Developer ID identity: $RESOLVED_CODESIGN_IDENTITY"
  fi
  codesign_real_binary_if_needed "$RESOLVED_CODESIGN_MODE" "$DEPLOY_BINARY_STAGE" \
    || fail "Failed to sign the staged candidate using mode: $RESOLVED_CODESIGN_MODE"
fi
DEPLOY_SIGNED_CANDIDATE_SHA="$(adk_sha256_file "$DEPLOY_BINARY_STAGE")" \
  || fail "Could not bind the signed candidate bytes"
adk_validate_staged_routine_asset_transaction \
  "$AD_HOME" "$ROUTINE_ASSET_TXN" "$DEPLOY_BINARY_STAGE" \
  || fail "Candidate runtime rejected the staged routine generation"

# No live binary or asset path may change while the old process can still load
# from it. Arm recovery before the first control-plane command so a signal at
# either boundary restores and restarts the previous generation.
stop_deploy_service_for_promotion \
  || fail "Could not stop the existing service before live promotion"

# Exact validation is repeated under the same transaction lock at the live
# asset boundary, then the fully validated asset pair is promoted while the old
# service is confirmed quiescent. The staged binary remains private until both
# surfaces are in place.
adk_verify_file_sha256 "$DEPLOY_BINARY_STAGE" "$DEPLOY_SIGNED_CANDIDATE_SHA" \
  && adk_verify_staged_tree_projection \
    "$PROJECT_DIR/routines" \
    "$ROUTINE_ASSET_TXN/staged/release-root/routines" \
    "$DEPLOY_EXPECTED_ROUTINES_SHA" \
  && adk_verify_staged_tree_projection \
    "$PROJECT_DIR/routine-helpers" \
    "$ROUTINE_ASSET_TXN/staged/release-root/routine-helpers" \
    "$DEPLOY_EXPECTED_HELPERS_SHA" \
  || fail "Private candidate generation changed before live promotion"
adk_promote_routine_asset_transaction \
  "$AD_HOME" "$ROUTINE_ASSET_TXN" "$DEPLOY_BINARY_STAGE" \
  || fail "Routine asset transaction promotion failed"
ok "Routines: $AD_HOME/routines/"
ok "Routine helpers: $AD_HOME/routine-helpers/"

# ── Step 2: Copy binary ──────────────────────────────────────────────────────
info "Installing binary..."
mkdir -p "$BIN_DIR"
if [ "$OS" = "darwin" ]; then
  # Snapshot both old paths before the first flag mutation. The generated
  # wrapper preserves its prior policy; the committed real binary is protected.
  snapshot_deploy_immutable_flags \
    || fail "Could not snapshot immutable flags from the existing install"
  DEPLOY_IMMUTABLE_MUTATION_ARMED=1
  clear_deploy_immutable_flag "$WRAPPER_BIN" \
    || fail "Could not clear immutable flag from existing wrapper"
  clear_deploy_immutable_flag "$REAL_BIN" \
    || fail "Could not clear immutable flag from existing binary"
fi
if [ -e "$WRAPPER_BIN" ]; then
  BACKUP_WRAPPER="$(mktemp "$BIN_DIR/agentdesk.wrapper.backup.XXXXXX")"
  cp "$WRAPPER_BIN" "$BACKUP_WRAPPER"
fi
if [ -e "$REAL_BIN" ]; then
  BACKUP_REAL="$(mktemp "$LIBEXEC_DIR/agentdesk.real.backup.XXXXXX")"
  cp "$REAL_BIN" "$BACKUP_REAL"
fi
DEPLOY_BINARY_PROMOTED=1
adk_durable_rename_path "$DEPLOY_BINARY_STAGE" "$REAL_BIN" \
  candidate-binary-to-live \
  || fail "Could not durably publish the candidate binary"
DEPLOY_BINARY_STAGE=""
adk_verify_file_sha256 "$REAL_BIN" "$DEPLOY_SIGNED_CANDIDATE_SHA" \
  || restore_previous_install_and_fail \
    "Published binary differs from the signed candidate generation"
write_wrapper_script \
  || restore_previous_install_and_fail \
    "Failed to update binary wrapper at $WRAPPER_BIN" \
    "Restored previous install after failed wrapper update"
ok "Binary wrapper: $WRAPPER_BIN -> $REAL_BIN"
run_installed_binary_self_check
adk_durable_remove_path "$BIN_DIR/agentdesk-real" missing-ok \
  legacy-real-binary

# Build and copy dashboard dist
if [ -d "$PROJECT_DIR/dashboard" ]; then
  echo "▸ Building dashboard..."
  (cd "$PROJECT_DIR/dashboard" && npm run build --silent)
fi
if [ -d "$PROJECT_DIR/dashboard/dist" ]; then
  mkdir -p "$AD_HOME/dashboard"
  rsync -a --delete "$PROJECT_DIR/dashboard/dist/" "$AD_HOME/dashboard/dist/"
  ok "Dashboard: $AD_HOME/dashboard/dist/"
fi

if [ -d "$PROJECT_DIR/policies" ]; then
  mkdir -p "$AD_HOME/policies"
  rsync -a --delete "$PROJECT_DIR/policies/" "$AD_HOME/policies/"
  ok "Policies: $AD_HOME/policies/"
fi

if [ -d "$PROJECT_DIR/skills" ]; then
  mkdir -p "$AD_HOME/skills"
  rsync -a --delete "$PROJECT_DIR/skills/" "$AD_HOME/skills/"
  ok "Managed skills: $AD_HOME/skills/"
fi

# ── Step 3: Install/update service ────────────────────────────────────────────
install_launchd() {
  local PLIST_DST="$HOME/Library/LaunchAgents/com.agentdesk.release.plist"

  # Migrate: remove legacy com.agentdesk plist if present
  local LEGACY_PLIST="$HOME/Library/LaunchAgents/com.agentdesk.plist"
  if [ -f "$LEGACY_PLIST" ]; then
    launchctl bootout "$(_launchd_domain)/com.agentdesk" 2>/dev/null || true
    rm -f "$LEGACY_PLIST"
    info "Removed legacy plist: $LEGACY_PLIST"
  fi

  mkdir -p "$HOME/Library/LaunchAgents"
  mkdir -p "$AD_HOME/logs"

  "$BIN_DIR/agentdesk" emit-launchd-plist \
    --flavor release \
    --home "$HOME" \
    --root-dir "$AD_HOME" \
    --agentdesk-bin "$BIN_DIR/agentdesk" \
    --output "$PLIST_DST"

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
  local attempt max_attempts=5
  if [ ! -f "$PLIST" ]; then
    info "Plist not installed — skipping restart"
    return
  fi

  # Arm both boundaries before each control-plane command. Cleanup therefore
  # stops any possibly-started new process even if launchctl performed the
  # side effect and TERM/failure landed before the next assignment.
  DEPLOY_SERVICE_STOP_ATTEMPTED=1
  launchctl bootout "$(_launchd_domain)/$LABEL" 2>/dev/null || true
  sleep 1

  # Load with retry because launchd can briefly report
  # "operation already in progress" immediately after bootout.
  for attempt in $(seq 1 "$max_attempts"); do
    DEPLOY_SERVICE_START_ATTEMPTED=1
    _persist_deploy_candidate_drain_authority \
      || { error "Could not arm durable candidate drain authority"; return 1; }
    if launchctl bootstrap "$(_launchd_domain)" "$PLIST" >/dev/null 2>&1; then
      _kickstart_launchd_job_if_needed "$LABEL" || true
      capture_deploy_candidate_process_after_start \
        || { error "Could not capture the bootstrapped launchd process identity"; return 1; }
      DEPLOY_SERVICE_START_CONFIRMED=1
      ok "Service restarted via launchd"
      return
    fi

    info "  launchd bootstrap attempt $attempt/$max_attempts failed — retrying"
    sleep 1
  done

  # Surface the real launchctl error on the final attempt.
  DEPLOY_SERVICE_START_ATTEMPTED=1
  _persist_deploy_candidate_drain_authority \
    || { error "Could not arm durable candidate drain authority"; return 1; }
  launchctl bootstrap "$(_launchd_domain)" "$PLIST" || return $?
  _kickstart_launchd_job_if_needed "$LABEL" || true
  capture_deploy_candidate_process_after_start \
    || { error "Could not capture the bootstrapped launchd process identity"; return 1; }
  DEPLOY_SERVICE_START_CONFIRMED=1
  ok "Service restarted via launchd"
}

restart_systemd() {
  DEPLOY_SERVICE_STOP_ATTEMPTED=1
  DEPLOY_SERVICE_START_ATTEMPTED=1
  _persist_deploy_candidate_drain_authority \
    || { error "Could not arm durable candidate drain authority"; return 1; }
  systemctl --user restart agentdesk-dcserver.service
  capture_deploy_candidate_process_after_start \
    || { error "Could not capture the restarted systemd process identity"; return 1; }
  DEPLOY_SERVICE_START_CONFIRMED=1
  ok "Service restarted via systemd"
}

case "$OS" in
  darwin)
    DEPLOY_RESTART_ARMED=1
    restart_launchd
    ;;
  linux)
    DEPLOY_RESTART_ARMED=1
    restart_systemd
    ;;
  *)      info "Restart manually" ;;
esac

# ── Step 5: Smoke test ────────────────────────────────────────────────────────
info "Waiting for health check (port $HEALTH_PORT)..."

# #4382: read the LIVE `degraded_reasons` straight off public /api/health — the
# axis that actually decides `degraded`/`status` — so a degraded deploy names its
# own cause in the log instead of leaving operators to misread the unrelated
# `startup_degraded_reasons`. jq-optional (falls back to the pure-bash readers).
log_health_degraded_reasons() {
  local body_file http_code health_json status degraded reasons
  body_file="$(mktemp)"
  # #4386-review defect 2: do NOT use `curl -f`. A degraded deploy answers
  # /api/health with HTTP 503, and `-f` discards the body on non-2xx — throwing
  # away the exact `degraded_reasons` this function exists to log. Capture the
  # body regardless of status code (`-o` + `-w '%{http_code}'`).
  http_code="$(curl -s --max-time 3 -o "$body_file" -w '%{http_code}' \
    "http://${ADK_DEFAULT_LOOPBACK}:${HEALTH_PORT}/api/health" 2>/dev/null || true)"
  health_json="$(cat "$body_file" 2>/dev/null || true)"
  rm -f "$body_file"
  if [ -z "$health_json" ]; then
    info "  health snapshot unavailable (could not read /api/health, http=${http_code:-none})"
    return 0
  fi
  status="$(_health_json_get_string_field "$health_json" status)"
  reasons="$(_health_json_get_string_array_csv "$health_json" degraded_reasons)"
  if _health_json_field_is_true "$health_json" degraded; then
    degraded=true
  else
    degraded=false
  fi
  info "  health(${http_code:-?}): status=${status:-unknown} degraded=${degraded} degraded_reasons=[${reasons}]"
}

if wait_for_http_service_health "$LABEL" "$HEALTH_PORT" 10 2 0 1; then
  ok "Health check passed on :$HEALTH_PORT/api/health"
  log_health_degraded_reasons
  finalize_healthy_deploy_generation \
    || fail "Could not protect and commit the healthy deploy generation"
else
  log_health_degraded_reasons
  fail "Health check failed after waiting for :$HEALTH_PORT/api/health. Check logs:"
  echo "  $AD_HOME/logs/dcserver.stdout.log"
  echo "  $AD_HOME/logs/dcserver.stderr.log"
fi

echo ""
ok "Deploy complete."
