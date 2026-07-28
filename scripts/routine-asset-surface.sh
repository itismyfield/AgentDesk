#!/usr/bin/env bash
# Shared, crash-recoverable deployment primitives for routine asset surfaces.

ADK_REQUIRED_ROUTINE_HELPER_REFS=(
    "monitoring/local_worktree_inventory.js"
    "monitoring/daily_log_digest.py"
    "monitoring/log_digest_issue_drafts.py"
    "monitoring/weekly_churn_audit.py"
)
ADK_LEGACY_ROUTINE_HELPER_REFS=("${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}")
ADK_ROUTINE_ASSET_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ADK_QUICKJS_VALIDATOR="${ADK_QUICKJS_VALIDATOR:-$ADK_ROUTINE_ASSET_SCRIPT_DIR/validate-quickjs-routines.py}"
ADK_ROUTINE_ASSET_LOCK_DIR="${ADK_ROUTINE_ASSET_LOCK_DIR:-}"
ADK_ROUTINE_ASSET_LOCK_TOKEN="${ADK_ROUTINE_ASSET_LOCK_TOKEN:-}"

_adk_path_mode() {
    local path="$1"
    local mode

    if mode="$(stat -f '%Lp' "$path" 2>/dev/null)"; then
        printf '%s\n' "$mode"
        return 0
    fi
    stat -c '%a' "$path" 2>/dev/null
}

_adk_assert_no_symlink_tree() {
    local root="$1"
    local parent
    local first_link

    parent="$(dirname "$root")" || return 1
    if [ -L "$parent" ] || [ -L "$root" ]; then
        echo "Refusing symlinked routine asset ancestor: $root" >&2
        return 1
    fi
    [ -d "$root" ] || {
        echo "Required routine asset directory missing: $root" >&2
        return 1
    }
    if ! first_link="$(find "$root" -type l -print -quit 2>/dev/null)"; then
        echo "Could not inspect routine asset symlinks: $root" >&2
        return 1
    fi
    if [ -n "$first_link" ]; then
        echo "Refusing symlink in routine asset surface: $first_link" >&2
        return 1
    fi
}

adk_validate_routine_helper_surface() {
    local root="$1"
    local helper_ref

    _adk_assert_no_symlink_tree "$root" || return 1
    for helper_ref in "${ADK_REQUIRED_ROUTINE_HELPER_REFS[@]}"; do
        if [ ! -f "$root/$helper_ref" ] || [ -L "$root/$helper_ref" ]; then
            echo "Required regular routine helper missing: $root/$helper_ref" >&2
            return 1
        fi
    done
}

adk_validate_quickjs_routine_tree() {
    local root="$1"
    local python_bin="${PYTHON:-python3}"

    _adk_assert_no_symlink_tree "$root" || return 1
    if [ ! -f "$ADK_QUICKJS_VALIDATOR" ] || [ -L "$ADK_QUICKJS_VALIDATOR" ]; then
        echo "QuickJS lexical validator missing: $ADK_QUICKJS_VALIDATOR" >&2
        return 1
    fi
    if ! command -v "$python_bin" >/dev/null 2>&1; then
        echo "Python 3 is required for bounded QuickJS routine validation: $python_bin" >&2
        return 1
    fi
    "$python_bin" "$ADK_QUICKJS_VALIDATOR" "$root"
}

adk_validate_repo_routine_assets() {
    local repo_root="$1"

    adk_validate_quickjs_routine_tree "$repo_root/routines" \
        && adk_validate_routine_helper_surface "$repo_root/routine-helpers"
}

adk_remove_legacy_routine_helpers() {
    local routines_root="$1"
    local helper_ref

    for helper_ref in "${ADK_LEGACY_ROUTINE_HELPER_REFS[@]}"; do
        rm -f "$routines_root/$helper_ref" || return 1
    done
}

# One mkdir lock is shared by deploy-release.sh, deploy.sh, and install.sh.
# The owner token prevents one process from releasing another process's lock;
# an atomic stale-directory rename makes crash recovery race-safe.
adk_acquire_routine_asset_lock() {
    local lock_file="$1"
    local timeout_seconds="${2:-1800}"
    local lock_dir="${lock_file}.d"
    local waited=0
    local missing_owner_waits=0
    local owner_pid
    local stale_dir
    local token

    if [ -n "$ADK_ROUTINE_ASSET_LOCK_DIR" ]; then
        [ "$ADK_ROUTINE_ASSET_LOCK_DIR" = "$lock_dir" ] || {
            echo "A different routine asset lock is already held: $ADK_ROUTINE_ASSET_LOCK_DIR" >&2
            return 1
        }
        return 0
    fi
    case "$timeout_seconds" in
        ''|*[!0-9]*) return 1 ;;
    esac
    mkdir -p "$(dirname "$lock_file")" || return 1
    if [ -L "$lock_dir" ] || [ -L "$(dirname "$lock_dir")" ]; then
        echo "Refusing symlinked routine asset lock path: $lock_dir" >&2
        return 1
    fi
    token="$$.${RANDOM:-0}.$(date +%s)"

    while ! mkdir "$lock_dir" 2>/dev/null; do
        if [ -L "$lock_dir" ] || [ ! -d "$lock_dir" ]; then
            echo "Invalid routine asset lock path: $lock_dir" >&2
            return 1
        fi
        owner_pid="$(sed -n '1p' "$lock_dir/pid" 2>/dev/null || true)"
        case "$owner_pid" in
            ''|*[!0-9]*)
                missing_owner_waits=$((missing_owner_waits + 1))
                ;;
            *)
                missing_owner_waits=0
                if ! kill -0 "$owner_pid" 2>/dev/null; then
                    stale_dir="${lock_dir}.stale.${token}"
                    if mv "$lock_dir" "$stale_dir" 2>/dev/null; then
                        rm -rf "$stale_dir" || return 1
                        continue
                    fi
                fi
                ;;
        esac
        if [ "$missing_owner_waits" -ge 3 ]; then
            stale_dir="${lock_dir}.stale.${token}"
            if mv "$lock_dir" "$stale_dir" 2>/dev/null; then
                rm -rf "$stale_dir" || return 1
                missing_owner_waits=0
                continue
            fi
        fi
        if [ "$waited" -ge "$timeout_seconds" ]; then
            echo "Timed out waiting for routine asset deploy lock: $lock_file" >&2
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done

    if ! printf '%s\n' "$$" > "$lock_dir/pid" \
      || ! printf '%s\n' "$token" > "$lock_dir/token"; then
        rm -f "$lock_dir/pid" "$lock_dir/token" 2>/dev/null || true
        rmdir "$lock_dir" 2>/dev/null || true
        return 1
    fi
    ADK_ROUTINE_ASSET_LOCK_DIR="$lock_dir"
    ADK_ROUTINE_ASSET_LOCK_TOKEN="$token"
}

adk_release_routine_asset_lock() {
    local current_token

    [ -n "$ADK_ROUTINE_ASSET_LOCK_DIR" ] || return 0
    if [ -L "$ADK_ROUTINE_ASSET_LOCK_DIR" ] || [ ! -d "$ADK_ROUTINE_ASSET_LOCK_DIR" ]; then
        return 1
    fi
    current_token="$(sed -n '1p' "$ADK_ROUTINE_ASSET_LOCK_DIR/token" 2>/dev/null || true)"
    [ "$current_token" = "$ADK_ROUTINE_ASSET_LOCK_TOKEN" ] || return 1
    rm -f "$ADK_ROUTINE_ASSET_LOCK_DIR/pid" "$ADK_ROUTINE_ASSET_LOCK_DIR/token" \
        || return 1
    rmdir "$ADK_ROUTINE_ASSET_LOCK_DIR" || return 1
    ADK_ROUTINE_ASSET_LOCK_DIR=""
    ADK_ROUTINE_ASSET_LOCK_TOKEN=""
}

_adk_state_dir() {
    printf '%s/runtime\n' "$1"
}

_adk_active_marker() {
    printf '%s/routine-assets.active\n' "$(_adk_state_dir "$1")"
}

_adk_write_atomic_file() {
    local path="$1"
    local value="$2"
    local tmp="${path}.tmp.$$.$RANDOM"

    printf '%s\n' "$value" > "$tmp" || return 1
    mv "$tmp" "$path"
}

_adk_write_phase() {
    _adk_write_atomic_file "$1/phase" "$2"
}

_adk_read_txn_phase() {
    local txn_root="$1"
    local phase_file="$txn_root/phase"
    local phase

    [ -f "$phase_file" ] && [ ! -L "$phase_file" ] || return 1
    phase="$(cat "$phase_file")" || return 1
    case "$phase" in
        staging|armed|promoted|rolling-back|rolled-back|committing|committed) ;;
        *) echo "Invalid routine asset transaction phase: $phase" >&2; return 1 ;;
    esac
    printf '%s\n' "$phase"
}

_adk_validate_txn_path() {
    local runtime_root="$1"
    local txn_root="$2"
    local state_dir
    local txn_parent
    local txn_name

    state_dir="$(_adk_state_dir "$runtime_root")" || return 1
    txn_parent="$(dirname "$txn_root")" || return 1
    txn_name="$(basename "$txn_root")" || return 1
    [ "$txn_parent" = "$state_dir" ] || return 1
    case "$txn_name" in
        routine-assets.txn.*) ;;
        *) return 1 ;;
    esac
    case "${txn_name#routine-assets.txn.}" in
        ''|*[!A-Za-z0-9]*) return 1 ;;
    esac
    [ ! -L "$txn_root" ] && [ -d "$txn_root" ]
}

_adk_active_txn() {
    local runtime_root="$1"
    local marker
    local txn_name
    local txn_root

    marker="$(_adk_active_marker "$runtime_root")" || return 1
    [ -e "$marker" ] || return 1
    if [ -L "$marker" ] || [ ! -f "$marker" ]; then
        echo "Invalid routine asset transaction marker: $marker" >&2
        return 2
    fi
    txn_name="$(sed -n '1p' "$marker")" || return 2
    case "$txn_name" in
        routine-assets.txn.*) ;;
        *) echo "Invalid routine asset transaction id: $txn_name" >&2; return 2 ;;
    esac
    case "${txn_name#routine-assets.txn.}" in
        ''|*[!A-Za-z0-9]*)
            echo "Invalid routine asset transaction id: $txn_name" >&2
            return 2
            ;;
    esac
    txn_root="$(_adk_state_dir "$runtime_root")/$txn_name"
    _adk_validate_txn_path "$runtime_root" "$txn_root" || {
        echo "Routine asset transaction directory missing: $txn_root" >&2
        return 2
    }
    printf '%s\n' "$txn_root"
}

_adk_assert_active_txn() {
    local runtime_root="$1"
    local expected="$2"
    local active

    active="$(_adk_active_txn "$runtime_root")" || return 1
    [ "$active" = "$expected" ] || {
        echo "Routine asset transaction ownership mismatch" >&2
        return 1
    }
}

adk_routine_asset_transaction_phase() {
    local runtime_root="$1"
    local txn_root="$2"

    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    _adk_read_txn_phase "$txn_root"
}

_adk_close_txn() {
    local runtime_root="$1"
    local txn_root="$2"
    local marker

    marker="$(_adk_active_marker "$runtime_root")" || return 1
    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    rm -f "$marker" || return 1
    rm -rf "$txn_root"
}

_adk_assert_surface_paths_safe() {
    local runtime_root="$1"
    local surface="$2"
    local path

    if [ -L "$runtime_root" ]; then
        echo "Refusing symlinked routine asset runtime root: $runtime_root" >&2
        return 1
    fi
    for path in \
        "$runtime_root/$surface" \
        "$runtime_root/$surface.old" \
        "$runtime_root/$surface.swap-current"; do
        if [ -L "$path" ]; then
            echo "Refusing symlinked routine asset transaction path: $path" >&2
            return 1
        fi
        if [ -e "$path" ] && [ ! -d "$path" ]; then
            echo "Routine asset transaction path is not a directory: $path" >&2
            return 1
        fi
        if [ -d "$path" ]; then
            _adk_assert_no_symlink_tree "$path" || return 1
        fi
    done
}

# Reconcile layouts left by the pre-state-machine deploy code. With no durable
# active marker, a present live tree is authoritative; .old can only be stale
# cleanup residue. If live is absent, restore a sole .old or swap-current before
# deleting anything so every last copy survives interruption.
_adk_reconcile_unmarked_surface() {
    local runtime_root="$1"
    local surface="$2"
    local live="$runtime_root/$surface"
    local old="$runtime_root/$surface.old"
    local retry="$runtime_root/$surface.swap-current"

    _adk_assert_surface_paths_safe "$runtime_root" "$surface" || return 1
    if [ -d "$live" ]; then
        [ ! -d "$old" ] || rm -rf "$old" || return 1
        [ ! -d "$retry" ] || rm -rf "$retry" || return 1
        return 0
    fi
    if [ -d "$old" ]; then
        mv "$old" "$live" || return 1
        [ ! -d "$retry" ] || rm -rf "$retry" || return 1
        return 0
    fi
    if [ -d "$retry" ]; then
        mv "$retry" "$live" || return 1
    fi
}

adk_begin_routine_asset_transaction() {
    local runtime_root="$1"
    local lock_file="${2:-$runtime_root/runtime/deploy-release.lock}"
    local state_dir
    local marker
    local marker_tmp
    local txn_root
    local txn_name

    state_dir="$(_adk_state_dir "$runtime_root")" || return 1
    marker="$(_adk_active_marker "$runtime_root")" || return 1
    [ "$ADK_ROUTINE_ASSET_LOCK_DIR" = "${lock_file}.d" ] || {
        echo "Routine asset transaction requires the shared deploy lock" >&2
        return 1
    }
    mkdir -p "$state_dir" || return 1
    if [ -e "$marker" ]; then
        adk_recover_active_routine_asset_transaction "$runtime_root" || return 1
    fi
    _adk_reconcile_unmarked_surface "$runtime_root" "routines" || return 1
    _adk_reconcile_unmarked_surface "$runtime_root" "routine-helpers" || return 1

    txn_root="$(mktemp -d "$state_dir/routine-assets.txn.XXXXXXXX")" || return 1
    txn_name="$(basename "$txn_root")" || return 1
    mkdir -p "$txn_root/staged" "$txn_root/surfaces/routines" \
        "$txn_root/surfaces/routine-helpers" || {
        rm -rf "$txn_root" 2>/dev/null || true
        return 1
    }
    _adk_write_phase "$txn_root" "staging" || {
        rm -rf "$txn_root" 2>/dev/null || true
        return 1
    }
    marker_tmp="${marker}.tmp.$$.$RANDOM"
    if ! printf '%s\n' "$txn_name" > "$marker_tmp" || ! mv "$marker_tmp" "$marker"; then
        rm -f "$marker_tmp" 2>/dev/null || true
        rm -rf "$txn_root" 2>/dev/null || true
        return 1
    fi
    printf '%s\n' "$txn_root"
}

_adk_stage_preserved_asset_surface() {
    local source_root="$1"
    local runtime_root="$2"
    local txn_root="$3"
    local surface="$4"
    local incoming_root="${5:-}"
    local live_root="$runtime_root/$surface"
    local staged_root="$txn_root/staged/$surface"
    local root_mode

    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    [ "$(_adk_read_txn_phase "$txn_root")" = "staging" ] || return 1
    _adk_assert_no_symlink_tree "$source_root" || return 1
    if [ -n "$incoming_root" ]; then
        [ -d "$incoming_root" ] || return 1
        _adk_assert_no_symlink_tree "$incoming_root" || return 1
    fi
    _adk_assert_surface_paths_safe "$runtime_root" "$surface" || return 1
    if [ -d "$live_root" ]; then
        root_mode="$(_adk_path_mode "$live_root")" || return 1
    else
        root_mode="$(_adk_path_mode "$source_root")" || return 1
    fi
    [ ! -e "$staged_root" ] || return 1
    mkdir -p "$staged_root" || return 1
    chmod "$root_mode" "$staged_root" || return 1
    if [ -d "$live_root" ]; then
        if ! rsync -a "$live_root/" "$staged_root/"; then
            rm -rf "$staged_root" 2>/dev/null || true
            return 1
        fi
    fi
    # A peer inbox is operator data delivered out-of-band. It overlays the
    # peer's old live tree but never wins over repository-owned paths below.
    if [ -n "$incoming_root" ]; then
        if ! rsync -a "$incoming_root/" "$staged_root/"; then
            rm -rf "$staged_root" 2>/dev/null || true
            return 1
        fi
    fi
    # Repository/artifact bytes are authoritative at matching relative paths.
    # --checksum defeats rsync's size+mtime quick-check: successive v0/v1 test
    # payloads (and real generated assets) can legitimately share both while
    # containing different bytes.
    if ! rsync -a --checksum "$source_root/" "$staged_root/"; then
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    fi
    chmod "$root_mode" "$staged_root" || {
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    }
}

adk_stage_routines() {
    local repo_root="$1"
    local runtime_root="$2"
    local txn_root="$3"
    local incoming_root="${4:-}"
    local staged_root="$txn_root/staged/routines"

    adk_validate_quickjs_routine_tree "$repo_root/routines" || return 1
    if [ -n "$incoming_root" ]; then
        adk_validate_quickjs_routine_tree "$incoming_root" || return 1
    fi
    _adk_stage_preserved_asset_surface \
        "$repo_root/routines" "$runtime_root" "$txn_root" "routines" \
        "$incoming_root" \
        || return 1
    if ! adk_remove_legacy_routine_helpers "$staged_root" \
      || ! adk_validate_quickjs_routine_tree "$staged_root"; then
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    fi
    printf '%s\n' "$staged_root"
}

adk_stage_routine_helpers() {
    local repo_root="$1"
    local runtime_root="$2"
    local txn_root="$3"
    local incoming_root="${4:-}"
    local staged_root="$txn_root/staged/routine-helpers"

    adk_validate_routine_helper_surface "$repo_root/routine-helpers" || return 1
    if [ -n "$incoming_root" ]; then
        adk_validate_routine_helper_surface "$incoming_root" || return 1
    fi
    _adk_stage_preserved_asset_surface \
        "$repo_root/routine-helpers" "$runtime_root" "$txn_root" "routine-helpers" \
        "$incoming_root" \
        || return 1
    if ! adk_validate_routine_helper_surface "$staged_root"; then
        rm -rf "$staged_root" 2>/dev/null || true
        return 1
    fi
    printf '%s\n' "$staged_root"
}

_adk_record_surface_baseline() {
    local runtime_root="$1"
    local txn_root="$2"
    local surface="$3"
    local live="$runtime_root/$surface"

    if [ -d "$live" ]; then
        _adk_write_atomic_file "$txn_root/surfaces/$surface/had_live" "1"
    elif [ -e "$live" ]; then
        return 1
    else
        _adk_write_atomic_file "$txn_root/surfaces/$surface/had_live" "0"
    fi
}

_adk_promote_surface() {
    local runtime_root="$1"
    local txn_root="$2"
    local surface="$3"
    local live="$runtime_root/$surface"
    local old="$runtime_root/$surface.old"
    local retry="$runtime_root/$surface.swap-current"
    local staged="$txn_root/staged/$surface"
    local had_live

    had_live="$(sed -n '1p' "$txn_root/surfaces/$surface/had_live")" || return 1
    [ -d "$staged" ] || return 1
    [ ! -e "$old" ] && [ ! -e "$retry" ] || return 1
    if [ "$had_live" = 1 ]; then
        [ -d "$live" ] || return 1
        mv "$live" "$old" || return 1
    elif [ "$had_live" = 0 ]; then
        [ ! -e "$live" ] || return 1
    else
        return 1
    fi
    mv "$staged" "$live"
}

adk_promote_routine_asset_transaction() {
    local runtime_root="$1"
    local txn_root="$2"

    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    [ "$(_adk_read_txn_phase "$txn_root")" = "staging" ] || return 1
    adk_validate_quickjs_routine_tree "$txn_root/staged/routines" || return 1
    adk_validate_routine_helper_surface "$txn_root/staged/routine-helpers" || return 1
    _adk_record_surface_baseline "$runtime_root" "$txn_root" "routines" || return 1
    _adk_record_surface_baseline "$runtime_root" "$txn_root" "routine-helpers" || return 1
    # Durable arming precedes the first live rename. The EXIT path can recover
    # by marker even if TERM lands inside the first mv before this function returns.
    _adk_write_phase "$txn_root" "armed" || return 1

    # Leave an interrupted/failed armed transaction durable. The caller owns
    # the binary decision and must choose rollback or fail-forward; rolling back
    # here would discard the exact staged generation after an installer has
    # already replaced its binary.
    _adk_promote_surface "$runtime_root" "$txn_root" "routines" || return 1
    _adk_promote_surface "$runtime_root" "$txn_root" "routine-helpers" || return 1
    _adk_write_phase "$txn_root" "promoted"
}

_adk_finish_promote_surface() {
    local runtime_root="$1"
    local txn_root="$2"
    local surface="$3"
    local live="$runtime_root/$surface"
    local old="$runtime_root/$surface.old"
    local retry="$runtime_root/$surface.swap-current"
    local staged="$txn_root/staged/$surface"
    local had_live

    had_live="$(sed -n '1p' "$txn_root/surfaces/$surface/had_live" 2>/dev/null)" \
        || return 1
    [ ! -e "$retry" ] || return 1
    if [ "$had_live" = 1 ]; then
        if [ -d "$staged" ]; then
            if [ -d "$old" ] && [ ! -e "$live" ]; then
                mv "$staged" "$live" || return 1
            elif [ ! -e "$old" ] && [ -d "$live" ]; then
                mv "$live" "$old" || return 1
                mv "$staged" "$live" || return 1
            else
                return 1
            fi
        else
            [ -d "$old" ] && [ -d "$live" ] || return 1
        fi
    elif [ "$had_live" = 0 ]; then
        [ ! -e "$old" ] || return 1
        if [ -d "$staged" ] && [ ! -e "$live" ]; then
            mv "$staged" "$live" || return 1
        elif [ ! -e "$staged" ] && [ -d "$live" ]; then
            :
        else
            return 1
        fi
    else
        return 1
    fi
}

# Fail-forward is used only when the binary cannot safely roll back. It can
# finish a signal-interrupted armed promotion without borrowing another
# transaction's stage directory, then commits that exact payload generation.
adk_commit_routine_asset_transaction_forward() {
    local runtime_root="$1"
    local txn_root="$2"
    local phase

    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    phase="$(_adk_read_txn_phase "$txn_root")" || return 1
    if [ "$phase" = "staging" ]; then
        adk_validate_quickjs_routine_tree "$txn_root/staged/routines" || return 1
        adk_validate_routine_helper_surface "$txn_root/staged/routine-helpers" || return 1
        _adk_record_surface_baseline "$runtime_root" "$txn_root" "routines" || return 1
        _adk_record_surface_baseline "$runtime_root" "$txn_root" "routine-helpers" || return 1
        _adk_write_phase "$txn_root" "armed" || return 1
        phase="armed"
    fi
    if [ "$phase" = "armed" ]; then
        _adk_finish_promote_surface "$runtime_root" "$txn_root" "routines" || return 1
        _adk_finish_promote_surface "$runtime_root" "$txn_root" "routine-helpers" || return 1
        _adk_write_phase "$txn_root" "promoted" || return 1
    elif [ "$phase" != "promoted" ] && [ "$phase" != "committing" ] \
      && [ "$phase" != "committed" ]; then
        return 1
    fi
    adk_commit_routine_asset_transaction "$runtime_root" "$txn_root"
}

_adk_rollback_surface() {
    local runtime_root="$1"
    local txn_root="$2"
    local surface="$3"
    local live="$runtime_root/$surface"
    local old="$runtime_root/$surface.old"
    local retry="$runtime_root/$surface.swap-current"
    local staged="$txn_root/staged/$surface"
    local had_live

    had_live="$(sed -n '1p' "$txn_root/surfaces/$surface/had_live" 2>/dev/null)" \
        || return 1
    _adk_assert_surface_paths_safe "$runtime_root" "$surface" || return 1
    if [ "$had_live" = 1 ]; then
        if [ -d "$old" ]; then
            if [ -d "$live" ]; then
                [ ! -e "$retry" ] || return 1
                mv "$live" "$retry" || return 1
            fi
            if ! mv "$old" "$live"; then
                [ -d "$live" ] || [ ! -d "$retry" ] \
                    || mv "$retry" "$live" 2>/dev/null || true
                return 1
            fi
        fi
        [ -d "$live" ] || {
            echo "Rollback lost the original $surface live tree" >&2
            return 1
        }
        [ ! -d "$retry" ] || rm -rf "$retry" || return 1
    elif [ "$had_live" = 0 ]; then
        [ ! -e "$old" ] || return 1
        if [ -d "$staged" ]; then
            [ ! -e "$live" ] || return 1
        elif [ -d "$live" ]; then
            [ ! -e "$retry" ] || return 1
            mv "$live" "$retry" || return 1
        fi
        [ ! -d "$retry" ] || rm -rf "$retry" || return 1
    else
        return 1
    fi
}

adk_abort_routine_asset_transaction() {
    local runtime_root="$1"
    local txn_root="$2"
    local phase

    if [ ! -e "$txn_root" ] && [ ! -e "$(_adk_active_marker "$runtime_root")" ]; then
        return 0
    fi
    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    phase="$(_adk_read_txn_phase "$txn_root")" || return 1
    [ "$phase" = "staging" ] || return 1
    _adk_write_phase "$txn_root" "rolled-back" || return 1
    _adk_close_txn "$runtime_root" "$txn_root"
}

adk_rollback_routine_asset_transaction() {
    local runtime_root="$1"
    local txn_root="$2"
    local phase

    if [ ! -e "$txn_root" ] && [ ! -e "$(_adk_active_marker "$runtime_root")" ]; then
        return 0
    fi
    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    phase="$(_adk_read_txn_phase "$txn_root")" || return 1
    case "$phase" in
        staging) adk_abort_routine_asset_transaction "$runtime_root" "$txn_root"; return ;;
        committing|committed)
            echo "Refusing rollback after routine asset commit began" >&2
            return 1
            ;;
        rolled-back) _adk_close_txn "$runtime_root" "$txn_root"; return ;;
        armed|promoted|rolling-back) ;;
        *) echo "Invalid routine asset rollback phase: $phase" >&2; return 1 ;;
    esac
    _adk_write_phase "$txn_root" "rolling-back" || return 1
    _adk_rollback_surface "$runtime_root" "$txn_root" "routine-helpers" || return 1
    _adk_rollback_surface "$runtime_root" "$txn_root" "routines" || return 1
    _adk_write_phase "$txn_root" "rolled-back" || return 1
    _adk_close_txn "$runtime_root" "$txn_root"
}

adk_mark_routine_asset_transaction_committing() {
    local runtime_root="$1"
    local txn_root="$2"
    local phase

    _adk_assert_active_txn "$runtime_root" "$txn_root" || return 1
    phase="$(_adk_read_txn_phase "$txn_root")" || return 1
    case "$phase" in
        promoted) _adk_write_phase "$txn_root" "committing" ;;
        committing|committed) ;;
        *) echo "Invalid routine asset commit-intent phase: $phase" >&2; return 1 ;;
    esac
}

adk_commit_routine_asset_transaction() {
    local runtime_root="$1"
    local txn_root="$2"
    local phase
    local surface

    if [ ! -e "$txn_root" ] && [ ! -e "$(_adk_active_marker "$runtime_root")" ]; then
        return 0
    fi
    adk_mark_routine_asset_transaction_committing "$runtime_root" "$txn_root" \
        || return 1
    phase="$(_adk_read_txn_phase "$txn_root")" || return 1
    if [ "$phase" = "committed" ]; then
        _adk_close_txn "$runtime_root" "$txn_root"
        return
    fi
    for surface in routine-helpers routines; do
        _adk_assert_surface_paths_safe "$runtime_root" "$surface" || return 1
        [ -d "$runtime_root/$surface" ] || return 1
        rm -rf "$runtime_root/$surface.old" \
            "$runtime_root/$surface.swap-current" \
            "$txn_root/staged/$surface" || return 1
    done
    _adk_write_phase "$txn_root" "committed" || return 1
    _adk_close_txn "$runtime_root" "$txn_root"
}

adk_recover_active_routine_asset_transaction() {
    local runtime_root="$1"
    local txn_root
    local phase
    local active_status

    if txn_root="$(_adk_active_txn "$runtime_root")"; then
        :
    else
        active_status=$?
        [ "$active_status" -eq 1 ] && return 0
        return 1
    fi
    phase="$(_adk_read_txn_phase "$txn_root")" || return 1
    case "$phase" in
        staging) adk_abort_routine_asset_transaction "$runtime_root" "$txn_root" ;;
        armed|promoted|rolling-back)
            adk_rollback_routine_asset_transaction "$runtime_root" "$txn_root"
            ;;
        committing|committed)
            adk_commit_routine_asset_transaction "$runtime_root" "$txn_root"
            ;;
        rolled-back) _adk_close_txn "$runtime_root" "$txn_root" ;;
        *) echo "Invalid active routine asset phase: $phase" >&2; return 1 ;;
    esac
}

adk_validate_peer_runtime_root() {
    local remote_root="$1"

    [ "${#remote_root}" -le 4096 ] || return 1
    case "$remote_root" in
        /*) ;;
        *) return 1 ;;
    esac
    case "$remote_root" in
        *$'\n'*|*$'\r'*) return 1 ;;
    esac
}

adk_validate_peer_destination() {
    local peer="$1"

    [ -n "$peer" ] && [ "${#peer}" -le 255 ] || return 1
    case "$peer" in
        -*|*[!A-Za-z0-9._@-]*) return 1 ;;
    esac
}

_adk_validate_peer_timeout() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$1" -le 86400 ]
}

_adk_validate_incoming_token() {
    local token="$1"

    [ -n "$token" ] && [ "${#token}" -le 128 ] || return 1
    case "$token" in
        *[!A-Za-z0-9._-]*) return 1 ;;
    esac
}

_adk_routine_asset_incoming_path() {
    local runtime_root="$1"
    local token="$2"

    adk_validate_peer_runtime_root "$runtime_root" || return 1
    _adk_validate_incoming_token "$token" || return 1
    printf '%s/runtime/routine-assets.incoming.%s\n' "$runtime_root" "$token"
}

adk_prepare_peer_asset_incoming() {
    local peer="$1"
    local remote_root="$2"
    local token="$3"
    local timeout_seconds="$4"
    local incoming
    local remote_command

    adk_validate_peer_destination "$peer" || return 1
    adk_validate_peer_runtime_root "$remote_root" || return 1
    _adk_validate_incoming_token "$token" || return 1
    _adk_validate_peer_timeout "$timeout_seconds" || return 1
    incoming="$(_adk_routine_asset_incoming_path "$remote_root" "$token")" \
        || return 1
    remote_command="set -e
runtime_root=$(printf '%q' "$remote_root")
incoming=$(printf '%q' "$incoming")
runtime_parent=\$(dirname \"\$runtime_root\")
for path in \"\$runtime_parent\" \"\$runtime_root\" \"\$runtime_root/runtime\" \"\$incoming\"; do
    [ ! -L \"\$path\" ] || exit 61
done
[ ! -e \"\$incoming\" ] || exit 62
mkdir -p \"\$runtime_root/runtime\"
mkdir \"\$incoming\"
chmod 700 \"\$incoming\""
    ssh -o "ConnectTimeout=$timeout_seconds" "$peer" \
        "bash -lc $(printf '%q' "$remote_command")" || return 1
    printf '%s\n' "$incoming"
}

adk_remove_peer_asset_incoming() {
    local peer="$1"
    local remote_root="$2"
    local token="$3"
    local timeout_seconds="$4"
    local incoming
    local remote_command

    adk_validate_peer_destination "$peer" || return 1
    adk_validate_peer_runtime_root "$remote_root" || return 1
    _adk_validate_incoming_token "$token" || return 1
    _adk_validate_peer_timeout "$timeout_seconds" || return 1
    incoming="$(_adk_routine_asset_incoming_path "$remote_root" "$token")" \
        || return 1
    remote_command="set -e
incoming=$(printf '%q' "$incoming")
[ ! -L \"\$incoming\" ] || exit 63
if [ -e \"\$incoming\" ]; then
    [ -d \"\$incoming\" ] || exit 64
    first_link=\$(find \"\$incoming\" -type l -print -quit) || exit 65
    [ -z \"\$first_link\" ] || exit 66
    rm -rf \"\$incoming\"
fi"
    ssh -o "ConnectTimeout=$timeout_seconds" "$peer" \
        "bash -lc $(printf '%q' "$remote_command")"
}

_adk_assert_routine_asset_incoming_path() {
    local runtime_root="$1"
    local incoming="$2"
    local expected_parent="$runtime_root/runtime"
    local incoming_parent
    local incoming_name

    adk_validate_peer_runtime_root "$runtime_root" || return 1
    [ -n "$incoming" ] && [ "${#incoming}" -le 4096 ] || return 1
    incoming_parent="$(dirname "$incoming")" || return 1
    incoming_name="$(basename "$incoming")" || return 1
    [ "$incoming_parent" = "$expected_parent" ] || return 1
    case "$incoming_name" in
        routine-assets.incoming.*)
            _adk_validate_incoming_token \
                "${incoming_name#routine-assets.incoming.}" || return 1
            ;;
        *) return 1 ;;
    esac
    [ ! -L "$runtime_root" ] \
        && [ ! -L "$expected_parent" ] \
        && [ ! -L "$incoming" ] \
        && [ -d "$incoming" ] || return 1
    _adk_assert_no_symlink_tree "$incoming"
}

adk_claim_routine_asset_incoming() {
    local runtime_root="$1"
    local incoming="$2"
    local lock_file="${3:-$runtime_root/runtime/deploy-release.lock}"
    local claim="$incoming/.claimed"

    [ "$ADK_ROUTINE_ASSET_LOCK_DIR" = "${lock_file}.d" ] || {
        echo "Routine asset inbox claim requires the shared deploy lock" >&2
        return 1
    }
    _adk_assert_routine_asset_incoming_path "$runtime_root" "$incoming" \
        || return 1
    [ ! -e "$claim" ] && [ ! -L "$claim" ] || return 1
    _adk_write_atomic_file "$claim" "$ADK_ROUTINE_ASSET_LOCK_TOKEN"
}

adk_remove_claimed_routine_asset_incoming() {
    local runtime_root="$1"
    local incoming="$2"
    local lock_file="${3:-$runtime_root/runtime/deploy-release.lock}"
    local claim="$incoming/.claimed"
    local claim_token

    [ "$ADK_ROUTINE_ASSET_LOCK_DIR" = "${lock_file}.d" ] || return 1
    _adk_assert_routine_asset_incoming_path "$runtime_root" "$incoming" \
        || return 1
    claim_token="$(sed -n '1p' "$claim" 2>/dev/null)" || return 1
    [ "$claim_token" = "$ADK_ROUTINE_ASSET_LOCK_TOKEN" ] || return 1
    rm -rf "$incoming"
}

adk_guard_peer_routine_asset_paths() {
    local peer="$1"
    local remote_root="$2"
    local timeout_seconds="$3"
    local remote_command

    adk_validate_peer_destination "$peer" || return 1
    adk_validate_peer_runtime_root "$remote_root" || return 1
    _adk_validate_peer_timeout "$timeout_seconds" || return 1
    remote_command="runtime_root=$(printf '%q' "$remote_root")
runtime_parent=\$(dirname \"\$runtime_root\")
for path in \"\$runtime_parent\" \"\$runtime_root\" \"\$runtime_root/routines\" \"\$runtime_root/routine-helpers\"; do
    [ ! -L \"\$path\" ] || exit 41
done
for root in \"\$runtime_root/routines\" \"\$runtime_root/routine-helpers\"; do
    if [ -d \"\$root\" ]; then
        first_link=\$(find \"\$root\" -type l -print -quit) || exit 42
        [ -z \"\$first_link\" ] || exit 43
    fi
done"
    ssh -o "ConnectTimeout=$timeout_seconds" "$peer" \
        "bash -lc $(printf '%q' "$remote_command")"
}

adk_rsync_peer_asset_surface() {
    local source_root="$1"
    local peer="$2"
    local remote_root="$3"
    local surface="$4"
    local timeout_seconds="$5"
    local remote_target
    local remote_command
    local pipeline_status

    adk_validate_peer_destination "$peer" || return 1
    adk_validate_peer_runtime_root "$remote_root" || return 1
    _adk_validate_peer_timeout "$timeout_seconds" || return 1
    case "$surface" in
        routines|routine-helpers) ;;
        *) return 1 ;;
    esac
    _adk_assert_no_symlink_tree "$source_root" || return 1

    # Apple still ships rsync/openrsync variants that reject --protect-args.
    # Probe the exact option instead of parsing vendor-specific version text.
    # Modern rsync keeps its argument-safe fast path; legacy hosts use a tar
    # stream whose remote command is quoted as one bash -lc argument.
    if rsync --protect-args --version >/dev/null 2>&1; then
        rsync -a --protect-args \
            -e "ssh -o ConnectTimeout=$timeout_seconds" \
            -- "$source_root/" "$peer:$remote_root/$surface/"
        return
    fi

    remote_target="$remote_root/$surface"
    remote_command="set -e
target=$(printf '%q' "$remote_target")
target_parent=\$(dirname \"\$target\")
for path in \"\$target_parent\" \"\$target\"; do
    [ ! -L \"\$path\" ] || exit 51
done
if [ -e \"\$target\" ] && [ ! -d \"\$target\" ]; then
    exit 52
fi
mkdir -p \"\$target\"
first_link=\$(find \"\$target\" -type l -print -quit) || exit 53
[ -z \"\$first_link\" ] || exit 54
tar -xf - -C \"\$target\""

    tar -C "$source_root" -cf - . \
        | ssh -o "ConnectTimeout=$timeout_seconds" "$peer" \
            "bash -lc $(printf '%q' "$remote_command")"
    pipeline_status=("${PIPESTATUS[@]}")
    [ "${pipeline_status[0]:-1}" -eq 0 ] \
        && [ "${pipeline_status[1]:-1}" -eq 0 ]
}
