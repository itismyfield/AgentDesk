#!/usr/bin/env bash
# shellcheck disable=SC2034,SC2329 # Functions/variables are invoked through extracted eval bodies.
# Targeted lifecycle coverage for #4902: arming must precede release bootout,
# and recovery may start the previous pair only after the old service stopped.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_RELEASE="$SCRIPT_DIR/../scripts/deploy-release.sh"
ASSET_SURFACE="$SCRIPT_DIR/../scripts/routine-asset-surface.sh"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

extract_function() {
    local name="$1"
    awk -v start="^${name}[(][)] [{]$" '
        printing && $0 ~ /^[A-Za-z_][A-Za-z0-9_]*[(][)] [{]$/ { exit }
        $0 ~ start { printing = 1 }
        printing { print }
    ' "$DEPLOY_RELEASE"
}

load_release_generation_functions() {
    local name
    for name in \
        _sha256_file \
        _sha256_tree \
        _manifest_latest_migration_name \
        _manifest_source_git_sha \
        _manifest_routine_helpers_binding_state \
        _release_migration_name_is_valid \
        _validate_legacy_routine_helpers_sentinel \
        _normalize_legacy_release_routine_helpers \
        _strip_legacy_helper_sentinel_from_staged_generation \
        _write_rollback_backup_metadata \
        _rollback_backup_latest_migration_name \
        _release_host_uses_immutable_flags \
        _release_binary_immutable_state \
        _snapshot_release_binary_immutable_flag \
        _set_release_binary_immutable_state \
        _restore_release_binary_immutable_flag \
        _prepare_release_rollback_generation; do
        eval "$(extract_function "$name")"
    done
}

# The stop helper must arm recovery before invoking bootout and refuse a live
# promotion until launchd/PID state confirms the old release is gone.
(
    eval "$(extract_function _release_launchd_job_is_loaded)"
    eval "$(extract_function _release_old_process_is_alive)"
    eval "$(extract_function _release_service_is_stopped)"
    eval "$(extract_function _stop_release_for_promotion)"

    PLIST_REL='com.agentdesk.release'
    LAUNCHD_DOMAIN='gui/test'
    ADK_REL='/tmp/adk-release-candidate-drain-test'
    ROUTINE_ASSET_TXN='txn'
    OLD_PID=''
    ADK_REL='/tmp/adk-release-service-recovery-test'
    ROLLBACK_ARMED=0
    RELEASE_SERVICE_RECOVERY_ARMED=0
    RELEASE_SERVICE_STOP_CONFIRMED=0
    RELEASE_SERVICE_RESTART_SAFE=0
    SERVICE_ACTIVE=1
    BOOTOUT_SAW_ARMED=0
    _pre_promotion_release_restart_is_safe() { return 0; }
    _release_candidate_port_refuses_connections() { return 0; }
    adk_process_instance_alive() { return 1; }
    sleep() { :; }
    tmux() { :; }
    launchctl() {
        case "$1" in
            bootout)
                [ "$ROLLBACK_ARMED" = 1 ] \
                    && [ "$RELEASE_SERVICE_RECOVERY_ARMED" = 1 ] \
                    || return 91
                BOOTOUT_SAW_ARMED=1
                SERVICE_ACTIVE=0
                ;;
            print)
                [ "$SERVICE_ACTIVE" = 1 ]
                ;;
            *) return 92 ;;
        esac
    }

    _stop_release_for_promotion
    [ "$BOOTOUT_SAW_ARMED" = 1 ] \
        && [ "$RELEASE_SERVICE_STOP_CONFIRMED" = 1 ] \
        && [ "$RELEASE_SERVICE_RESTART_SAFE" = 1 ] \
        || exit 93
) || fail 'release recovery was not armed and stop-confirmed before bootout'

# TERM can land after bootout returns but before STOP_CONFIRMED is assigned.
# Reconcile that exact boundary: an old job that never stopped needs no second
# bootstrap, while a now-stopped job must restart the untouched old pair.
(
    eval "$(extract_function _restart_pre_promotion_release)"

    PLIST_REL='com.agentdesk.release'
    LAUNCHD_DOMAIN='gui/test'
    ADK_DEFAULT_PORT=8791
    DEPLOY_HEALTH_RETRIES=1
    DEPLOY_HEALTH_DELAY_SECS=0
    RELEASE_SERVICE_RECOVERY_ARMED=1
    RELEASE_SERVICE_STOP_CONFIRMED=0
    RELEASE_SERVICE_RESTART_SAFE=1
    BOOTSTRAPS=0
    JOB_LOADED=1
    OLD_PROCESS_ALIVE=1
    DRAIN_WAITS=0
    _release_launchd_job_is_loaded() { [ "$JOB_LOADED" = 1 ]; }
    _release_old_process_is_alive() { [ "$OLD_PROCESS_ALIVE" = 1 ]; }
    _release_service_is_stopped() {
        [ "$JOB_LOADED" = 0 ] && [ "$OLD_PROCESS_ALIVE" = 0 ]
    }
    sleep() {
        DRAIN_WAITS=$((DRAIN_WAITS + 1))
        OLD_PROCESS_ALIVE=0
    }
    start_release_tmux_fallback() { return 0; }
    wait_for_http_service_health() { return 0; }
    _restore_release_binary_immutable_flag() { return 0; }
    xattr() { :; }
    launchctl() {
        [ "$1" = bootstrap ] || return 94
        BOOTSTRAPS=$((BOOTSTRAPS + 1))
    }

    _restart_pre_promotion_release
    [ "$BOOTSTRAPS" = 0 ] || exit 96
    JOB_LOADED=0
    _restart_pre_promotion_release
    [ "$RELEASE_SERVICE_STOP_CONFIRMED" = 1 ] \
        && [ "$DRAIN_WAITS" = 1 ] \
        && [ "$BOOTSTRAPS" = 1 ] || exit 97
) || fail 'pre-promotion recovery did not reconcile the stop-confirmation signal gap'

# Forward-only migration recovery owns every later signal boundary. A TERM
# after stop but before asset promotion must publish/start the staged candidate;
# a TERM after binary promotion but before bootstrap must start that exact live
# candidate. Neither path may call the old-generation restart/rollback seam.
(
    # shellcheck source=../scripts/routine-asset-surface.sh
    . "$ASSET_SURFACE"
    eval "$(extract_function _forward_migration_marker_path)"
    eval "$(extract_function _forward_migration_marker_value)"
    eval "$(extract_function _forward_migration_candidate_sha)"
    eval "$(extract_function _forward_migration_staged_binary)"
    eval "$(extract_function _release_current_service_pid)"
    eval "$(extract_function _capture_release_old_process)"
    eval "$(extract_function _recover_forward_migrated_release)"
    eval "$(extract_function _recover_durable_forward_migration_before_new_deploy)"

    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-forward-recovery.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    ADK_REL="$TEST_ROOT/release"
    mkdir -p "$ADK_REL/bin" "$ADK_REL/runtime"
    REL_BINARY="$ADK_REL/bin/agentdesk"
    printf 'old-incompatible\n' > "$REL_BINARY"
    PLIST_REL='com.agentdesk.release'
    FORWARD_MIGRATION_APPLIED=0
    FORWARD_MIGRATION_CANDIDATE_SHA=''
    ROLLBACK_ARMED=1
    OLD_RESTARTS=0
    chflags() { :; }
    _release_service_is_stopped() { return 0; }
    _resume_release_candidate_drain_authority() { return 0; }
    _stop_release_for_promotion() { return 90; }
    _restart_pre_promotion_release() { OLD_RESTARTS=$((OLD_RESTARTS + 1)); return 91; }
    _rollback_release_binary() { OLD_RESTARTS=$((OLD_RESTARTS + 1)); return 92; }
    _sha256_file() { adk_sha256_file "$1"; }
    _finish_forward_asset_promotion() { EVENTS="${EVENTS}promote-assets "; }
    _start_forward_migrated_release() { EVENTS="${EVENTS}start-candidate "; }
    _set_release_binary_immutable_state() { return 0; }
    _retire_release_rollback_material() { return 0; }
    adk_commit_routine_asset_transaction_forward() { EVENTS="${EVENTS}commit-assets "; }

    write_forward_marker() {
        local txn_root="$1"
        local binary="$2"
        local candidate_name="${3:-$(basename "$binary")}"
        local binary_sha marker
        binary_sha="$(adk_sha256_file "$binary")"
        marker="$txn_root/forward-migration-applied.json"
        AGENTDESK_TEST_TXN="$(basename "$txn_root")" \
        AGENTDESK_TEST_SHA="$binary_sha" \
        AGENTDESK_TEST_NAME="$candidate_name" \
        python3 - "$marker" <<'PY'
import json
import os
import sys

with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(
        {
            "format": "agentdesk-forward-migration-v1",
            "asset_transaction": os.environ["AGENTDESK_TEST_TXN"],
            "candidate_binary_sha256": os.environ["AGENTDESK_TEST_SHA"],
            "candidate_binary_name": os.environ["AGENTDESK_TEST_NAME"],
            "latest_postgres_migration": "0100_forward.sql",
            "source_git_sha": "a" * 40,
        },
        handle,
        sort_keys=True,
    )
    handle.write("\n")
PY
    }

    # TERM: service already stopped; staged candidate and staging assets remain.
    TXN_ONE="$ADK_REL/runtime/routine-assets.txn.StopGap1"
    mkdir -p "$TXN_ONE"
    STAGED_BINARY="$ADK_REL/bin/agentdesk.deploy.StopGap1"
    printf 'compatible-candidate\n' > "$STAGED_BINARY"
    write_forward_marker "$TXN_ONE" "$STAGED_BINARY"
    adk_routine_asset_transaction_phase() { printf 'staging\n'; }
    EVENTS=''
    _recover_forward_migrated_release "$TXN_ONE"
    [ "$EVENTS" = 'promote-assets start-candidate commit-assets ' ] \
        && [ "$ROLLBACK_ARMED" = 0 ] \
        && [ "$OLD_RESTARTS" = 0 ] \
        && [ ! -e "$STAGED_BINARY" ] \
        && [ "$(<"$REL_BINARY")" = compatible-candidate ] \
        || exit 110

    # TERM: compatible binary/assets promoted, but launchd bootstrap not called.
    TXN_TWO="$ADK_REL/runtime/routine-assets.txn.BootGap2"
    mkdir -p "$TXN_TWO"
    STAGED_BINARY=''
    write_forward_marker "$TXN_TWO" "$REL_BINARY" agentdesk.deploy.BootGap2
    adk_routine_asset_transaction_phase() { printf 'promoted\n'; }
    ROLLBACK_ARMED=1
    EVENTS=''
    _recover_forward_migrated_release "$TXN_TWO"
    [ "$EVENTS" = 'start-candidate commit-assets ' ] \
        && [ "$ROLLBACK_ARMED" = 0 ] \
        && [ "$OLD_RESTARTS" = 0 ] \
        || exit 111

    # A compatible candidate may serve, but its asset transaction cannot commit
    # while an incompatible rollback generation remains usable.
    _retire_release_rollback_material() {
        EVENTS="${EVENTS}retire-failed "
        return 1
    }
    ROLLBACK_ARMED=1
    EVENTS=''
    if _recover_forward_migrated_release "$TXN_TWO" >/dev/null 2>&1; then
        exit 119
    fi
    [ "$EVENTS" = 'start-candidate retire-failed ' ] \
        && [ "$ROLLBACK_ARMED" = 1 ] \
        && [ "$OLD_RESTARTS" = 0 ] \
        || exit 120

    # New invocation: only the durable active marker + persisted candidate name
    # remain. Recovery must consume that generation before generic begin can
    # roll the staged assets backward.
    TXN_THREE="$ADK_REL/runtime/routine-assets.txn.NextRun3"
    mkdir -p "$TXN_THREE"
    DURABLE_CANDIDATE="$ADK_REL/bin/agentdesk.deploy.NextRun3"
    printf 'next-invocation-candidate\n' > "$DURABLE_CANDIDATE"
    write_forward_marker "$TXN_THREE" "$DURABLE_CANDIDATE"
    STAGED_BINARY=''
    ACTIVE_TXN=1
    _adk_active_txn() {
        [ "$ACTIVE_TXN" = 1 ] || return 1
        printf '%s\n' "$TXN_THREE"
    }
    adk_routine_asset_transaction_phase() { printf 'staging\n'; }
    _retire_release_rollback_material() { return 0; }
    adk_commit_routine_asset_transaction_forward() {
        EVENTS="${EVENTS}commit-assets "
        ACTIVE_TXN=0
    }
    _resolve_release_server_port() { printf '8791\n'; }
    LAUNCHD_DOMAIN='gui/test'
    OLD_SERVICE_LIVE=1
    OLD_PORT_OPEN=1
    OLD_STOP_CAPTURED=0
    _release_launchd_job_is_loaded() { [ "$OLD_SERVICE_LIVE" = 1 ]; }
    _release_candidate_port_refuses_connections() { [ "$OLD_PORT_OPEN" = 0 ]; }
    _release_current_service_pid() { printf '5151\n'; }
    adk_process_identity() {
        [ "$1" = 5151 ] || return 1
        printf 'old-release-instance\n'
    }
    kill() { [ "$1" = -0 ] && [ "$2" = 5151 ]; }
    _release_service_is_stopped() {
        [ "$OLD_SERVICE_LIVE" = 0 ] && [ "$OLD_PORT_OPEN" = 0 ]
    }
    _stop_release_for_promotion() {
        [ "$OLD_PID" = 5151 ] \
            && [ "$OLD_PID_IDENTITY" = old-release-instance ] || return 90
        OLD_STOP_CAPTURED=1
        OLD_SERVICE_LIVE=0
        OLD_PORT_OPEN=0
    }
    EVENTS=''
    ROLLBACK_ARMED=1
    _recover_durable_forward_migration_before_new_deploy
    [ "$EVENTS" = 'promote-assets start-candidate commit-assets ' ] \
        && [ "$ACTIVE_TXN" = 0 ] \
        && [ "$ROLLBACK_ARMED" = 0 ] \
        && [ "$OLD_STOP_CAPTURED" = 1 ] \
        && [ ! -e "$DURABLE_CANDIDATE" ] \
        && [ "$(<"$REL_BINARY")" = next-invocation-candidate ] \
        || exit 122
) || fail 'forward-migration signal recovery attempted an incompatible old generation'

# Bash defers TERM while a foreground child runs, then may deliver it before the
# next assignment. The durable fail-forward marker must therefore exist before
# the migration command starts, not merely after it reports success.
(
    eval "$(extract_function _apply_release_postgres_migration_with_forward_barrier)"
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-migration-return-gap.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    STAGED_BINARY="$TEST_ROOT/agentdesk.deploy.ReturnGap1"
    BARRIER_MARKER="$TEST_ROOT/forward-migration-applied.json"
    ROUTINE_ASSET_TXN="$TEST_ROOT/routine-assets.txn.ReturnGap1"
    mkdir -p "$ROUTINE_ASSET_TXN"
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        '[ "${1:-}" = release-migrate-postgres ] || exit 90' \
        'kill -TERM "$PPID"' \
        'exit 0' \
        > "$STAGED_BINARY"
    chmod +x "$STAGED_BINARY"
    _sha256_file() { printf '%064d\n' 0; }
    _fsync_forward_recovery_generation() {
        : > "$TEST_ROOT/generation-fsynced"
    }
    _persist_forward_migration_applied() {
        [ -f "$TEST_ROOT/generation-fsynced" ] || return 92
        [ "$1" = "$ROUTINE_ASSET_TXN" ] \
            && [ "$2" = "$(printf '%064d' 0)" ] || return 91
        printf 'durable-barrier\n' > "$BARRIER_MARKER"
    }

    set +e
    (
        trap '[ -f "$BARRIER_MARKER" ] && [ -f "$STAGED_BINARY" ] || exit 99; exit 143' TERM
        _apply_release_postgres_migration_with_forward_barrier
    )
    migration_status=$?
    set -e
    [ "$migration_status" = 143 ] && [ -f "$BARRIER_MARKER" ] \
        || exit 123
) || fail 'TERM at migration-command return preceded the durable forward barrier'

rg -q '_recover_forward_migrated_release "\$active_txn"' "$DEPLOY_RELEASE" \
    || fail 'cleanup does not route durable forward-migration phase to compatible recovery'
RECOVER_BEFORE_BEGIN_LINE="$(rg -n '^if ! _recover_durable_forward_migration_before_new_deploy; then$' "$DEPLOY_RELEASE" | cut -d: -f1)"
BEGIN_TXN_LINE="$(rg -n '^[[:space:]]+adk_begin_routine_asset_transaction "\$ADK_REL" ' "$DEPLOY_RELEASE" | cut -d: -f1)"
[ -n "$RECOVER_BEFORE_BEGIN_LINE" ] && [ -n "$BEGIN_TXN_LINE" ] \
    && [ "$RECOVER_BEFORE_BEGIN_LINE" -lt "$BEGIN_TXN_LINE" ] \
    || fail 'durable forward recovery is not dispatched before generic transaction begin'

# Even if lock ownership itself becomes unverifiable, a migration-success flag
# is enough to prohibit cleanup from deleting the only compatible staged binary.
(
    eval "$(extract_function _cleanup_on_exit)"
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-forward-retain.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    STAGED_BINARY="$TEST_ROOT/agentdesk.new"
    printf 'compatible-candidate\n' > "$STAGED_BINARY"
    DEPLOY_LOCK_FILE="$TEST_ROOT/deploy.lock"
    DEPLOY_OK=0
    FORWARD_MIGRATION_APPLIED=1
    POLICIES_STAGED=''
    LAUNCHD_MIGRATED_STAGED=''
    RELEASE_ROOT_SCRIPTS_STAGED=''
    ROUTINE_ASSET_INCOMING_CLAIMED=0
    ROUTINE_ASSET_INCOMING=''
    adk_routine_asset_lock_owned() { return 1; }
    _cleanup_owned_pg_tunnel_preflight() { :; }
    _rollback_pg_tunnel_migration() { :; }
    _finalize_detached_helper() { :; }
    set +e
    _cleanup_on_exit 143 >/dev/null 2>&1
    cleanup_status=$?
    set -e
    [ "$cleanup_status" = 143 ] && [ -f "$STAGED_BINARY" ] \
        || exit 121
) || fail 'cleanup deleted the compatible candidate after migration success'

(
    eval "$(extract_function _cleanup_on_exit)"
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-forward-unowned-marker.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    ADK_REL="$TEST_ROOT/release"
    ROUTINE_ASSET_TXN="$ADK_REL/runtime/routine-assets.txn.Unowned1"
    mkdir -p "$ROUTINE_ASSET_TXN"
    printf 'durable-marker\n' > "$ROUTINE_ASSET_TXN/forward-migration-applied.json"
    STAGED_BINARY="$ADK_REL/bin/agentdesk.deploy.Unowned1"
    mkdir -p "$(dirname "$STAGED_BINARY")"
    printf 'compatible-candidate\n' > "$STAGED_BINARY"
    DEPLOY_LOCK_FILE="$ADK_REL/runtime/deploy.lock"
    DEPLOY_OK=0
    FORWARD_MIGRATION_APPLIED=0
    POLICIES_STAGED=''
    LAUNCHD_MIGRATED_STAGED=''
    RELEASE_ROOT_SCRIPTS_STAGED=''
    ROUTINE_ASSET_INCOMING_CLAIMED=0
    ROUTINE_ASSET_INCOMING=''
    adk_routine_asset_lock_owned() { return 1; }
    _forward_migration_marker_path() {
        printf '%s/forward-migration-applied.json\n' "$1"
    }
    _cleanup_owned_pg_tunnel_preflight() { :; }
    _rollback_pg_tunnel_migration() { :; }
    _finalize_detached_helper() { :; }
    set +e
    _cleanup_on_exit 143 >/dev/null 2>&1
    cleanup_status=$?
    set -e
    [ "$cleanup_status" = 143 ] \
        && [ -f "$STAGED_BINARY" ] \
        && [ -f "$ROUTINE_ASSET_TXN/forward-migration-applied.json" ] \
        || exit 129
) || fail 'unverifiable lock ownership deleted a durable forward candidate'

# A corrupt durable marker is still an irreversible boundary. Cleanup must not
# interpret parse failure as permission to roll assets/binary backward.
(
    eval "$(extract_function _cleanup_on_exit)"
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-forward-invalid.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    ADK_REL="$TEST_ROOT/release"
    ACTIVE_TXN="$ADK_REL/runtime/routine-assets.txn.Invalid1"
    mkdir -p "$ACTIVE_TXN"
    printf 'invalid\n' > "$ACTIVE_TXN/forward-migration-applied.json"
    STAGED_BINARY="$ADK_REL/bin/agentdesk.deploy.Invalid1"
    mkdir -p "$(dirname "$STAGED_BINARY")"
    printf 'compatible-candidate\n' > "$STAGED_BINARY"
    DEPLOY_LOCK_FILE="$ADK_REL/runtime/deploy.lock"
    DEPLOY_OK=0
    FORWARD_MIGRATION_APPLIED=0
    ROLLBACK_ARMED=1
    POLICIES_STAGED=''
    LAUNCHD_MIGRATED_STAGED=''
    RELEASE_ROOT_SCRIPTS_STAGED=''
    ROUTINE_ASSET_INCOMING_CLAIMED=0
    ROUTINE_ASSET_INCOMING=''
    DESTRUCTIVE_CALLS=0
    adk_routine_asset_lock_owned() { return 0; }
    adk_routine_asset_candidate_drain_authority_exists() { return 1; }
    _adk_active_txn() { printf '%s\n' "$ACTIVE_TXN"; }
    adk_routine_asset_transaction_phase() { printf 'staging\n'; }
    _forward_migration_marker_path() {
        printf '%s/forward-migration-applied.json\n' "$1"
    }
    _forward_migration_candidate_sha() { return 1; }
    _rollback_release_binary() { DESTRUCTIVE_CALLS=$((DESTRUCTIVE_CALLS + 1)); }
    adk_rollback_routine_asset_transaction() {
        DESTRUCTIVE_CALLS=$((DESTRUCTIVE_CALLS + 1))
    }
    adk_commit_routine_asset_transaction_forward() {
        DESTRUCTIVE_CALLS=$((DESTRUCTIVE_CALLS + 1))
    }
    _restart_pre_promotion_release() {
        DESTRUCTIVE_CALLS=$((DESTRUCTIVE_CALLS + 1))
    }
    _cleanup_owned_pg_tunnel_preflight() { :; }
    _rollback_pg_tunnel_migration() { :; }
    _finalize_detached_helper() { :; }
    adk_release_routine_asset_lock() { :; }

    set +e
    _cleanup_on_exit 143 >/dev/null 2>&1
    cleanup_status=$?
    set -e
    [ "$cleanup_status" = 1 ] \
        && [ "$DESTRUCTIVE_CALLS" = 0 ] \
        && [ -f "$STAGED_BINARY" ] \
        && [ -f "$ACTIVE_TXN/forward-migration-applied.json" ] \
        || exit 126
) || fail 'invalid forward marker fell through to destructive rollback'

# A peer invocation is the synchronous ownership boundary for its uploaded
# inbox. On Darwin it must remain in the SSH foreground until the script body
# completes instead of taking the top-level nohup exit.
(
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-peer-no-detach.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    mkdir -p "$TEST_ROOT/bin" "$TEST_ROOT/home"
    printf '#!/usr/bin/env bash\nprintf "Darwin\\n"\n' > "$TEST_ROOT/bin/uname"
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        'printf "nohup-called\n" > "$AGENTDESK_TEST_NOHUP_MARKER"' \
        > "$TEST_ROOT/bin/nohup"
    chmod +x "$TEST_ROOT/bin/uname" "$TEST_ROOT/bin/nohup"
    sed -n '1,/^fi$/p' "$DEPLOY_RELEASE" > "$TEST_ROOT/probe.sh"
    printf 'printf "peer-complete\\n" > "$AGENTDESK_TEST_COMPLETE_MARKER"\n' \
        >> "$TEST_ROOT/probe.sh"
    AGENTDESK_DEPLOY_PEER_INVOCATION=1 \
    AGENTDESK_DEPLOY_NO_DETACH=0 \
    AGENTDESK_TEST_NOHUP_MARKER="$TEST_ROOT/nohup" \
    AGENTDESK_TEST_COMPLETE_MARKER="$TEST_ROOT/complete" \
    HOME="$TEST_ROOT/home" \
    PATH="$TEST_ROOT/bin:$PATH" \
        bash "$TEST_ROOT/probe.sh"
    [ -f "$TEST_ROOT/complete" ] && [ ! -e "$TEST_ROOT/nohup" ] \
        || exit 112
) || fail 'Darwin peer deploy detached before synchronous SSH completion'

# Exercise the actual peer caller seam. The final fake SSH remains blocked after
# the remote claim starts; the caller must remain live and must not run inbox
# cleanup until remote deploy completion releases that SSH call.
(
    eval "$(extract_function _deploy_to_one_peer)"
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-peer-ssh-sync.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    ADK_REL="$TEST_ROOT/local-release"
    mkdir -p "$ADK_REL/routines" "$ADK_REL/routine-helpers"
    DEPLOY_SSH_CONNECT_TIMEOUT=5
    ROUTINE_ASSET_INCOMING=''
    REPO="$TEST_ROOT/repo"
    DEPLOY_LOCK_FILE="$ADK_REL/runtime/deploy-release.lock"
    printf '0\n' > "$TEST_ROOT/ssh-count"
    adk_validate_peer_destination() { return 0; }
    adk_validate_peer_runtime_root() { return 0; }
    _adk_validate_peer_timeout() { return 0; }
    _deploy_peer_env_prelude() { printf 'AGENTDESK_DEPLOY_PEER_INVOCATION=1'; }
    adk_validate_quickjs_routine_tree() { return 0; }
    adk_validate_routine_helper_surface() { return 0; }
    adk_prepare_peer_asset_incoming() {
        mkdir -p "$TEST_ROOT/remote/incoming"
        printf '%s\n' "$TEST_ROOT/remote/incoming"
    }
    adk_rsync_peer_asset_surface() { return 0; }
    adk_remove_peer_asset_incoming() {
        [ -f "$TEST_ROOT/remote-complete" ] || return 91
        : > "$TEST_ROOT/inbox-cleaned"
    }
    ssh() {
        local count
        count="$(<"$TEST_ROOT/ssh-count")"
        count=$((count + 1))
        printf '%s\n' "$count" > "$TEST_ROOT/ssh-count"
        case "$count" in
            1) return 0 ;;
            2) printf '%s\n' "$TEST_ROOT/remote" ;;
            3) printf '%s\n' "$TEST_ROOT/remote/runtime/deploy-release.lock" ;;
            4)
                : > "$TEST_ROOT/remote-claim-started"
                while [ ! -f "$TEST_ROOT/release-remote" ]; do
                    sleep 0.05
                done
                : > "$TEST_ROOT/remote-complete"
                ;;
            *) return 92 ;;
        esac
    }

    _deploy_to_one_peer fake-peer --skip-build \
        >"$TEST_ROOT/peer.log" 2>&1 &
    peer_pid=$!
    attempts=0
    while [ ! -f "$TEST_ROOT/remote-claim-started" ] && [ "$attempts" -lt 100 ]; do
        sleep 0.05
        attempts=$((attempts + 1))
    done
    [ -f "$TEST_ROOT/remote-claim-started" ] \
        && kill -0 "$peer_pid" 2>/dev/null \
        && [ ! -e "$TEST_ROOT/inbox-cleaned" ] \
        || exit 127
    : > "$TEST_ROOT/release-remote"
    wait "$peer_pid"
    [ -f "$TEST_ROOT/remote-complete" ] \
        && [ -f "$TEST_ROOT/inbox-cleaned" ] \
        || exit 128
) || fail 'peer SSH returned or cleaned its inbox before remote deploy completion'

# Capture the exact process instance started by the candidate generation, then
# prove rollback drain waits for launchd, that PID/identity, and the HTTP port.
(
    eval "$(extract_function _release_current_service_pid)"
    eval "$(extract_function _capture_release_candidate_process)"

    PLIST_REL='com.agentdesk.release'
    LAUNCHD_DOMAIN='gui/test'
    ADK_REL='/tmp/adk-candidate-capture-test'
    LOCK_FILE="$ADK_REL/runtime/dcserver.lock"
    RELEASE_CANDIDATE_PID=''
    RELEASE_CANDIDATE_IDENTITY=''
    RELEASE_CANDIDATE_CAPTURED=0
    launchctl() {
        [ "$1" = print ] || return 1
        printf '    pid = 4321\n'
    }
    adk_process_identity() {
        [ "$1" = 4321 ] || return 1
        printf 'candidate-start-identity\n'
    }
    kill() { [ "$1" = -0 ] && [ "$2" = 4321 ]; }
    sleep() { :; }

    _capture_release_candidate_process 1
    [ "$RELEASE_CANDIDATE_CAPTURED" = 1 ] \
        && [ "$RELEASE_CANDIDATE_PID" = 4321 ] \
        && [ "$RELEASE_CANDIDATE_IDENTITY" = candidate-start-identity ] \
        || exit 113
) || fail 'post-bootstrap candidate PID/identity was not captured exactly'

(
    # shellcheck source=../scripts/routine-asset-surface.sh
    . "$ASSET_SURFACE"
    eval "$(extract_function _release_candidate_process_is_alive)"
    RELEASE_CANDIDATE_CAPTURED=1
    RELEASE_CANDIDATE_PID=4321
    RELEASE_CANDIDATE_IDENTITY='candidate-start-identity'
    kill() { [ "$1" = -0 ] && [ "$2" = 4321 ]; }
    adk_process_identity() { return 1; }
    _release_candidate_process_is_alive || exit 131
) || fail 'release drain treated unavailable PID identity as proven death'

(
    eval "$(extract_function _stop_and_drain_release_candidate)"

    PLIST_REL='com.agentdesk.release'
    LAUNCHD_DOMAIN='gui/test'
    ADK_REL='/tmp/adk-release-candidate-drain-test'
    ROUTINE_ASSET_TXN='txn'
    RELEASE_CANDIDATE_CAPTURED=1
    RELEASE_CANDIDATE_PID=4321
    RELEASE_CANDIDATE_IDENTITY='candidate-start-identity'
    JOB_LOADED=1
    PID_ALIVE=1
    PORT_OPEN=1
    DRAIN_MARKER=1
    DRAIN_WAITS=0
    _launchd_domain() { printf 'gui/test\n'; }
    _release_launchd_job_is_loaded() { [ "$JOB_LOADED" = 1 ]; }
    _release_candidate_process_is_alive() { [ "$PID_ALIVE" = 1 ]; }
    _release_candidate_port_refuses_connections() { [ "$PORT_OPEN" = 0 ]; }
    adk_routine_asset_candidate_drain_authority_exists() {
        [ "$DRAIN_MARKER" = 1 ]
    }
    adk_clear_routine_asset_candidate_drain_authority() {
        [ "$JOB_LOADED" = 0 ] && [ "$PID_ALIVE" = 0 ] \
            && [ "$PORT_OPEN" = 0 ] || return 91
        DRAIN_MARKER=0
    }
    launchctl() {
        [ "$1" = bootout ] || return 1
        JOB_LOADED=0
    }
    tmux() { :; }
    sleep() {
        DRAIN_WAITS=$((DRAIN_WAITS + 1))
        if [ "$DRAIN_WAITS" = 1 ]; then
            PID_ALIVE=0
        else
            PORT_OPEN=0
        fi
    }

    _stop_and_drain_release_candidate
    [ "$DRAIN_WAITS" = 2 ] \
        && [ "$JOB_LOADED" = 0 ] \
        && [ "$PID_ALIVE" = 0 ] \
        && [ "$PORT_OPEN" = 0 ] \
        && [ "$DRAIN_MARKER" = 0 ] \
        || exit 114
) || fail 'rollback restored before the candidate process and port drained'

# A fresh forward-recovery invocation must rehydrate the exact prior candidate
# identity from disk. Job disappearance and a closed port alone are not enough
# while that exact PID instance is still alive.
(
    # shellcheck source=../scripts/routine-asset-surface.sh
    . "$ASSET_SURFACE"
    eval "$(extract_function _release_candidate_process_is_alive)"
    eval "$(extract_function _stop_and_drain_release_candidate)"
    eval "$(extract_function _resume_release_candidate_drain_authority)"

    ADK_REL='/tmp/adk-release-resume-drain-test'
    PLIST_REL='com.agentdesk.release'
    LAUNCHD_DOMAIN='gui/test'
    REL_PORT=8791
    PID_ALIVE=1
    DRAIN_MARKER=1
    DRAIN_WAITS=0
    adk_routine_asset_candidate_drain_authority_exists() {
        [ "$DRAIN_MARKER" = 1 ]
    }
    adk_routine_asset_candidate_drain_authority_value() {
        case "$3" in
            pid) printf '7878\n' ;;
            identity) printf 'persisted-candidate-instance\n' ;;
            port) printf '8791\n' ;;
            supervisor) printf 'gui/test/com.agentdesk.release\n' ;;
            *) return 91 ;;
        esac
    }
    _launchd_domain() { printf 'gui/test\n'; }
    _resolve_release_server_port() { printf '8791\n'; }
    _release_launchd_job_is_loaded() { return 1; }
    _release_candidate_port_refuses_connections() { return 0; }
    adk_process_identity() {
        [ "$1" = 7878 ] || return 1
        printf 'persisted-candidate-instance\n'
    }
    kill() {
        [ "$1" = -0 ] && [ "$2" = 7878 ] && [ "$PID_ALIVE" = 1 ]
    }
    launchctl() { [ "$1" = bootout ]; }
    tmux() { :; }
    sleep() {
        DRAIN_WAITS=$((DRAIN_WAITS + 1))
        PID_ALIVE=0
    }
    adk_clear_routine_asset_candidate_drain_authority() {
        [ "$PID_ALIVE" = 0 ] || return 92
        DRAIN_MARKER=0
    }

    _resume_release_candidate_drain_authority txn
    [ "$DRAIN_WAITS" = 1 ] \
        && [ "$DRAIN_MARKER" = 0 ] \
        && [ "$RELEASE_CANDIDATE_CAPTURED" = 0 ] \
        || exit 130
) || fail 'forward recovery bypassed the persisted exact candidate identity'

(
    eval "$(extract_function _release_candidate_port_refuses_connections)"
    CURL_STATUS=28
    curl() { return "$CURL_STATUS"; }
    if _release_candidate_port_refuses_connections; then
        exit 124
    fi
    CURL_STATUS=7
    _release_candidate_port_refuses_connections || exit 125
) || fail 'release port drain accepted a timeout instead of exact connection refusal'

DRAIN_LINE="$(awk '
    /^_rollback_release_binary[(][)] [{]$/ { inside = 1 }
    inside && /_stop_and_drain_release_candidate/ { print NR; exit }
' "$DEPLOY_RELEASE")"
RESTORE_LINE="$(awk '
    /^_rollback_release_binary[(][)] [{]$/ { inside = 1 }
    inside && /mv -f "\$rel_backup" "\$rel_binary"/ { print NR; exit }
' "$DEPLOY_RELEASE")"
[ -n "$DRAIN_LINE" ] && [ -n "$RESTORE_LINE" ] \
    && [ "$DRAIN_LINE" -lt "$RESTORE_LINE" ] \
    || fail 'candidate drain is not wired before old binary restore'

# Immutable flags are observed, mutated, and verified as part of the same
# generation transaction. An apparently successful chflags with a stale stat
# result must fail closed; rollback restores the exact prior state.
(
    for name in \
        _release_host_uses_immutable_flags \
        _release_binary_immutable_state \
        _snapshot_release_binary_immutable_flag \
        _set_release_binary_immutable_state \
        _restore_release_binary_immutable_flag; do
        eval "$(extract_function "$name")"
    done
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-release-flags.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    REL_BINARY="$TEST_ROOT/agentdesk"
    printf 'binary\n' > "$REL_BINARY"
    LIVE_FLAG='uchg'
    STALE_VERIFY=0
    uname() { printf 'Darwin\n'; }
    stat() { printf '%s\n' "$LIVE_FLAG"; }
    chflags() {
        if [ "$STALE_VERIFY" = 1 ]; then
            return 0
        fi
        case "$1" in
            uchg) LIVE_FLAG='uchg' ;;
            nouchg) LIVE_FLAG='-' ;;
            *) return 1 ;;
        esac
    }

    _snapshot_release_binary_immutable_flag
    [ "$RELEASE_BINARY_OLD_IMMUTABLE" = 1 ] || exit 115
    _set_release_binary_immutable_state "$REL_BINARY" 0
    [ "$LIVE_FLAG" = '-' ] || exit 116
    STALE_VERIFY=1
    if _set_release_binary_immutable_state "$REL_BINARY" 1; then
        exit 117
    fi
    STALE_VERIFY=0
    _restore_release_binary_immutable_flag
    [ "$LIVE_FLAG" = uchg ] || exit 118
) || fail 'release immutable flag mutation was not verified or restored transactionally'

PROTECT_LINE="$(rg -n '^if ! _set_release_binary_immutable_state "\$REL_BINARY" 1; then$' "$DEPLOY_RELEASE" | tail -1 | cut -d: -f1)"
COMMIT_INTENT_LINE="$(rg -n '^if ! adk_mark_routine_asset_transaction_committing ' "$DEPLOY_RELEASE" | tail -1 | cut -d: -f1)"
DEPLOY_OK_LINE="$(rg -n '^DEPLOY_OK=1$' "$DEPLOY_RELEASE" | tail -1 | cut -d: -f1)"
[ -n "$PROTECT_LINE" ] && [ -n "$COMMIT_INTENT_LINE" ] && [ -n "$DEPLOY_OK_LINE" ] \
    && [ "$PROTECT_LINE" -lt "$COMMIT_INTENT_LINE" ] \
    && [ "$PROTECT_LINE" -lt "$DEPLOY_OK_LINE" ] \
    || fail 'immutable verification is not before commit intent/disarm'

# A release from before the helper split has bin+routines+release-source but no
# helper tree. Normalize that absence explicitly before stop, bind it into the
# rollback metadata, promote the new pair, and prove the metadata still verifies
# against the sentinel tree after it moves to routine-helpers.old.
(
    # shellcheck source=../scripts/routine-asset-surface.sh
    . "$ASSET_SURFACE"
    load_release_generation_functions

    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-release-legacy-helpers.XXXXXX")"
    cleanup_legacy_test() {
        adk_release_routine_asset_lock 2>/dev/null || true
        rm -rf "$TEST_ROOT"
    }
    trap cleanup_legacy_test EXIT
    chflags() { :; }

    ADK_REL="$TEST_ROOT/release"
    REPO="$TEST_ROOT/repo"
    DEPLOY_LOCK_FILE="$ADK_REL/runtime/deploy-release.lock"
    LEGACY_ROUTINE_HELPERS_SENTINEL_NAME='.agentdesk-legacy-empty-v1'
    LEGACY_SHA='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
    LEGACY_MIGRATION='0100_legacy_release.sql'
    mkdir -p "$ADK_REL/bin" "$ADK_REL/routines" "$ADK_REL/runtime" \
        "$REPO/routines" "$REPO/routine-helpers"
    printf '#!/usr/bin/env bash\nexit 0\n' > "$ADK_REL/bin/agentdesk"
    chmod +x "$ADK_REL/bin/agentdesk"
    printf 'agentdesk.routines.register({ tick() { return { action: "complete" }; } });\n' \
        > "$ADK_REL/routines/legacy.js"
    printf 'agentdesk.routines.register({ tick() { return { action: "complete" }; } });\n' \
        > "$REPO/routines/current.js"
    for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
        mkdir -p "$REPO/routine-helpers/$(dirname "$helper_ref")"
        printf 'helper:%s\n' "$helper_ref" > "$REPO/routine-helpers/$helper_ref"
    done
    printf '{"source_git_sha":"%s","latest_postgres_migration":"%s"}\n' \
        "$LEGACY_SHA" "$LEGACY_MIGRATION" \
        > "$ADK_REL/runtime/release-source.json"
    CANDIDATE="$TEST_ROOT/candidate-agentdesk"
    printf '%s\n' \
        '#!/usr/bin/env bash' \
        '[ "${1:-}" = validate-routines ]' \
        '[ "${2:-}" = --root ]' \
        '[ -d "${3:-}" ]' \
        '[ "${4:-}" = --runtime-root ]' \
        '[ "${3:-}" = "${5:-}/routines" ]' \
        '[ -d "${5:-}/routine-helpers" ]' \
        > "$CANDIDATE"
    chmod +x "$CANDIDATE"

    adk_acquire_routine_asset_lock "$DEPLOY_LOCK_FILE" 0
    ROUTINE_ASSET_TXN="$(
        adk_begin_routine_asset_transaction "$ADK_REL" "$DEPLOY_LOCK_FILE"
    )"
    adk_stage_routines "$REPO" "$ADK_REL" "$ROUTINE_ASSET_TXN" >/dev/null
    adk_stage_routine_helpers "$REPO" "$ADK_REL" "$ROUTINE_ASSET_TXN" >/dev/null
    _strip_legacy_helper_sentinel_from_staged_generation \
        "$ROUTINE_ASSET_TXN/staged/release-root/routine-helpers"
    adk_validate_staged_routine_asset_transaction \
        "$ADK_REL" "$ROUTINE_ASSET_TXN" "$CANDIDATE"

    REL_BINARY="$ADK_REL/bin/agentdesk"
    REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
    REL_BINARY_BACKUP_META="$REL_BINARY_BACKUP.meta"
    _prepare_release_rollback_generation
    [ "$REL_ROLLBACK_MATERIAL_MODE" = capture ] \
        || exit 101
    [ -f "$ADK_REL/routine-helpers/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" ] \
        && [ -f "$ROUTINE_ASSET_TXN/rollback-backup.meta.preflight" ] \
        || exit 102

    cp -p "$REL_BINARY" "$REL_BINARY_BACKUP.tmp"
    _write_rollback_backup_metadata \
        "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP_META.tmp" \
        "$ROUTINE_ASSET_TXN"
    mv "$REL_BINARY_BACKUP.tmp" "$REL_BINARY_BACKUP"
    mv "$REL_BINARY_BACKUP_META.tmp" "$REL_BINARY_BACKUP_META"
    adk_promote_routine_asset_transaction \
        "$ADK_REL" "$ROUTINE_ASSET_TXN" "$CANDIDATE"

    [ -f "$ADK_REL/routine-helpers.old/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" ] \
        && [ ! -e "$ADK_REL/routine-helpers/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" ] \
        || exit 103
    [ "$(_rollback_backup_latest_migration_name)" = "$LEGACY_MIGRATION" ] \
        || exit 104
    EXPECTED_HELPERS_SHA="$(adk_sha256_tree "$ADK_REL/routine-helpers.old")"
    python3 - "$REL_BINARY_BACKUP_META" "$EXPECTED_HELPERS_SHA" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
raise SystemExit(0 if data.get("routine_helpers_sha256") == sys.argv[2] else 1)
PY
    adk_rollback_routine_asset_transaction "$ADK_REL" "$ROUTINE_ASSET_TXN"
    [ -f "$ADK_REL/routine-helpers/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" ] \
        || exit 105

    # A retry stages the preserved sentinel as operator data; the reserved-file
    # scrub must remove it before exact candidate validation/promotion.
    ROUTINE_ASSET_TXN="$(
        adk_begin_routine_asset_transaction "$ADK_REL" "$DEPLOY_LOCK_FILE"
    )"
    adk_stage_routines "$REPO" "$ADK_REL" "$ROUTINE_ASSET_TXN" >/dev/null
    adk_stage_routine_helpers "$REPO" "$ADK_REL" "$ROUTINE_ASSET_TXN" >/dev/null
    _strip_legacy_helper_sentinel_from_staged_generation \
        "$ROUTINE_ASSET_TXN/staged/release-root/routine-helpers"
    [ ! -e "$ROUTINE_ASSET_TXN/staged/release-root/routine-helpers/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" ] \
        || exit 106
    adk_validate_staged_routine_asset_transaction \
        "$ADK_REL" "$ROUTINE_ASSET_TXN" "$CANDIDATE"
    adk_rollback_routine_asset_transaction "$ADK_REL" "$ROUTINE_ASSET_TXN"
) || fail 'legacy helper absence was not normalized into an exact rollback generation'

# Invalid rollback material must fail while the old service is untouched. This
# models the main-flow guard: stop is reachable only after preflight succeeds.
(
    # shellcheck source=../scripts/routine-asset-surface.sh
    . "$ASSET_SURFACE"
    load_release_generation_functions
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-release-prev-preflight.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    chflags() { :; }
    ADK_REL="$TEST_ROOT/release"
    ROUTINE_ASSET_TXN="$ADK_REL/runtime/routine-assets.txn.ABC123"
    LEGACY_ROUTINE_HELPERS_SENTINEL_NAME='.agentdesk-legacy-empty-v1'
    mkdir -p "$ADK_REL/bin" "$ADK_REL/routines" "$ROUTINE_ASSET_TXN"
    printf 'old\n' > "$ADK_REL/bin/agentdesk"
    printf 'stale backup\n' > "$ADK_REL/bin/agentdesk.prev"
    printf 'legacy\n' > "$ADK_REL/routines/legacy.js"
    printf '%s\n' \
        '{"source_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","latest_postgres_migration":"0100_legacy_release.sql"}' \
        > "$ADK_REL/runtime/release-source.json"
    REL_BINARY="$ADK_REL/bin/agentdesk"
    REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
    REL_BINARY_BACKUP_META="$REL_BINARY_BACKUP.meta"
    STOP_CALLS=0
    stop_release() { STOP_CALLS=$((STOP_CALLS + 1)); }
    if _prepare_release_rollback_generation >/dev/null 2>&1; then
        stop_release
    fi
    [ "$STOP_CALLS" = 0 ] && [ ! -e "$ADK_REL/routine-helpers" ] \
        || exit 107

    # Once release-source explicitly binds a helper generation, absence is
    # corruption rather than legacy compatibility and must never get a sentinel.
    rm -f "$REL_BINARY_BACKUP"
    printf '%s\n' \
        '{"source_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","latest_postgres_migration":"0100_legacy_release.sql","routine_helpers_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}' \
        > "$ADK_REL/runtime/release-source.json"
    if _prepare_release_rollback_generation >/dev/null 2>&1; then
        stop_release
    fi
    [ "$STOP_CALLS" = 0 ] && [ ! -e "$ADK_REL/routine-helpers" ] \
        || exit 109
) || fail 'invalid .prev state reached release stop or fabricated a helper generation'

# Metadata serialization itself is part of preflight. Inject its failure on the
# same no-helper/no-prev legacy shape and prove the stop edge is unreachable.
(
    # shellcheck source=../scripts/routine-asset-surface.sh
    . "$ASSET_SURFACE"
    load_release_generation_functions
    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/adk-release-meta-preflight.XXXXXX")"
    trap 'rm -rf "$TEST_ROOT"' EXIT
    chflags() { :; }
    ADK_REL="$TEST_ROOT/release"
    ROUTINE_ASSET_TXN="$ADK_REL/runtime/routine-assets.txn.ABC123"
    LEGACY_ROUTINE_HELPERS_SENTINEL_NAME='.agentdesk-legacy-empty-v1'
    mkdir -p "$ADK_REL/bin" "$ADK_REL/routines" "$ROUTINE_ASSET_TXN"
    printf 'old\n' > "$ADK_REL/bin/agentdesk"
    printf 'legacy\n' > "$ADK_REL/routines/legacy.js"
    printf '%s\n' \
        '{"source_git_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","latest_postgres_migration":"0100_legacy_release.sql"}' \
        > "$ADK_REL/runtime/release-source.json"
    REL_BINARY="$ADK_REL/bin/agentdesk"
    REL_BINARY_BACKUP="$ADK_REL/bin/agentdesk.prev"
    REL_BINARY_BACKUP_META="$REL_BINARY_BACKUP.meta"
    STOP_CALLS=0
    _write_rollback_backup_metadata() { return 88; }
    stop_release() { STOP_CALLS=$((STOP_CALLS + 1)); }
    if _prepare_release_rollback_generation >/dev/null 2>&1; then
        stop_release
    fi
    [ "$STOP_CALLS" = 0 ] \
        && [ "$REL_ROLLBACK_MATERIAL_MODE" = '' ] \
        && [ -f "$ADK_REL/routine-helpers/$LEGACY_ROUTINE_HELPERS_SENTINEL_NAME" ] \
        || exit 108
) || fail 'rollback metadata preflight failure reached release stop'

PREPARE_LINE="$(rg -n '^if ! _prepare_release_rollback_generation; then$' "$DEPLOY_RELEASE" | cut -d: -f1)"
STOP_LINE="$(rg -n '^if ! _stop_release_for_promotion; then$' "$DEPLOY_RELEASE" | cut -d: -f1)"
[ -n "$PREPARE_LINE" ] && [ -n "$STOP_LINE" ] \
    && [ "$PREPARE_LINE" -lt "$STOP_LINE" ] \
    || fail 'rollback generation preflight is not wired before release stop'

# Cleanup must use strict on-disk token ownership, never merely a non-empty
# inherited lock-directory variable. This guards an SSH sender/receiver race.
rg -q 'if adk_routine_asset_lock_owned "\$DEPLOY_LOCK_FILE"; then' "$DEPLOY_RELEASE" \
    || fail 'cleanup does not verify routine-asset lock ownership'
if rg -q '\[ -z "\$\{ADK_ROUTINE_ASSET_LOCK_DIR:-\}" \] \|\| asset_lock_held=1' "$DEPLOY_RELEASE"; then
    fail 'cleanup still treats lock-directory presence as ownership'
fi

echo 'deploy release service recovery tests passed'
