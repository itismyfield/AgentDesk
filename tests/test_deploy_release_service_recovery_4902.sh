#!/usr/bin/env bash
# shellcheck disable=SC2034,SC2329 # Functions/variables are invoked through extracted eval bodies.
# Targeted lifecycle coverage for #4902: arming must precede release bootout,
# and recovery may start the previous pair only after the old service stopped.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_RELEASE="$SCRIPT_DIR/../scripts/deploy-release.sh"

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

# The stop helper must arm recovery before invoking bootout and refuse a live
# promotion until launchd/PID state confirms the old release is gone.
(
    eval "$(extract_function _release_launchd_job_is_loaded)"
    eval "$(extract_function _release_old_process_is_alive)"
    eval "$(extract_function _release_service_is_stopped)"
    eval "$(extract_function _stop_release_for_promotion)"

    PLIST_REL='com.agentdesk.release'
    LAUNCHD_DOMAIN='gui/test'
    OLD_PID=''
    ADK_REL='/tmp/adk-release-service-recovery-test'
    ROLLBACK_ARMED=0
    RELEASE_SERVICE_RECOVERY_ARMED=0
    RELEASE_SERVICE_STOP_CONFIRMED=0
    RELEASE_SERVICE_RESTART_SAFE=0
    SERVICE_ACTIVE=1
    BOOTOUT_SAW_ARMED=0
    _pre_promotion_release_restart_is_safe() { return 0; }
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

# Cleanup must use strict on-disk token ownership, never merely a non-empty
# inherited lock-directory variable. This guards an SSH sender/receiver race.
rg -q 'if adk_routine_asset_lock_owned "\$DEPLOY_LOCK_FILE"; then' "$DEPLOY_RELEASE" \
    || fail 'cleanup does not verify routine-asset lock ownership'
if rg -q '\[ -z "\$\{ADK_ROUTINE_ASSET_LOCK_DIR:-\}" \] \|\| asset_lock_held=1' "$DEPLOY_RELEASE"; then
    fail 'cleanup still treats lock-directory presence as ownership'
fi

echo 'deploy release service recovery tests passed'
