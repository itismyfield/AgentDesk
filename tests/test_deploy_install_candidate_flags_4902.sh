#!/usr/bin/env bash
# shellcheck disable=SC2034,SC2317,SC2329
# Regression coverage for #4902 lifecycle blockers 4/5 in deploy.sh/install.sh.
set -euo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_SCRIPT="$TEST_DIR/../scripts/deploy.sh"
INSTALL_SCRIPT="$TEST_DIR/../scripts/install.sh"

fail_test() {
  echo "FAIL: $*" >&2
  exit 1
}

extract_function() {
  local file="$1"
  local name="$2"

  awk -v start="^${name}[(][)] [{]$" '
    printing && $0 ~ /^[A-Za-z_][A-Za-z0-9_]*[(][)] [{]$/ { exit }
    $0 ~ start { printing = 1 }
    printing { print }
  ' "$file"
}

load_functions() {
  local file="$1"
  shift
  local name

  for name in "$@"; do
    eval "$(extract_function "$file" "$name")"
    command -v "$name" >/dev/null 2>&1 \
      || fail_test "could not extract $name from $file"
  done
}

# deploy.sh must bind the post-bootstrap process, unload the launchd job, drain
# that exact PID/start identity, and only then restore/start the previous pair.
(
  load_functions "$DEPLOY_SCRIPT" \
    _deploy_service_job_is_running \
    _deploy_current_service_pid \
    _capture_deploy_candidate_process \
    _persist_deploy_candidate_drain_authority \
    capture_deploy_candidate_process_after_start \
    _deploy_candidate_process_is_alive \
    _deploy_candidate_port_refuses_connections \
    _deploy_candidate_drain_is_proven \
    wait_for_deploy_candidate_stop \
    _force_stop_deploy_candidate \
    stop_deploy_service_for_rollback \
    restart_launchd \
    cleanup_deploy_transaction

  TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-deploy-candidate.XXXXXX")"
  trap 'rm -rf "$TEST_ROOT"' EXIT
  HOME="$TEST_ROOT/home"
  mkdir -p "$HOME/Library/LaunchAgents"
  : > "$HOME/Library/LaunchAgents/com.agentdesk.release.plist"

  OS=darwin
  LABEL=com.agentdesk.release
  ADK_DEFAULT_LOOPBACK=127.0.0.1
  HEALTH_PORT=8791
  JOB_LOADED=0
  CANDIDATE_ALIVE=0
  PORT_OPEN=0
  IDENTITY_AVAILABLE=1
  DRAIN_MODE=0
  RELOAD_ON_DRAIN=0
  EVENTS=""
  RESTORED=0
  DEPLOY_SERVICE_CANDIDATE_PID=""
  DEPLOY_SERVICE_CANDIDATE_IDENTITY=""
  DEPLOY_SERVICE_START_ATTEMPTED=0
  DEPLOY_SERVICE_START_CONFIRMED=0
  AD_HOME="$TEST_ROOT/runtime"
  ROUTINE_ASSET_TXN=txn
  DRAIN_MARKER=0
  DRAIN_CAPTURE_STATE=''
  adk_persist_routine_asset_candidate_drain_authority() {
    DRAIN_MARKER=1
    if [ -n "$4" ] && [ -n "$5" ]; then
      DRAIN_CAPTURE_STATE="exact:$4:$5"
    else
      DRAIN_CAPTURE_STATE=provisional
    fi
  }
  adk_routine_asset_candidate_drain_authority_exists() {
    [ "$DRAIN_MARKER" = 1 ]
  }
  adk_clear_routine_asset_candidate_drain_authority() { DRAIN_MARKER=0; }
  _launchd_domain() { printf 'gui/test\n'; }
  _kickstart_launchd_job_if_needed() { :; }
  info() { :; }
  ok() { :; }
  error() { EVENTS="${EVENTS}error "; }
  launchctl() {
    case "$1" in
      bootout)
        EVENTS="${EVENTS}bootout "
        JOB_LOADED=0
        ;;
      bootstrap)
        EVENTS="${EVENTS}bootstrap "
        JOB_LOADED=1
        CANDIDATE_ALIVE=1
        ;;
      print)
        [ "$JOB_LOADED" = 1 ] || return 1
        printf '    pid = 4242\n'
        ;;
      *) return 90 ;;
    esac
  }
  adk_process_identity() {
    [ "$IDENTITY_AVAILABLE" = 1 ] || return 1
    [ "$1" = 4242 ] || return 1
    printf 'candidate-start\n'
  }
  adk_process_instance_alive() {
    [ "$1" = 4242 ] && [ "$2" = candidate-start ] || return 1
    EVENTS="${EVENTS}probe "
    [ "$CANDIDATE_ALIVE" = 1 ]
  }
  curl() {
    if [ "$PORT_OPEN" = 1 ]; then
      EVENTS="${EVENTS}port-open "
      return 0
    fi
    EVENTS="${EVENTS}port-closed "
    return 7
  }
  sleep() {
    if [ "$DRAIN_MODE" = 1 ]; then
      if [ "$CANDIDATE_ALIVE" = 1 ]; then
        EVENTS="${EVENTS}drain-pid "
        CANDIDATE_ALIVE=0
        if [ "$RELOAD_ON_DRAIN" = 1 ]; then
          JOB_LOADED=1
          PORT_OPEN=0
          EVENTS="${EVENTS}reload-job "
        fi
      elif [ "$PORT_OPEN" = 1 ]; then
        EVENTS="${EVENTS}drain-port "
        PORT_OPEN=0
      fi
    fi
  }

  restart_launchd
  [ "$DEPLOY_SERVICE_CANDIDATE_PID" = 4242 ] \
    && [ "$DEPLOY_SERVICE_CANDIDATE_IDENTITY" = candidate-start ] \
    && [ "$DEPLOY_SERVICE_START_CONFIRMED" = 1 ] \
    && [ "$DRAIN_CAPTURE_STATE" = 'exact:4242:candidate-start' ] \
    || exit 101

  EVENTS=""
  DRAIN_MODE=1
  PORT_OPEN=1
  DEPLOY_LOCK_HELD=1
  DEPLOY_LOCK_FILE="$TEST_ROOT/deploy.lock"
  DEPLOY_HEALTH_OK=0
  DEPLOY_BINARY_PROMOTED=1
  DEPLOY_RESTART_ARMED=1
  DEPLOY_SERVICE_STOP_ATTEMPTED=1
  DEPLOY_SERVICE_STOP_CONFIRMED=1
  DEPLOY_SERVICE_WAS_RUNNING=1
  DEPLOY_SERVICE_START_ATTEMPTED=1
  adk_routine_asset_lock_owned() { return 0; }
  _adk_active_txn() { printf 'txn\n'; }
  adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
  restore_previous_install() {
    [ "$JOB_LOADED" = 0 ] && [ "$CANDIDATE_ALIVE" = 0 ] \
      && [ "$PORT_OPEN" = 0 ] || return 91
    EVENTS="${EVENTS}restore "
    RESTORED=1
  }
  adk_rollback_routine_asset_transaction() {
    [ "$RESTORED" = 1 ] || return 92
    EVENTS="${EVENTS}rollback-assets "
  }
  adk_commit_routine_asset_transaction_forward() { return 93; }
  start_previous_deploy_service() {
    [ "$RESTORED" = 1 ] && [ "$CANDIDATE_ALIVE" = 0 ] \
      && [ "$PORT_OPEN" = 0 ] || return 94
    EVENTS="${EVENTS}restart-old "
  }
  adk_release_routine_asset_lock() { :; }
  cleanup_backup() { :; }

  cleanup_deploy_transaction 1 || :
  [ "$EVENTS" = \
    'bootout probe drain-pid probe port-open drain-port probe port-closed probe port-closed restore rollback-assets restart-old ' ] \
    || { echo "unexpected deploy recovery events: $EVENTS" >&2; exit 102; }

  # A supervisor can be reloaded while the exact candidate is draining. A
  # dead old PID plus a refusing port is insufficient at marker-clear time.
  EVENTS=""
  JOB_LOADED=1
  CANDIDATE_ALIVE=1
  PORT_OPEN=1
  DRAIN_MODE=1
  RELOAD_ON_DRAIN=1
  DRAIN_MARKER=1
  if stop_deploy_service_for_rollback; then
    exit 103
  fi
  [ "$JOB_LOADED" = 1 ] && [ "$CANDIDATE_ALIVE" = 0 ] \
    && [ "$PORT_OPEN" = 0 ] && [ "$DRAIN_MARKER" = 1 ] \
    && [[ "$EVENTS" == *'reload-job '* ]] || exit 104

  EVENTS=""
  JOB_LOADED=1
  CANDIDATE_ALIVE=1
  PORT_OPEN=0
  DRAIN_MODE=0
  RELOAD_ON_DRAIN=0
  IDENTITY_AVAILABLE=0
  DEPLOY_SERVICE_CANDIDATE_PID=""
  DEPLOY_SERVICE_CANDIDATE_IDENTITY=""
  DEPLOY_SERVICE_START_CONFIRMED=0
  if stop_deploy_service_for_rollback; then
    exit 103
  fi
  [ "$JOB_LOADED" = 0 ] && [ "$CANDIDATE_ALIVE" = 1 ] \
    && [ "$EVENTS" = 'bootout error ' ] \
    || exit 104

  # A start side effect can outlive the launchd job record. START_ATTEMPTED is
  # still enough to prohibit an identity-free in-place restore.
  EVENTS=""
  JOB_LOADED=0
  if stop_deploy_service_for_rollback; then
    exit 105
  fi
  [ "$CANDIDATE_ALIVE" = 1 ] && [ "$EVENTS" = 'bootout error ' ] \
    || exit 106

  # TERM after durable commit intent but before DEPLOY_HEALTH_OK must finish the
  # new pair and must never restart the old service.
  EVENTS=""
  DEPLOY_LOCK_HELD=1
  DEPLOY_HEALTH_OK=0
  DEPLOY_BINARY_PROMOTED=1
  DEPLOY_SERVICE_STOP_CONFIRMED=1
  DEPLOY_SERVICE_WAS_RUNNING=1
  adk_routine_asset_transaction_phase() { printf 'committing\n'; }
  adk_commit_routine_asset_transaction_forward() {
    EVENTS="${EVENTS}commit-assets "
  }
  start_previous_deploy_service() { EVENTS="${EVENTS}restart-old "; }
  cleanup_deploy_transaction 143 || :
  [ "$EVENTS" = 'commit-assets ' ] || exit 107

  # A failed exact drain retains both the candidate and the active asset
  # transaction; no binary restore, asset commit/rollback, or old restart runs.
  EVENTS=""
  DEPLOY_LOCK_HELD=1
  DEPLOY_HEALTH_OK=0
  DEPLOY_BINARY_PROMOTED=1
  DEPLOY_RESTART_ARMED=1
  adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
  stop_deploy_service_for_rollback() {
    EVENTS="${EVENTS}stop-failed "
    return 1
  }
  DRAIN_MARKER=1
  restore_previous_install() { EVENTS="${EVENTS}restore "; }
  adk_commit_routine_asset_transaction_forward() { EVENTS="${EVENTS}commit-assets "; }
  adk_rollback_routine_asset_transaction() { EVENTS="${EVENTS}rollback-assets "; }
  start_previous_deploy_service() { EVENTS="${EVENTS}restart-old "; }
  cleanup_deploy_transaction 1 || :
  [ "$EVENTS" = 'stop-failed error ' ] && [ "$DRAIN_MARKER" = 1 ] || exit 108

  # A fresh invocation has none of the volatile candidate state. The durable
  # marker must prevent its EXIT cleanup from rolling the promoted assets back.
  EVENTS=""
  DEPLOY_LOCK_HELD=1
  DEPLOY_BINARY_PROMOTED=0
  DEPLOY_SERVICE_START_ATTEMPTED=0
  cleanup_deploy_transaction 1 || :
  [ "$EVENTS" = 'error ' ] && [ "$DRAIN_MARKER" = 1 ] || exit 109
) || fail_test 'deploy recovery did not exact-drain the captured candidate before restore'

# install.sh has the same post-bootstrap identity and exact-drain requirement.
(
  load_functions "$INSTALL_SCRIPT" \
    _install_service_job_is_loaded \
    _install_current_service_pid \
    _capture_install_candidate_process \
    _persist_install_candidate_drain_authority \
    capture_install_candidate_process_after_start \
    _install_candidate_process_is_alive \
    _install_candidate_port_refuses_connections \
    _install_candidate_drain_is_proven \
    wait_for_install_candidate_stop \
    _force_stop_install_candidate \
    start_install_service \
    stop_install_service_for_recovery \
    _install_cleanup

  TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-install-candidate.XXXXXX")"
  trap 'rm -rf "$TEST_ROOT"' EXIT
  INSTALL_LAUNCHD_DOMAIN=gui/test
  LAUNCHD_LABEL=com.agentdesk.release
  DEFAULT_LOOPBACK=127.0.0.1
  INSTALL_PORT=8791
  JOB_LOADED=0
  CANDIDATE_ALIVE=0
  PORT_OPEN=0
  IDENTITY_AVAILABLE=1
  DRAIN_MODE=0
  RELOAD_ON_DRAIN=0
  EVENTS=""
  RESTORED=0
  INSTALL_SERVICE_CANDIDATE_PID=""
  INSTALL_SERVICE_CANDIDATE_IDENTITY=""
  INSTALL_SERVICE_START_ATTEMPTED=0
  INSTALL_SERVICE_START_CONFIRMED=0
  INSTALL_ROUTINE_ASSET_RUNTIME="$TEST_ROOT/runtime"
  INSTALL_ROUTINE_ASSET_TXN=txn
  DRAIN_MARKER=0
  DRAIN_CAPTURE_STATE=''
  adk_persist_routine_asset_candidate_drain_authority() {
    DRAIN_MARKER=1
    if [ -n "$4" ] && [ -n "$5" ]; then
      DRAIN_CAPTURE_STATE="exact:$4:$5"
    else
      DRAIN_CAPTURE_STATE=provisional
    fi
  }
  adk_routine_asset_candidate_drain_authority_exists() {
    [ "$DRAIN_MARKER" = 1 ]
  }
  adk_clear_routine_asset_candidate_drain_authority() { DRAIN_MARKER=0; }
  launchctl() {
    case "$1" in
      bootstrap)
        EVENTS="${EVENTS}bootstrap "
        JOB_LOADED=1
        CANDIDATE_ALIVE=1
        ;;
      bootout)
        EVENTS="${EVENTS}bootout "
        JOB_LOADED=0
        ;;
      print)
        [ "$JOB_LOADED" = 1 ] || return 1
        printf '    pid = 5252\n'
        ;;
      *) return 90 ;;
    esac
  }
  adk_process_identity() {
    [ "$IDENTITY_AVAILABLE" = 1 ] || return 1
    [ "$1" = 5252 ] || return 1
    printf 'install-candidate-start\n'
  }
  adk_process_instance_alive() {
    [ "$1" = 5252 ] && [ "$2" = install-candidate-start ] || return 1
    EVENTS="${EVENTS}probe "
    [ "$CANDIDATE_ALIVE" = 1 ]
  }
  curl() {
    if [ "$PORT_OPEN" = 1 ]; then
      EVENTS="${EVENTS}port-open "
      return 0
    fi
    EVENTS="${EVENTS}port-closed "
    return 7
  }
  sleep() {
    if [ "$DRAIN_MODE" = 1 ]; then
      if [ "$CANDIDATE_ALIVE" = 1 ]; then
        EVENTS="${EVENTS}drain-pid "
        CANDIDATE_ALIVE=0
        if [ "$RELOAD_ON_DRAIN" = 1 ]; then
          JOB_LOADED=1
          PORT_OPEN=0
          EVENTS="${EVENTS}reload-job "
        fi
      elif [ "$PORT_OPEN" = 1 ]; then
        EVENTS="${EVENTS}drain-port "
        PORT_OPEN=0
      fi
    fi
  }

  start_install_service "$TEST_ROOT/agentdesk.plist" 1
  [ "$INSTALL_SERVICE_CANDIDATE_PID" = 5252 ] \
    && [ "$INSTALL_SERVICE_CANDIDATE_IDENTITY" = install-candidate-start ] \
    && [ "$INSTALL_SERVICE_START_CONFIRMED" = 1 ] \
    && [ "$DRAIN_CAPTURE_STATE" = 'exact:5252:install-candidate-start' ] \
    || exit 111

  EVENTS=""
  DRAIN_MODE=1
  PORT_OPEN=1
  INSTALL_LOCK_HELD=1
  INSTALL_LOCK_FILE="$TEST_ROOT/deploy.lock"
  INSTALL_ROUTINE_ASSET_RUNTIME="$TEST_ROOT/runtime"
  INSTALL_ASSET_FINALIZED=0
  INSTALL_COMMIT_INTENT=0
  INSTALL_SERVICE_STOP_ATTEMPTED=1
  INSTALL_SERVICE_STOP_CONFIRMED=1
  INSTALL_SERVICE_WAS_RUNNING=1
  INSTALL_SERVICE_HEALTHY=0
  INSTALL_BINARY_STAGE=""
  INSTALL_BINARY_BACKUP=""
  adk_routine_asset_lock_owned() { return 0; }
  _adk_active_txn() { printf 'txn\n'; }
  adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
  _install_binary_is_promoted() { return 0; }
  _restore_install_binary_transaction() {
    [ "$JOB_LOADED" = 0 ] && [ "$CANDIDATE_ALIVE" = 0 ] \
      && [ "$PORT_OPEN" = 0 ] || return 91
    EVENTS="${EVENTS}restore "
    RESTORED=1
  }
  adk_rollback_routine_asset_transaction() {
    [ "$RESTORED" = 1 ] || return 92
    EVENTS="${EVENTS}rollback-assets "
  }
  adk_commit_routine_asset_transaction_forward() { return 93; }
  restart_previous_install_service() {
    [ "$RESTORED" = 1 ] && [ "$CANDIDATE_ALIVE" = 0 ] \
      && [ "$PORT_OPEN" = 0 ] || return 94
    EVENTS="${EVENTS}restart-old "
  }
  adk_release_routine_asset_lock() { :; }

  _install_cleanup 1 || :
  [ "$EVENTS" = \
    'bootout probe drain-pid probe port-open drain-port probe port-closed probe port-closed restore rollback-assets restart-old ' ] \
    || { echo "unexpected install recovery events: $EVENTS" >&2; exit 112; }

  # Re-loading launchd during the drain invalidates the proof even if the old
  # exact PID is dead and the TCP port has not rebound yet.
  EVENTS=""
  JOB_LOADED=1
  CANDIDATE_ALIVE=1
  PORT_OPEN=1
  DRAIN_MODE=1
  RELOAD_ON_DRAIN=1
  DRAIN_MARKER=1
  if stop_install_service_for_recovery; then
    exit 113
  fi
  [ "$JOB_LOADED" = 1 ] && [ "$CANDIDATE_ALIVE" = 0 ] \
    && [ "$PORT_OPEN" = 0 ] && [ "$DRAIN_MARKER" = 1 ] \
    && [[ "$EVENTS" == *'reload-job '* ]] || exit 114

  EVENTS=""
  JOB_LOADED=1
  CANDIDATE_ALIVE=1
  PORT_OPEN=0
  DRAIN_MODE=0
  RELOAD_ON_DRAIN=0
  IDENTITY_AVAILABLE=0
  INSTALL_SERVICE_CANDIDATE_PID=""
  INSTALL_SERVICE_CANDIDATE_IDENTITY=""
  INSTALL_SERVICE_START_CONFIRMED=0
  if stop_install_service_for_recovery 2>/dev/null; then
    exit 113
  fi
  [ "$JOB_LOADED" = 0 ] && [ "$CANDIDATE_ALIVE" = 1 ] \
    && [ "$EVENTS" = 'bootout ' ] \
    || exit 114

  EVENTS=""
  JOB_LOADED=0
  if stop_install_service_for_recovery 2>/dev/null; then
    exit 115
  fi
  [ "$CANDIDATE_ALIVE" = 1 ] && [ "$EVENTS" = 'bootout ' ] \
    || exit 116

  # The on-disk committing phase is authoritative if TERM precedes the
  # INSTALL_COMMIT_INTENT assignment.
  EVENTS=""
  INSTALL_LOCK_HELD=1
  INSTALL_ASSET_FINALIZED=0
  INSTALL_COMMIT_INTENT=0
  INSTALL_SERVICE_HEALTHY=0
  INSTALL_SERVICE_STOP_CONFIRMED=1
  INSTALL_SERVICE_WAS_RUNNING=1
  adk_routine_asset_transaction_phase() { printf 'committing\n'; }
  adk_commit_routine_asset_transaction_forward() {
    EVENTS="${EVENTS}commit-assets "
  }
  restart_previous_install_service() { EVENTS="${EVENTS}restart-old "; }
  _install_cleanup 143 || :
  [ "$EVENTS" = 'commit-assets ' ] || exit 117

  # After commit closes the marker, the in-memory intent still prohibits an old
  # restart before INSTALL_ASSET_FINALIZED is assigned.
  EVENTS=""
  INSTALL_LOCK_HELD=1
  INSTALL_COMMIT_INTENT=1
  _adk_active_txn() { return 1; }
  _install_cleanup 143 || :
  [ -z "$EVENTS" ] || exit 118

  EVENTS=""
  INSTALL_LOCK_HELD=1
  INSTALL_COMMIT_INTENT=0
  _adk_active_txn() { printf 'txn\n'; }
  adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
  stop_install_service_for_recovery() {
    EVENTS="${EVENTS}stop-failed "
    return 1
  }
  DRAIN_MARKER=1
  _restore_install_binary_transaction() { EVENTS="${EVENTS}restore "; }
  adk_commit_routine_asset_transaction_forward() { EVENTS="${EVENTS}commit-assets "; }
  adk_rollback_routine_asset_transaction() { EVENTS="${EVENTS}rollback-assets "; }
  restart_previous_install_service() { EVENTS="${EVENTS}restart-old "; }
  _install_cleanup 1 >/dev/null 2>&1 || :
  [ "$EVENTS" = 'stop-failed ' ] && [ "$DRAIN_MARKER" = 1 ] || exit 119

  EVENTS=""
  INSTALL_LOCK_HELD=1
  INSTALL_SERVICE_START_ATTEMPTED=0
  _install_cleanup 1 >/dev/null 2>&1 || :
  [ -z "$EVENTS" ] && [ "$DRAIN_MARKER" = 1 ] || exit 120
) || fail_test 'install recovery did not exact-drain the captured candidate before restore'

# deploy.sh snapshots both old paths, forces the committed real binary to uchg,
# preserves the wrapper policy, and restores both prior policies on rollback.
(
  load_functions "$DEPLOY_SCRIPT" \
    _deploy_immutable_flag_state \
    _deploy_apply_immutable_flag_state \
    clear_deploy_immutable_flag \
    snapshot_deploy_immutable_flags \
    restore_deploy_snapshot_immutable_flags \
    apply_deploy_committed_immutable_flags \
    finalize_healthy_deploy_generation \
    install_file_atomically \
    restore_previous_install \
    cleanup_deploy_transaction

  TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-deploy-flags.XXXXXX")"
  trap 'rm -rf "$TEST_ROOT"' EXIT
  mkdir -p "$TEST_ROOT/bin" "$TEST_ROOT/libexec"
  WRAPPER_BIN="$TEST_ROOT/bin/agentdesk"
  REAL_BIN="$TEST_ROOT/libexec/agentdesk"
  BACKUP_WRAPPER="$TEST_ROOT/wrapper.old"
  BACKUP_REAL="$TEST_ROOT/real.old"
  printf 'old-wrapper\n' > "$WRAPPER_BIN"
  printf 'old-real\n' > "$REAL_BIN"
  cp "$WRAPPER_BIN" "$BACKUP_WRAPPER"
  cp "$REAL_BIN" "$BACKUP_REAL"

  OS=darwin
  WRAPPER_FLAG=1
  REAL_FLAG=0
  VERIFY_REAL_UCHG=1
  EVENTS=""
  DEPLOY_IMMUTABLE_SNAPSHOT_TAKEN=0
  DEPLOY_IMMUTABLE_MUTATION_ARMED=0
  stat() {
    local path="${!#}"
    case "$path" in
      "$WRAPPER_BIN") [ "$WRAPPER_FLAG" = 1 ] && printf 'uchg\n' || printf -- '-\n' ;;
      "$REAL_BIN") [ "$REAL_FLAG" = 1 ] && printf 'uchg\n' || printf -- '-\n' ;;
      *) printf -- '-\n' ;;
    esac
  }
  chflags() {
    local mode="$1"
    local path="$2"
    EVENTS="${EVENTS}${mode}:$(basename "$path") "
    case "$path:$mode" in
      "$WRAPPER_BIN:uchg") WRAPPER_FLAG=1 ;;
      "$WRAPPER_BIN:nouchg") WRAPPER_FLAG=0 ;;
      "$REAL_BIN:uchg") [ "$VERIFY_REAL_UCHG" = 1 ] && REAL_FLAG=1 ;;
      "$REAL_BIN:nouchg") REAL_FLAG=0 ;;
    esac
  }

  snapshot_deploy_immutable_flags
  [ "$DEPLOY_WRAPPER_OLD_IMMUTABLE" = 1 ] \
    && [ "$DEPLOY_REAL_OLD_IMMUTABLE" = 0 ] \
    && [ "$DEPLOY_IMMUTABLE_SNAPSHOT_TAKEN" = 1 ] \
    || exit 121

  printf 'new-wrapper\n' > "$WRAPPER_BIN"
  printf 'new-real\n' > "$REAL_BIN"
  DEPLOY_HEALTH_OK=0
  AD_HOME="$TEST_ROOT/runtime"
  ROUTINE_ASSET_TXN=txn
  adk_mark_routine_asset_transaction_committing() { EVENTS="${EVENTS}mark "; }
  adk_commit_routine_asset_transaction() { EVENTS="${EVENTS}commit "; }
  error() { return 99; }

  EVENTS=""
  finalize_healthy_deploy_generation
  [ "$WRAPPER_FLAG" = 1 ] && [ "$REAL_FLAG" = 1 ] \
    && [ "$DEPLOY_HEALTH_OK" = 1 ] \
    && [ "$EVENTS" = 'uchg:agentdesk uchg:agentdesk mark commit ' ] \
    || exit 122

  restore_previous_install
  [ "$(cat "$WRAPPER_BIN")" = old-wrapper ] \
    && [ "$(cat "$REAL_BIN")" = old-real ] \
    && [ "$WRAPPER_FLAG" = 1 ] && [ "$REAL_FLAG" = 0 ] \
    || exit 123

  # Model failure after only the wrapper flag was cleared (for example backup
  # mktemp/cp failure). Cleanup must restore both snapshot policies before the
  # old service can restart, even though no binary rename happened.
  clear_deploy_immutable_flag "$WRAPPER_BIN"
  DEPLOY_IMMUTABLE_MUTATION_ARMED=1
  DEPLOY_LOCK_HELD=1
  DEPLOY_LOCK_FILE="$TEST_ROOT/deploy.lock"
  DEPLOY_HEALTH_OK=0
  DEPLOY_BINARY_PROMOTED=0
  DEPLOY_SERVICE_STOP_CONFIRMED=1
  DEPLOY_SERVICE_WAS_RUNNING=1
  EVENTS=""
  adk_routine_asset_lock_owned() { return 0; }
  adk_routine_asset_candidate_drain_authority_exists() { return 1; }
  _adk_active_txn() { printf 'txn\n'; }
  adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
  adk_rollback_routine_asset_transaction() { EVENTS="${EVENTS}rollback-assets "; }
  adk_commit_routine_asset_transaction_forward() { return 91; }
  start_previous_deploy_service() {
    [ "$WRAPPER_FLAG" = 1 ] && [ "$REAL_FLAG" = 0 ] || return 92
    EVENTS="${EVENTS}restart-old "
  }
  adk_release_routine_asset_lock() { :; }
  cleanup_backup() { :; }
  error() { EVENTS="${EVENTS}error "; }
  cleanup_deploy_transaction 1 || :
  [ "$DEPLOY_IMMUTABLE_MUTATION_ARMED" = 0 ] \
    && [ "$WRAPPER_FLAG" = 1 ] && [ "$REAL_FLAG" = 0 ] \
    && [ "$EVENTS" = 'uchg:agentdesk nouchg:agentdesk rollback-assets restart-old ' ] \
    || exit 126

  printf 'new-real\n' > "$REAL_BIN"
  DEPLOY_HEALTH_OK=0
  VERIFY_REAL_UCHG=0
  EVENTS=""
  if finalize_healthy_deploy_generation; then
    exit 124
  fi
  [ "$DEPLOY_HEALTH_OK" = 0 ] \
    && [[ "$EVENTS" != *'mark '* ]] \
    && [[ "$EVENTS" != *'commit '* ]] \
    || exit 125
) || fail_test 'deploy immutable state was not verified before commit or restored exactly'

# install.sh snapshots stat-derived state, verifies uchg before finalization, and
# keeps recovery armed when verification fails.
(
  load_functions "$INSTALL_SCRIPT" \
    _install_binary_immutable_flag_state \
    _install_binary_has_immutable_flag \
    _install_apply_binary_immutable_flag_state \
    _install_clear_binary_immutable_flag \
    _install_restore_old_binary_flag \
    prepare_install_binary_transaction \
    finalize_healthy_install_generation

  TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-install-flags.XXXXXX")"
  trap 'rm -rf "$TEST_ROOT"' EXIT
  mkdir -p "$TEST_ROOT/runtime/bin"
  SOURCE_BINARY="$TEST_ROOT/candidate"
  INSTALL_BINARY_LIVE="$TEST_ROOT/runtime/bin/agentdesk"
  printf 'candidate\n' > "$SOURCE_BINARY"
  printf 'old\n' > "$INSTALL_BINARY_LIVE"
  chmod +x "$SOURCE_BINARY" "$INSTALL_BINARY_LIVE"

  OS=darwin
  LIVE_FLAG=1
  BACKUP_FLAG=1
  VERIFY_UCHG=1
  EVENTS=""
  INSTALL_BINARY_STAGE=""
  INSTALL_BINARY_BACKUP=""
  INSTALL_BINARY_HAD_LIVE=0
  INSTALL_BINARY_OLD_IMMUTABLE=0
  INSTALL_BINARY_FLAG_SNAPSHOT_TAKEN=0
  stat() {
    local path="${!#}"
    case "$path" in
      "$INSTALL_BINARY_LIVE") [ "$LIVE_FLAG" = 1 ] && printf 'uchg\n' || printf -- '-\n' ;;
      *.agentdesk.rollback.*) [ "$BACKUP_FLAG" = 1 ] && printf 'uchg\n' || printf -- '-\n' ;;
      *) printf -- '-\n' ;;
    esac
  }
  chflags() {
    local mode="$1"
    local path="$2"
    EVENTS="${EVENTS}${mode} "
    case "$path:$mode" in
      "$INSTALL_BINARY_LIVE:uchg") [ "$VERIFY_UCHG" = 1 ] && LIVE_FLAG=1 ;;
      "$INSTALL_BINARY_LIVE:nouchg") LIVE_FLAG=0 ;;
      *.agentdesk.rollback.*:nouchg) BACKUP_FLAG=0 ;;
    esac
  }
  sign_binary_with_fallback() { :; }
  _install_sha256_file() { printf 'test-sha\n'; }

  prepare_install_binary_transaction "$SOURCE_BINARY" "$TEST_ROOT/runtime"
  [ "$INSTALL_BINARY_OLD_IMMUTABLE" = 1 ] \
    && [ "$INSTALL_BINARY_FLAG_SNAPSHOT_TAKEN" = 1 ] \
    && [ "$INSTALL_BINARY_HAD_LIVE" = 1 ] \
    || exit 131

  LIVE_FLAG=0
  INSTALL_SERVICE_HEALTHY=0
  EVENTS=""
  finalize_install_routine_asset_surfaces() { EVENTS="${EVENTS}finalize "; }
  finalize_healthy_install_generation
  [ "$LIVE_FLAG" = 1 ] && [ "$INSTALL_SERVICE_HEALTHY" = 1 ] \
    && [ "$EVENTS" = 'uchg finalize ' ] \
    || exit 132

  LIVE_FLAG=0
  VERIFY_UCHG=0
  INSTALL_SERVICE_HEALTHY=0
  EVENTS=""
  if finalize_healthy_install_generation; then
    exit 133
  fi
  [ "$INSTALL_SERVICE_HEALTHY" = 0 ] \
    && [[ "$EVENTS" != *'finalize '* ]] \
    || exit 134

  VERIFY_UCHG=1
  INSTALL_BINARY_HAD_LIVE=1
  INSTALL_BINARY_FLAG_SNAPSHOT_TAKEN=1
  INSTALL_BINARY_OLD_IMMUTABLE=0
  LIVE_FLAG=1
  _install_restore_old_binary_flag
  [ "$LIVE_FLAG" = 0 ] || exit 135
  INSTALL_BINARY_OLD_IMMUTABLE=1
  _install_restore_old_binary_flag
  [ "$LIVE_FLAG" = 1 ] || exit 136
) || fail_test 'install immutable snapshot/apply/verify/rollback contract regressed'

echo "deploy/install candidate identity and immutable flag tests passed"
