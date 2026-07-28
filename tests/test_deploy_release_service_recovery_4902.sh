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
