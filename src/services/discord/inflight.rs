//! Inflight turn state persistence.
//!
//! `response_sent_offset`, `current_msg_id`, and `last_watcher_relayed_offset`
//! participate in the relay state contract documented in
//! `docs/relay-state-contract.md` (#1222 / #1224). Any change that touches
//! relay producers/consumers must keep the invariants there satisfied.

pub(in crate::services::discord) mod anchor_repost;
pub(in crate::services::discord) mod budget;
mod finalizer_identity;
#[cfg(test)]
mod invariant_test_capture;
mod model;

// #3479: the pure domain model moved to `model.rs`; re-export every public
// item at its original visibility so existing `inflight::*` / `super::*`
// references across the discord module resolve unchanged.
pub(in crate::services::discord) use model::{
    InflightTurnIdentity, InflightTurnState, RelayOwnerKind, TurnSource, opt_channel_id,
    opt_message_id, optional_message_id,
};

mod episode_guard;
mod store;

// #3479: the FS path layout + flock guard moved to `store.rs`. `inflight_state_path`
// / `lock_inflight_state_path` are re-exported at the module-tree visibility so
// root call sites and `super::*` references in sibling child modules (e.g.
// `budget`) resolve unchanged; `inflight_provider_dir` stays inflight-private
// (root callers only) and is brought in via a plain import. `InflightStateFileLock`
// is named nowhere outside `store` (it only flows as a return type), so it keeps
// its module-tree visibility there without a parent re-export.
pub(in crate::services::discord) use episode_guard::{
    InflightEpisodeLockError, InflightEpisodePin, LockedInflightEpisode,
    adopt_and_lock_inflight_episode, lock_inflight_episode,
};
pub(in crate::services::discord) use store::InflightDeliveryRewindReason;
use store::inflight_provider_dir;
pub(in crate::services::discord::inflight) use store::inflight_state_path;
pub(crate) use store::lock_inflight_state_path;

// #3715 / #3835: the rebind-origin dead-watcher/orphan-lock helpers PLUS the
// staleness predicates and orphan-lock / rebind-origin reap helpers live in this
// capped sibling so this hot state parent stays below the frozen production-LoC
// baseline without changing call-site names.
mod rebind_reap;
// Facade re-exports: keep every reap/staleness symbol still referenced by
// discord-module / inflight-core lib code at its original `pub(super)` visibility
// so `inflight::*` paths stay byte-identical after the #3835 move.
pub(super) use self::rebind_reap::{
    DEAD_WATCHER_PROVEN_DEAD_SECS, INFLIGHT_STALENESS_THRESHOLD_SECS, RebindReapOutcome,
    emit_reap_abandoned_rebind_origin, inflight_state_is_stale,
    ownerless_external_input_inflight_is_stale, parse_started_at_unix, parse_updated_at_unix,
    reap_abandoned_rebind_origin_locked, reap_orphan_inflight_locks,
    rebind_origin_deadline_secs_env, should_reap_abandoned_rebind_origin,
    sweep_reap_dead_watcher_rebind_origin,
};
// Parent-internal helper (private in the parent before the move): re-imported so
// inflight-core resolves it unchanged, WITHOUT widening it to the discord module.
use self::rebind_reap::{reap_abandoned_rebind_origin_locked_in_root, rebind_origin_age_secs};
// Symbols only referenced by the in-file `#[cfg(test)]` modules (their lib callers
// moved into `rebind_reap`): re-imported test-only so the test modules resolve
// `super::*` unchanged without emitting unused-import warnings in the lib build.
#[cfg(test)]
use self::rebind_reap::{
    ORPHAN_LOCK_REAP_MIN_AGE_SECS, REBIND_ORIGIN_DEADLINE_SECS_DEFAULT, WatcherLiveness,
    ownerless_external_input_inflight_is_stale_at, proven_dead_from_signals,
    reap_dead_watcher_rebind_origin_locked, reap_dead_watcher_rebind_origin_locked_in_root,
    reap_orphan_inflight_locks_in_root, should_reap_dead_watcher_rebind_origin,
};

mod removal;
pub(crate) use self::removal::invalidate_stale_generation;
use self::removal::load_inflight_states_from_root;
#[cfg(test)]
use self::removal::{
    invalidate_stale_generation_in_root, set_test_tmux_alive_override, stale_removal_reason,
};
pub(super) use self::removal::{log_inflight_remove, log_inflight_remove_for_path};

mod watcher_state;
pub(in crate::services::discord) use self::watcher_state::{
    WatcherProgressOutcome, WatcherRelayWatermarkOutcome, WatcherRelayWatermarkPatch,
    WatcherStreamProgressPatch, WatcherTerminalCommitOutcome, WatcherTerminalCommitPatch,
    commit_watcher_terminal_delivery_locked, persist_watcher_relay_watermark_locked,
    persist_watcher_stream_progress_locked,
};
#[cfg(test)]
use self::watcher_state::{
    commit_watcher_terminal_delivery_locked_in_root,
    persist_watcher_relay_watermark_locked_in_root, persist_watcher_stream_progress_locked_in_root,
};

// #3835: typed status-panel / current-message ownership writes (the #3077
// panel-lifecycle race family) moved into a child module; re-exported below so
// `inflight::*` paths stay byte-identical for discord-module callers.
mod ownership_ops;
pub(in crate::services::discord) use self::ownership_ops::{
    StatusPanelBindGuard, StatusPanelBindOutcome, StatusPanelClearGuard, bind_status_panel,
    clear_current_msg_if_matches, clear_status_panel_if_current,
};
#[cfg(test)]
use self::ownership_ops::{
    bind_status_panel_in_root, clear_current_msg_if_matches_in_root,
    clear_status_panel_if_current_in_root,
};

// #3960: producer-liveness TOCTOU reclaim for orphaned `SessionBoundRelay`
// TUI-direct rows (the #3876 residual deferred from PR #3953).
mod orphan_relay_reclaim;
pub(in crate::services::discord) use self::orphan_relay_reclaim::{
    OrphanRelayReclaimOutcome, downgrade_orphaned_session_bound_relay_owner_locked,
    mark_session_bound_relay_delivered_locked, session_bound_relay_external_input_orphan_shape,
};

// #3835: the store-side CAS "save" and "clear" clusters moved into two capped
// children so this hot state parent retires from the giant-file registry. Every
// symbol is re-exported at its ORIGINAL visibility below (3-tier: pub(crate) /
// pub(in ...discord) / pub(super)), so external flat `inflight::SYMBOL` paths
// (incl. the #3016 core hot files) stay byte-identical. Public API is not
// widened. The shared persist/validate primitives moved to `store.rs`.
mod clear_store;
mod save_store;

// Shared persist/validate primitives (moved to `store.rs`): re-imported at the
// parent's original inflight-private visibility so sibling children resolve
// `super::persist_under_lock` / `super::load_inflight_state_unlocked` /
// `super::persist_under_lock_preserving_updated_at` and the CAS children resolve
// `validate_inflight_state_for_save` (via `use super::*`) unchanged.
use self::store::{
    load_inflight_state_unlocked, persist_under_lock, persist_under_lock_preserving_updated_at,
    validate_inflight_state_for_save, validate_inflight_state_for_save_with_delivery_rewind_reason,
};

// Save cluster re-exports (original visibility mirrored). The save child declares
// these `pub(in crate::services::discord)` (the absolute spelling of the parent's
// original `pub(super)`), so this `pub(super)` re-export does not widen the surface.
pub(super) use self::save_store::{
    CreateNewInflightError, save_inflight_delivery_rewind_if_matches_identity, save_inflight_state,
    save_inflight_state_create_new, save_inflight_state_if_absent,
};
pub(in crate::services::discord) use self::save_store::{
    GuardedSaveOutcome, bind_recovery_anchor_if_matches_identity,
    mark_readopted_from_inflight_if_identity_unchanged,
    patch_restart_full_response_if_identity_unchanged,
    persist_leak_recovery_response_offset_if_matches_identity_locked,
    persist_recovery_output_path_if_matches_identity_locked,
    recovery_anchor_msg_id_if_matches_identity,
    save_existing_inflight_rebind_adoption_if_matches_episode,
    save_existing_inflight_rebind_adoption_if_matches_identity,
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_episode,
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity,
    save_inflight_state_if_identity_matches_allow_output_restamp,
    save_inflight_state_if_identity_unchanged, save_inflight_state_if_matches_identity,
    stamp_claude_e_process_if_matches_identity,
};
// Explicit-root save seams reached only by the parent's / siblings' test modules.
#[cfg(test)]
use self::save_store::{
    save_existing_inflight_rebind_adoption_if_matches_identity_in_root,
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root,
    save_inflight_state_if_matches_identity_in_root, save_inflight_state_in_root,
};

// Clear/Abandon cluster re-exports (original visibility mirrored).
pub(crate) use self::clear_store::{
    GuardedClearOutcome, clear_inflight_state, clear_inflight_state_if_matches,
    clear_inflight_state_if_matches_zero_owned, request_inflight_abandon_if_matches,
    request_inflight_abandon_if_matches_zero_owned,
};
pub(in crate::services::discord) use self::clear_store::{
    clear_inflight_state_if_matches_identity,
    clear_inflight_state_if_matches_identity_after_delivery,
    clear_inflight_state_if_matches_identity_generation,
    clear_inflight_state_if_matches_identity_turn_nonce,
    clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence,
    clear_rebind_origin_inflight_state_if_matches_identity,
    refresh_inflight_last_offset_if_matches_identity,
};
// `clear_*_in_root` seams reached by inflight-core in production (health recovery
// engine): the clear child declares them `pub(in crate::services::discord)`.
pub(super) use self::clear_store::{
    clear_inflight_state_if_matches_in_root, clear_inflight_state_if_matches_zero_owned_in_root,
};
// `normalize_response_sent_offset` is consumed by the `watcher_state` sibling in
// production, so it stays re-imported here at the parent's original private seam.
use self::clear_store::normalize_response_sent_offset;
// Explicit-root clear seams reached only by the parent's / siblings' test modules.
#[cfg(test)]
use self::clear_store::{
    clear_inflight_state_if_matches_identity_after_delivery_in_root,
    clear_inflight_state_if_matches_identity_in_root,
    clear_inflight_state_if_matches_identity_turn_nonce_in_root,
    clear_inflight_state_if_matches_tmux_response_in_root,
    clear_rebind_origin_inflight_state_if_matches_identity_in_root,
    refresh_inflight_last_offset_if_matches_identity_in_root,
    request_inflight_abandon_if_matches_in_root,
    request_inflight_abandon_if_matches_zero_owned_in_root, row_has_finalizable_placeholder,
};

use finalizer_identity::{
    backfill_finalizer_turn_id_under_lock, parse_inflight_state_content,
    parse_inflight_state_content_with_finalizer_backfill, read_inflight_state_content,
};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::TimeZone;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::InflightRestartMode;
use super::runtime_store::{atomic_write, discord_inflight_root};
use crate::dispatch::Source;
use crate::services::agent_protocol::RuntimeHandoffKind;
// #3552: short alias for the invariant-severity hint forwarded to observability.
use crate::services::observability::InvariantSeverity as ObsSeverity;
use crate::services::provider::ProviderKind;

// #2235 (follow-up to #2213): bump v7→v8. v7 added `runtime_kind` without a
// version change, so new→old rollbacks could read rows whose FIFO synthesis
// was elided for ClaudeTui and reject recovery with a misleading "input fifo
// path missing" notice. v8 marks the shape shipping the compat-fixed
// `input_fifo_path` alongside ClaudeTui plus the silent-skip recovery branch;
// old binaries deserialize v8 rows via `#[serde(default)]` (compat window:
// one release each direction).
//
// FIX #6 (Codex P2): bump v8→v9. v9 persists the originating Intervention's
// follow-up requeue context (`followup_reply_context`,
// `followup_has_reply_boundary`, `followup_merge_consecutive`,
// `followup_pending_uploads`, `followup_voice_announcement`) so a follow-up
// that hit a PRE-submit busy-timeout with requeue enabled can rebuild the
// retry Intervention faithfully instead of dropping its attachments / reply
// context / voice metadata. All five fields are `#[serde(default)]`, so v8 rows
// (and rows written by binaries that pre-date this field) still deserialize and
// simply yield empty/None — no recovery regression, full compat each direction.
const INFLIGHT_STATE_VERSION: u32 = 9;
const INFLIGHT_MAX_AGE_SECS: u64 = 300; // 5 minutes
const DRAIN_RESTART_MAX_AGE_SECS: u64 = 1800; // 30 minutes
const HOT_SWAP_HANDOFF_MAX_AGE_SECS: u64 = 900; // 15 minutes
/// #3293: restarts-with-failed-terminal-relay budget. `recovery_relay_attempts`
/// grows at most once per boot (recovery runs once per boot), so this is a
/// "3 consecutive restarts" budget, not a per-process retry cap.
pub(super) const RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET: u32 = 3;

/// #3581: current Unix epoch seconds (wall clock). Used to stamp a
/// rebind-origin row's birth time at creation.
pub(super) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(super) fn inflight_runtime_root() -> Option<PathBuf> {
    discord_inflight_root()
}

/// #2235: expose the local `INFLIGHT_STATE_VERSION` so the recovery engine
/// can decide whether an on-disk row was authored by a newer binary (i.e.
/// `state.version > inflight_state_version()`). Read-only accessor — the
/// constant itself stays private so we control the single bump site.
pub(super) fn inflight_state_version() -> u32 {
    INFLIGHT_STATE_VERSION
}

/// Load all inflight states for a provider WITHOUT the eviction side-effect
/// that `load_inflight_states_from_root` performs. Returns each state paired
/// with its file-mtime age in seconds. Used by `placeholder_sweeper` so the
/// sweeper can read-then-act-then-evict in one pass instead of racing the
/// regular load path's auto-deletion on stale entries.
pub(super) fn load_inflight_states_for_sweep(
    provider: &ProviderKind,
) -> Vec<(InflightTurnState, u64)> {
    let Some(root) = inflight_runtime_root() else {
        return Vec::new();
    };
    let dir = inflight_provider_dir(&root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = parse_inflight_state_content(&content) else {
            continue;
        };
        if state.provider_kind().as_ref() != Some(provider) {
            continue;
        }
        let age_secs = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        out.push((state, age_secs));
    }
    out
}

/// Delete the inflight state file for a (provider, channel_id) pair if it
/// still exists. Used by `placeholder_sweeper` to evict abandoned states
/// after a final placeholder edit. Idempotent.
pub(super) fn delete_inflight_state_file(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    log_inflight_remove_for_path(provider, channel_id, "delete_inflight_state_file", &path);
    fs::remove_file(path).is_ok()
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn bump_save_generation_for_write(path: &Path, state: &mut InflightTurnState) {
    let existing_generation = fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<InflightTurnState>(&content).ok())
        .map(|existing| existing.save_generation)
        .unwrap_or(0);
    state.save_generation = existing_generation
        .saturating_add(1)
        .max(state.save_generation.saturating_add(1))
        .max(1);
}

fn turn_id_for_state(state: &InflightTurnState) -> Option<String> {
    (state.user_msg_id != 0).then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id))
}

fn record_inflight_invariant(
    condition: bool,
    state: &InflightTurnState,
    invariant: &'static str,
    code_location: &'static str,
    message: &'static str,
    details: serde_json::Value,
) -> bool {
    record_inflight_invariant_with_severity(
        condition,
        state,
        invariant,
        code_location,
        message,
        details,
        ObsSeverity::Error,
    )
}

/// #3552 (pure, testable): the offset-monotonic invariants on the save path are
/// paired with the #3416 enforce guard. When that guard will SKIP the backward
/// write (`enforce_skips_backward_write`), the violation is already safely
/// handled (offset preserved, zero data loss) → record at WARN so the paired
/// `#3416 enforce` WARN is the only operator log and the duplicate ERROR noise
/// (~17/day) disappears. Otherwise the backward write persists → ERROR (a
/// genuine breach). The structured analytics event is identical either way.
fn offset_monotonic_invariant_severity(enforce_skips_backward_write: bool) -> ObsSeverity {
    if enforce_skips_backward_write {
        ObsSeverity::Warn
    } else {
        ObsSeverity::Error
    }
}

/// #3552: severity-aware variant of `record_inflight_invariant`.
fn record_inflight_invariant_with_severity(
    condition: bool,
    state: &InflightTurnState,
    invariant: &'static str,
    code_location: &'static str,
    message: &'static str,
    details: serde_json::Value,
    severity: ObsSeverity,
) -> bool {
    // #4422: the old test scraped formatted tracing text through a scoped
    // subscriber while tracing callsite/subscriber state changed in parallel.
    // Expose the already-selected typed severity to a thread-local TEST-ONLY
    // witness instead; production logging/event behavior remains byte-identical.
    #[cfg(test)]
    if !condition {
        invariant_test_capture::record(invariant, severity);
    }
    let turn_id = turn_id_for_state(state);
    crate::services::observability::record_invariant_check_with_severity(
        condition,
        crate::services::observability::InvariantViolation {
            provider: Some(state.provider.as_str()),
            channel_id: Some(state.channel_id),
            dispatch_id: state.dispatch_id.as_deref(),
            session_key: state.session_key.as_deref(),
            turn_id: turn_id.as_deref(),
            invariant,
            code_location,
            message,
            details,
        },
        severity,
    )
}

pub(in crate::services::discord) fn inflight_state_allows_idle_tmux_repair_state(
    state: &InflightTurnState,
) -> bool {
    state.full_response.trim().is_empty()
        && state.response_sent_offset == 0
        && state.last_watcher_relayed_offset.is_none()
        && state.dispatch_id.as_deref().is_none_or(str::is_empty)
        && state.current_tool_line.is_none()
        && state.last_tool_name.is_none()
        && !state.long_running_placeholder_active
}

pub(crate) fn inflight_state_allows_idle_tmux_repair(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<bool> {
    load_inflight_state(provider, channel_id)
        .map(|state| inflight_state_allows_idle_tmux_repair_state(&state))
}

pub(super) fn inflight_state_file_exists(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    inflight_state_path(&root, provider, channel_id).exists()
}

pub(super) fn clear_inflight_by_tmux_name(provider: &ProviderKind, tmux_name: &str) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };

    let provider_dir = inflight_provider_dir(&root, provider);
    let Ok(entries) = fs::read_dir(&provider_dir) else {
        return false;
    };

    let mut cleared = false;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(_lock) = lock_inflight_state_path(&path) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            continue;
        };
        if state.tmux_session_name.as_deref() != Some(tmux_name) {
            continue;
        }
        log_inflight_remove(
            provider,
            state.channel_id,
            state.user_msg_id,
            "clear_inflight_by_tmux_name",
            &path,
        );
        if fs::remove_file(&path).is_ok() {
            cleared = true;
        }
    }

    cleared
}

pub(super) fn mark_all_inflight_states_restart_mode(
    provider: &ProviderKind,
    restart_mode: InflightRestartMode,
) -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    // #3860 — set restart_mode via a per-row lock-RMW instead of blind-saving
    // the unlocked snapshot. `load_inflight_states_from_root` reads each row
    // WITHOUT holding its flock; the old code then `save`d that stale whole-row
    // snapshot back under the lock. A draining watcher that advanced the
    // delivery frontier (`response_sent_offset` / `last_offset`) on disk in the
    // gap therefore had its progress overwritten (frontier regression) → the
    // replacement watcher re-relayed `full_response[response_sent_offset..]`,
    // i.e. a duplicate Discord send (the issue's live sub-2000-char repro).
    // The enumeration is reused only to discover the live rows (its stale-row
    // GC side effects are preserved); the mutation re-reads the FRESH on-disk
    // row under the flock and sets ONLY restart_mode / restart_generation,
    // never the frontier, so it can no longer regress a concurrent writer.
    let states = load_inflight_states_from_root(&root, provider);
    let mut updated = 0usize;
    for state in states {
        let path = inflight_state_path(&root, provider, state.channel_id);
        if set_inflight_restart_mode_under_lock(&path, restart_mode) {
            updated += 1;
        }
    }
    updated
}

/// #3860 — RMW the restart-mode marker on one inflight row under its flock.
///
/// Re-reads the CURRENT on-disk state (so a delivery frontier that a concurrent
/// draining watcher advanced between the unlocked enumeration and this write is
/// preserved) and persists it with only `restart_mode` / `restart_generation`
/// changed. Mirrors the lock-then-read pattern of `clear_inflight_by_tmux_name`.
/// Returns whether the row was rewritten. Deliberately does NOT route through
/// `save_inflight_state_in_root` (which writes the *caller's* snapshot): the
/// whole point is to keep the on-disk frontier rather than carry a stale one.
fn set_inflight_restart_mode_under_lock(path: &Path, restart_mode: InflightRestartMode) -> bool {
    let Ok(_lock) = lock_inflight_state_path(path) else {
        return false;
    };
    let Some(mut state) = read_inflight_state_content(path) else {
        return false;
    };
    state.set_restart_mode(restart_mode);
    state.ensure_finalizer_turn_id();
    state.updated_at = now_string();
    bump_save_generation_for_write(path, &mut state);
    match serde_json::to_string_pretty(&state) {
        Ok(json) => atomic_write(path, &json).is_ok(),
        Err(_) => false,
    }
}

/// Load a single inflight state by provider + channel_id (returns None if missing).
pub(super) fn load_inflight_state(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<InflightTurnState> {
    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let data = fs::read_to_string(&path).ok()?;
    let (state, backfilled) = parse_inflight_state_content_with_finalizer_backfill(&data).ok()?;
    if backfilled {
        backfill_finalizer_turn_id_under_lock(&root, &path, provider).or(Some(state))
    } else {
        Some(state)
    }
}

/// Load a single inflight state without compatibility backfills or cleanup.
///
/// Use this for diagnostic/read-only probes that must not mutate sidecar state.
pub(in crate::services::discord) fn load_inflight_state_read_only(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<InflightTurnState> {
    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let data = fs::read_to_string(&path).ok()?;
    parse_inflight_state_content(&data).ok()
}

pub(super) fn load_inflight_states(provider: &ProviderKind) -> Vec<InflightTurnState> {
    let Some(root) = inflight_runtime_root() else {
        return Vec::new();
    };
    load_inflight_states_from_root(&root, provider)
}

pub(crate) fn latest_request_owner_user_id_for_channel(channel_id: u64) -> Option<u64> {
    let providers = [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::Qwen,
    ];

    providers
        .iter()
        .flat_map(load_inflight_states)
        .filter(|state| state.channel_id == channel_id)
        .max_by(|left, right| left.updated_at.cmp(&right.updated_at))
        .map(|state| state.request_owner_user_id)
}

/// #2448: explicit completion signal published from the turn_bridge
/// CompletionGuard so downstream listeners (currently the standby JSONL
/// relay) can exit promptly instead of polling against a wall-clock
/// timeout. Variants are intentionally narrow; add cases as new
/// listeners need them.
#[derive(Debug, Clone)]
pub(in crate::services::discord) enum InflightSignal {
    /// The turn_bridge task for this exact turn reached its terminal drop.
    /// Channel-scoped relays may use `channel_id`; turn-scoped consumers must
    /// also match `turn_id` so a late turn cannot stop its successor.
    Completed { channel_id: u64, turn_id: u64 },
}

#[cfg(test)]
#[path = "inflight/save_store/post_loop_identity_guard_tests.rs"]
mod post_loop_identity_guard_tests;

/// #1446 Layer 1 — `inflight_state_is_stale` is a pure helper with no
/// filesystem or runtime dependencies, so we keep its test always-on
/// (`#[cfg(test)]`) rather than tying it to the removed SQLite-only harness.
/// The heavier tests below require fixtures that still are not available in
/// plain `cargo test --bin agentdesk` invocations.
#[cfg(test)]
mod stall_recovery_tests {
    use super::{
        GuardedClearOutcome, GuardedSaveOutcome, INFLIGHT_STALENESS_THRESHOLD_SECS,
        InflightRestartMode, InflightTurnIdentity, InflightTurnState, RelayOwnerKind,
        StatusPanelBindGuard, StatusPanelBindOutcome, StatusPanelClearGuard,
        WatcherProgressOutcome, WatcherRelayWatermarkOutcome, WatcherRelayWatermarkPatch,
        WatcherStreamProgressPatch, WatcherTerminalCommitOutcome, WatcherTerminalCommitPatch,
        bind_status_panel_in_root, clear_current_msg_if_matches_in_root,
        clear_inflight_state_if_matches_identity_after_delivery_in_root,
        clear_inflight_state_if_matches_identity_in_root, clear_inflight_state_if_matches_in_root,
        clear_inflight_state_if_matches_tmux_response_in_root,
        clear_inflight_state_if_matches_zero_owned_in_root,
        clear_rebind_origin_inflight_state_if_matches_identity_in_root,
        clear_status_panel_if_current_in_root, commit_watcher_terminal_delivery_locked_in_root,
        inflight_state_allows_idle_tmux_repair_state, inflight_state_is_stale, inflight_state_path,
        load_inflight_states_from_root, lock_inflight_state_path, normalize_response_sent_offset,
        offset_monotonic_invariant_severity, ownerless_external_input_inflight_is_stale_at,
        persist_watcher_relay_watermark_locked_in_root,
        persist_watcher_stream_progress_locked_in_root,
        refresh_inflight_last_offset_if_matches_identity_in_root,
        request_inflight_abandon_if_matches_in_root,
        request_inflight_abandon_if_matches_zero_owned_in_root, row_has_finalizable_placeholder,
        save_existing_inflight_rebind_adoption_if_matches_identity_in_root,
        save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root,
        save_inflight_state_if_matches_identity_in_root, save_inflight_state_in_root,
        validate_inflight_state_for_save,
    };
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;
    use chrono::TimeZone;
    use std::path::Path;
    use tempfile::TempDir;

    // #4361: the flake-isolation regression tests live in a child file so their
    // LoC does not count against `inflight.rs`'s frozen `parent_test_residue`
    // ceiling (#4267/#4269 — move the tests, never raise the cap). They reach
    // these fixtures (`build_synth_3358`, `monotonic_3358_test_mutex`, the
    // `_in_root` helpers, …) via `use super::*`.
    mod flake_isolation_4361;
    mod flake_isolation_4422;

    /// `inflight_state_is_stale` must flip to true once `updated_at` is
    /// older than the configured threshold and stay false for fresh state.
    #[test]
    fn inflight_state_is_stale_returns_true_after_threshold() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            7,
            8,
            9,
            "hello".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            None,
            None,
            0,
        );

        // Anchor `now` and derive `updated_at` from it deterministically so
        // the test is independent of wall clock.
        let now_unix = chrono::Utc::now().timestamp();
        let fresh_unix = now_unix - 5;
        let stale_unix = now_unix - (INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 1;

        let to_local = |unix: i64| {
            chrono::Local
                .timestamp_opt(unix, 0)
                .single()
                .expect("valid local time")
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        };

        state.updated_at = to_local(fresh_unix);
        assert!(
            !inflight_state_is_stale(&state, now_unix, INFLIGHT_STALENESS_THRESHOLD_SECS),
            "fresh state must NOT be reported as stale"
        );

        state.updated_at = to_local(stale_unix);
        assert!(
            inflight_state_is_stale(&state, now_unix, INFLIGHT_STALENESS_THRESHOLD_SECS),
            "state older than threshold must be reported as stale"
        );

        // Unparseable timestamp must default to "not stale" — never infer
        // staleness from missing data.
        state.updated_at = "garbage-not-a-date".to_string();
        assert!(
            !inflight_state_is_stale(&state, now_unix, INFLIGHT_STALENESS_THRESHOLD_SECS),
            "unparseable updated_at must NOT be treated as stale"
        );
    }

    #[test]
    fn ownerless_external_input_stale_only_for_unowned_pre_placeholder_synthetic_rows() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale_unix = now_unix - (INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 1;
        let fresh_unix = now_unix - 5;
        let to_local = |unix: i64| {
            chrono::Local
                .timestamp_opt(unix, 0)
                .single()
                .expect("valid local time")
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        };
        let mut state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 8,
            "current_msg_id": 0,
            "current_msg_len": 3,
            "user_text": "typed in TUI",
            "source": "text",
            "session_id": null,
            "tmux_session_name": "AgentDesk-codex-adk-cdx",
            "output_path": "/tmp/rollout.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": to_local(stale_unix),
            "updated_at": to_local(stale_unix),
            "terminal_delivery_committed": false,
            "relay_owner_kind": "none",
            "turn_source": "external_input",
            "injected_prompt_message_id": 8
        }))
        .expect("deserialize external-input inflight row");

        assert!(
            ownerless_external_input_inflight_is_stale_at(&state, now_unix),
            "stale bridge-owned synthetic claim without a response placeholder is not live evidence"
        );

        state.updated_at = to_local(fresh_unix);
        assert!(
            !ownerless_external_input_inflight_is_stale_at(&state, now_unix),
            "fresh synthetic rows still block to protect live turns"
        );

        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        assert!(
            ownerless_external_input_inflight_is_stale_at(&state, now_unix),
            "planned-restart rows use started_at because updated_at is rewritten during boot"
        );

        state.restart_mode = None;
        state.updated_at = to_local(stale_unix);
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        assert!(
            !ownerless_external_input_inflight_is_stale_at(&state, now_unix),
            "watcher-owned TUI-direct rows have a live relay owner"
        );

        state.set_relay_owner_kind(RelayOwnerKind::None);
        state.current_msg_id = 9002;
        assert!(
            !ownerless_external_input_inflight_is_stale_at(&state, now_unix),
            "rows that already created a Discord response placeholder use terminal recovery paths"
        );
    }

    #[test]
    fn idle_tmux_repair_only_allows_empty_unclaimed_inflight() {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            888,
            Some("adk-cc".to_string()),
            1,
            2,
            3,
            "user prompt".to_string(),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        assert!(inflight_state_allows_idle_tmux_repair_state(&state));

        state.current_msg_len = "⠋ Processing...".len();
        assert!(inflight_state_allows_idle_tmux_repair_state(&state));

        state.full_response = "partial".to_string();
        assert!(!inflight_state_allows_idle_tmux_repair_state(&state));
        state.full_response.clear();

        state.last_watcher_relayed_offset = Some(10);
        assert!(!inflight_state_allows_idle_tmux_repair_state(&state));
        state.last_watcher_relayed_offset = None;

        state.dispatch_id = Some("dispatch-1".to_string());
        assert!(!inflight_state_allows_idle_tmux_repair_state(&state));
    }

    #[test]
    fn status_message_id_round_trips_for_status_panel_resume() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        let state = status_panel_test_state(
            42,
            8,
            99,
            Some("AgentDesk-claude-adk-claude"),
            Some(123_456),
        );

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status_message_id, Some(123_456));
        assert_eq!(loaded[0].current_msg_id, 99);
    }

    /// FIX #6 (Codex P2): the follow-up requeue context must survive the
    /// on-disk JSON round-trip, and rows written WITHOUT the fields (legacy
    /// v8 / pre-field rows) must deserialize cleanly to empty/None so requeue
    /// behaves exactly as before for them.
    #[test]
    fn followup_requeue_context_round_trips_and_defaults_when_absent() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        let announcement = crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: "전투 시스템 고쳐줘".to_string(),
            user_id: "7".to_string(),
            utterance_id: "utt-1".to_string(),
            language: "ko".to_string(),
            verbose_progress: false,
            started_at: None,
            completed_at: None,
            samples_written: None,
            control_channel_id: None,
            stt_mode: None,
            stt_latency_ms: None,
        };
        state.set_followup_requeue_context(
            Some("quoted reply context".to_string()),
            true,
            true,
            vec!["upload://a.png".to_string(), "upload://b.png".to_string()],
            Some(announcement.clone()),
            true,
        );

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].followup_reply_context.as_deref(),
            Some("quoted reply context")
        );
        assert!(loaded[0].followup_has_reply_boundary);
        assert!(loaded[0].followup_merge_consecutive);
        assert_eq!(
            loaded[0].followup_pending_uploads,
            vec!["upload://a.png".to_string(), "upload://b.png".to_string()]
        );
        assert_eq!(loaded[0].followup_voice_announcement, Some(announcement));
        assert!(loaded[0].followup_preserve_on_cancel);

        // A JSON row that omits the new fields entirely (legacy v8 / pre-field
        // shape) must still deserialize, defaulting the follow-up context.
        let mut value = serde_json::to_value(&state).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("followup_reply_context");
        obj.remove("followup_has_reply_boundary");
        obj.remove("followup_merge_consecutive");
        obj.remove("followup_pending_uploads");
        obj.remove("followup_voice_announcement");
        obj.remove("followup_preserve_on_cancel");
        let legacy: InflightTurnState =
            serde_json::from_value(value).expect("legacy row must deserialize");
        assert_eq!(legacy.followup_reply_context, None);
        assert!(!legacy.followup_has_reply_boundary);
        assert!(!legacy.followup_merge_consecutive);
        assert!(legacy.followup_pending_uploads.is_empty());
        assert_eq!(legacy.followup_voice_announcement, None);
        assert!(!legacy.followup_preserve_on_cancel);
    }

    // ---- #3558: watcher locked read-modify-write (offset TOCTOU) tests ----

    /// Seeds a watcher-streaming inflight row in `root` and returns it, with
    /// caller-controlled `last_offset` / `response_sent_offset` / `full_response`
    /// so the offset ownership semantics can be exercised.
    fn seed_watcher_stream_state(
        root: &Path,
        channel_id: u64,
        tmux_session_name: &str,
        full_response: &str,
        last_offset: u64,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-claude".to_string()),
            42,
            42,
            43,
            "prompt".to_string(),
            Some("session-3558".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-3558.jsonl".to_string()),
            None,
            64,
        );
        state.turn_start_offset = Some(64);
        state.full_response = full_response.to_string();
        state.response_sent_offset = full_response.len();
        state.last_offset = last_offset;
        save_inflight_state_in_root(root, &state).expect("seed watcher stream state");
        state
    }

    fn loaded_row(root: &Path, channel_id: u64) -> InflightTurnState {
        load_inflight_states_from_root(root, &ProviderKind::Claude)
            .into_iter()
            .find(|s| s.channel_id == channel_id)
            .expect("inflight row present")
    }

    /// Writes `state` to its on-disk path bypassing `validate_inflight_state_for_save`
    /// so a test can seed a pre-condition that is itself a (legitimate)
    /// fresh-turn reset / concurrently-advanced watermark without tripping the
    /// `#[cfg(debug_assertions)]` monotonic tripwire — these are exactly the
    /// disk states the helper under test must handle, not produce.
    fn force_write_state(root: &Path, state: &InflightTurnState) {
        let provider = state.provider_kind().expect("known provider");
        let path = inflight_state_path(root, &provider, state.channel_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create provider dir");
        }
        let json = serde_json::to_string_pretty(state).expect("serialize state");
        super::atomic_write(&path, &json).expect("force write state");
    }

    // ---- #3859: failure-path abandon-request (durable handoff, row deleted) ----

    fn spinner_row(channel_id: u64, user_msg_id: u64, current_msg_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-claude".to_string()),
            7,
            user_msg_id,
            current_msg_id,
            "prompt".to_string(),
            Some("session-3859".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        )
    }

    /// `row_has_finalizable_placeholder` gate: a pure spinner is finalizable; a
    /// placeholderless (`current_msg_id == user_msg_id`) or partial-output row is
    /// not (so the failure path deletes the row WITHOUT a "중단됨" clobber).
    #[test]
    fn finalizable_placeholder_gate_matches_pure_spinner_only() {
        // #3859 r5 (codex P2): seed a temp runtime root — `spinner_row` builds via
        // `InflightTurnState::new`, which loads the generation and trips the #3293
        // "test must set AGENTDESK_ROOT_DIR" guard under an isolated run.
        let (_lock, _temp, _env) = status_panel_test_root();
        let mut state = spinner_row(100, 8, 9001);
        assert!(row_has_finalizable_placeholder(&state), "pure spinner");

        let placeholderless = spinner_row(100, 8, 8);
        assert!(
            !row_has_finalizable_placeholder(&placeholderless),
            "anchor mirrors user msg → no separate placeholder card"
        );

        let mut zero = spinner_row(100, 0, 0);
        zero.current_msg_id = 0;
        assert!(!row_has_finalizable_placeholder(&zero), "no anchor");

        state.full_response = "partial answer".to_string();
        assert!(
            !row_has_finalizable_placeholder(&state),
            "partial output → keep delivered text, no clobber"
        );
        state.full_response.clear();
        state.response_sent_offset = 5;
        assert!(!row_has_finalizable_placeholder(&state), "streamed offset");
        state.response_sent_offset = 0;
        state.long_running_placeholder_active = true;
        state.full_response = "prose then long-running card".to_string();
        assert!(
            row_has_finalizable_placeholder(&state),
            "explicit long-running placeholder is finalizable even with prose"
        );
    }

    /// The failure path on a pure-spinner row: enqueue a durable abandon-request
    /// (so the sweeper finalizes the "🔄 처리 중" card to "중단됨" by msg id) AND
    /// DELETE the inflight row — freeing the channel immediately (no flag-on-live-
    /// row, no busy regression).
    #[test]
    fn abandon_request_enqueues_record_and_deletes_row() {
        let (_lock, temp, _env) = status_panel_test_root();
        let state = spinner_row(4242, 8, 9001);
        save_inflight_state_in_root(temp.path(), &state).expect("seed inflight row");

        let outcome = request_inflight_abandon_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            4242,
            8,
            "tok",
        );
        assert_eq!(outcome, GuardedClearOutcome::Cleared);

        // Row is DELETED (channel free).
        assert!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty(),
            "inflight row must be deleted (channel freed immediately)"
        );
        // Durable abandon-request carries the placeholder id + render fields.
        let pending =
            super::super::abandon_request_store::load_pending(&ProviderKind::Claude, "tok");
        assert_eq!(pending.len(), 1, "one durable abandon-request");
        assert_eq!(pending[0].0, 4242);
        assert_eq!(pending[0].1.msg_id, 9001);
        assert_eq!(pending[0].1.started_at, state.started_at);
    }

    /// A placeholderless row (anchor == user msg) and a partial-output row are
    /// DELETED but NOT enqueued — no "중단됨" clobber of the user's message / the
    /// delivered partial answer.
    #[test]
    fn abandon_request_deletes_without_enqueue_for_non_finalizable_rows() {
        let (_lock, temp, _env) = status_panel_test_root();
        // Placeholderless: current_msg_id == user_msg_id.
        let pl = spinner_row(4243, 8, 8);
        save_inflight_state_in_root(temp.path(), &pl).expect("seed");
        assert_eq!(
            request_inflight_abandon_if_matches_in_root(
                temp.path(),
                &ProviderKind::Claude,
                4243,
                8,
                "tok"
            ),
            GuardedClearOutcome::Cleared
        );

        // Partial-output row.
        let mut partial = spinner_row(4244, 9, 9100);
        partial.full_response = "partial".to_string();
        force_write_state(temp.path(), &partial);
        assert_eq!(
            request_inflight_abandon_if_matches_in_root(
                temp.path(),
                &ProviderKind::Claude,
                4244,
                9,
                "tok"
            ),
            GuardedClearOutcome::Cleared
        );

        assert!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty(),
            "both rows deleted (channels freed)"
        );
        assert!(
            super::super::abandon_request_store::load_pending(&ProviderKind::Claude, "tok")
                .is_empty(),
            "non-finalizable rows must NOT enqueue an abandon-request"
        );
    }

    /// Restart-mode / rebind-origin / newer-owner rows are PRESERVED: no enqueue,
    /// no delete (recovery owns their lifecycle).
    #[test]
    fn abandon_request_preserves_recovery_owned_and_newer_owner_rows() {
        let (_lock, temp, _env) = status_panel_test_root();

        let mut restart = spinner_row(4245, 8, 9001);
        restart.restart_mode = Some(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &restart).expect("seed restart");
        assert_eq!(
            request_inflight_abandon_if_matches_in_root(
                temp.path(),
                &ProviderKind::Claude,
                4245,
                8,
                "tok"
            ),
            GuardedClearOutcome::PlannedRestartSkipped
        );

        let mut rebind = spinner_row(4246, 0, 9002);
        rebind.rebind_origin = true;
        save_inflight_state_in_root(temp.path(), &rebind).expect("seed rebind");
        assert_eq!(
            request_inflight_abandon_if_matches_zero_owned_in_root(
                temp.path(),
                &ProviderKind::Claude,
                4246,
                "tok"
            ),
            GuardedClearOutcome::RebindOriginSkipped
        );

        let newer = spinner_row(4247, 99, 9003);
        save_inflight_state_in_root(temp.path(), &newer).expect("seed newer");
        assert_eq!(
            request_inflight_abandon_if_matches_in_root(
                temp.path(),
                &ProviderKind::Claude,
                4247,
                8, // stale signal for an older turn
                "tok"
            ),
            GuardedClearOutcome::UserMsgMismatch
        );

        // All three row FILES survive on disk (the helpers returned early without
        // deleting). Check paths directly — `load_inflight_states_from_root` GCs a
        // generation-mismatched restart row and filters rebind-origin rows on read.
        for ch in [4245u64, 4246, 4247] {
            assert!(
                inflight_state_path(temp.path(), &ProviderKind::Claude, ch).exists(),
                "preserved row {ch} must survive on disk"
            );
        }
        assert!(
            super::super::abandon_request_store::load_pending(&ProviderKind::Claude, "tok")
                .is_empty(),
            "preserved rows must NOT enqueue"
        );
    }

    /// #3859 r5 (codex P1): if the abandon-request fails to persist for a
    /// finalizable placeholder, the inflight row MUST be PRESERVED (outcome
    /// IoError) — never deleted without a record, which would re-strand the
    /// placeholder forever (the original #3859 bug on the error path).
    #[test]
    fn abandon_request_preserves_row_when_enqueue_fails() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Seed the inflight row in its own temp root.
        let inflight_temp = TempDir::new().unwrap();
        // Point AGENTDESK_ROOT_DIR (→ abandon-requests root) at a FILE so the
        // store's atomic_write fails → enqueue returns Err.
        let bad = TempDir::new().unwrap();
        let bad_root = bad.path().join("not_a_dir");
        std::fs::write(&bad_root, b"x").expect("seed file at root path");
        let _env = set_agentdesk_root_for_test(&bad_root);

        let state = spinner_row(4248, 8, 9001);
        save_inflight_state_in_root(inflight_temp.path(), &state).expect("seed inflight row");

        let outcome = request_inflight_abandon_if_matches_in_root(
            inflight_temp.path(),
            &ProviderKind::Claude,
            4248,
            8,
            "tok",
        );
        assert_eq!(
            outcome,
            GuardedClearOutcome::IoError,
            "enqueue failure must NOT delete the row"
        );
        assert!(
            inflight_state_path(inflight_temp.path(), &ProviderKind::Claude, 4248).exists(),
            "inflight row must be PRESERVED so the placeholder is not stranded"
        );
    }

    /// #3558 core: a streaming progress write must PRESERVE the on-disk
    /// `last_offset` (which a concurrent owner-gated refresh advanced) instead
    /// of clobbering it backward from a stale unlocked snapshot.
    #[test]
    fn watcher_stream_progress_preserves_concurrently_advanced_last_offset() {
        let temp = TempDir::new().unwrap();
        let channel_id = 35_580_001;
        let session = "AgentDesk-claude-3558-a";
        let state = seed_watcher_stream_state(temp.path(), channel_id, session, "hello", 100);
        let identity = InflightTurnIdentity::from_state(&state);

        // Concurrent owner-gated refresh advances the persisted watermark to 200
        // (simulating the race window between the old unlocked load and save).
        assert!(refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            &identity,
            Some(64),
            "/tmp/agentdesk-3558.jsonl",
            None,
            200,
            RelayOwnerKind::None,
        ));

        // The streaming caller (holding a stale last_offset == 100 implicitly)
        // patches only watcher-owned fields.
        let outcome = persist_watcher_stream_progress_locked_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            Some(&identity),
            session,
            WatcherStreamProgressPatch {
                current_msg_id: Some(43),
                full_response: "hello world".to_string(),
                response_sent_offset: 11,
                current_tool_line: None,
                prev_tool_status: None,
                task_notification_kind: None,
                any_tool_used: false,
                has_post_tool_text: false,
                streaming_rollover_frozen_msg_ids: Vec::new(),
            },
        );
        assert_eq!(outcome, WatcherProgressOutcome::Saved);

        let persisted = loaded_row(temp.path(), channel_id);
        assert_eq!(persisted.full_response, "hello world");
        assert_eq!(persisted.response_sent_offset, 11);
        assert_eq!(
            persisted.last_offset, 200,
            "last_offset must be preserved at the concurrently-advanced value, NOT clobbered to 100"
        );
    }

    /// #3558: a streaming write must be SKIPPED when a fresh turn (row B) with a
    /// different `turn_start_offset` replaced the row mid-frame — the identity
    /// guard rejects it and leaves row B untouched.
    #[test]
    fn watcher_stream_progress_skips_on_fresh_row_identity_mismatch() {
        let temp = TempDir::new().unwrap();
        let channel_id = 35_580_002;
        let session = "AgentDesk-claude-3558-b";
        let state = seed_watcher_stream_state(temp.path(), channel_id, session, "old", 50);
        let stale_identity = InflightTurnIdentity::from_state(&state);

        // A fresh turn B replaces the row (different turn_start_offset). A legit
        // fresh-turn reset lowers last_offset/offset on purpose, so seed it via a
        // direct write (the on-disk pre-condition the helper must reject).
        let mut fresh = state.clone();
        fresh.turn_start_offset = Some(999);
        fresh.full_response = "fresh".to_string();
        fresh.response_sent_offset = "fresh".len();
        fresh.last_offset = 0;
        force_write_state(temp.path(), &fresh);

        let outcome = persist_watcher_stream_progress_locked_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            Some(&stale_identity),
            session,
            WatcherStreamProgressPatch {
                current_msg_id: Some(43),
                full_response: "stale write".to_string(),
                response_sent_offset: 11,
                current_tool_line: None,
                prev_tool_status: None,
                task_notification_kind: None,
                any_tool_used: false,
                has_post_tool_text: false,
                streaming_rollover_frozen_msg_ids: Vec::new(),
            },
        );
        assert_eq!(outcome, WatcherProgressOutcome::Skipped);

        let persisted = loaded_row(temp.path(), channel_id);
        assert_eq!(
            persisted.full_response, "fresh",
            "fresh row B must be untouched by the stale streaming write"
        );
        assert_eq!(persisted.turn_start_offset, Some(999));
    }

    /// #3558: the terminal-commit RMW max-serializes `last_offset` /
    /// `response_sent_offset` / `full_response` — a late commit observing a NEWER
    /// disk watermark (a concurrent stream persisted a longer body / larger
    /// offset) must not move any of them backward. The commit owns the fields but
    /// clamps up, keeping the longer already-relayed body so nothing is truncated.
    #[test]
    fn watcher_terminal_commit_max_serializes_backward_offsets() {
        let temp = TempDir::new().unwrap();
        let channel_id = 35_580_003;
        let session = "AgentDesk-claude-3558-c";
        // Disk carries a LONGER already-streamed body + watermark than the
        // (stale) commit — the concurrent-advance pre-condition. Seed via a
        // direct write since this is the on-disk state the commit must handle.
        let long_body = "delivered body plus a much longer already-relayed tail";
        let mut state = seed_watcher_stream_state(temp.path(), channel_id, session, long_body, 300);
        state.response_sent_offset = long_body.len();
        force_write_state(temp.path(), &state);
        let identity = InflightTurnIdentity::from_state(&state);

        // Commit arrives with a SMALLER last_offset (250 < disk 300) and a
        // SHORTER body than disk.
        let outcome = commit_watcher_terminal_delivery_locked_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            &identity,
            session,
            WatcherTerminalCommitPatch {
                full_response: "delivered body".to_string(),
                last_offset: 250,
                last_watcher_relayed_offset: Some(64),
                last_watcher_relayed_generation_mtime_ns: Some(7),
            },
        );
        assert_eq!(outcome, WatcherTerminalCommitOutcome::Committed);

        let persisted = loaded_row(temp.path(), channel_id);
        assert!(persisted.terminal_delivery_committed);
        assert_eq!(
            persisted.last_offset, 300,
            "backward commit last_offset must clamp UP to the disk watermark"
        );
        assert_eq!(
            persisted.full_response, long_body,
            "the longer already-relayed body must NOT be truncated by a shorter stale commit"
        );
        assert_eq!(
            persisted.response_sent_offset,
            long_body.len(),
            "response_sent_offset must clamp UP to the longer body length, never backward"
        );
        assert!(
            persisted.response_sent_offset <= persisted.full_response.len(),
            "response_sent_offset must stay in bounds"
        );
    }

    /// #3558: a forward commit (larger watermark than disk) advances normally —
    /// the max-serialize is a no-op when the commit is the authoritative tip.
    #[test]
    fn watcher_terminal_commit_advances_forward_offset() {
        let temp = TempDir::new().unwrap();
        let channel_id = 35_580_004;
        let session = "AgentDesk-claude-3558-d";
        let state = seed_watcher_stream_state(temp.path(), channel_id, session, "body", 100);
        let identity = InflightTurnIdentity::from_state(&state);

        let outcome = commit_watcher_terminal_delivery_locked_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            &identity,
            session,
            WatcherTerminalCommitPatch {
                full_response: "delivered response".to_string(),
                last_offset: 256,
                last_watcher_relayed_offset: Some(64),
                last_watcher_relayed_generation_mtime_ns: Some(9),
            },
        );
        assert_eq!(outcome, WatcherTerminalCommitOutcome::Committed);

        let persisted = loaded_row(temp.path(), channel_id);
        assert_eq!(persisted.last_offset, 256);
        assert_eq!(persisted.full_response, "delivered response");
        assert_eq!(
            persisted.response_sent_offset,
            "delivered response".len(),
            "forward commit sets response_sent_offset to the committed body length"
        );
        assert_eq!(persisted.last_watcher_relayed_offset, Some(64));
        assert_eq!(persisted.last_watcher_relayed_generation_mtime_ns, Some(9));
    }

    /// #3558 (codex review follow-up): the two `tmux_watcher.rs`
    /// session-bound-relay-success sites only mean to advance the relay
    /// watermark, but the OLD unlocked `load_inflight_state` → mutate →
    /// `save_inflight_state(&inflight)` re-wrote the WHOLE stale row — including a
    /// `last_offset`/`response_sent_offset`/`full_response` that a concurrent
    /// owner-gated refresh had since advanced — reintroducing the exact
    /// backward-write TOCTOU. The locked relay-watermark RMW must patch ONLY
    /// `last_watcher_relayed_*` and PRESERVE the concurrently-advanced disk
    /// watermark.
    #[test]
    fn watcher_relay_watermark_preserves_concurrently_advanced_last_offset() {
        let temp = TempDir::new().unwrap();
        let channel_id = 35_580_007;
        let session = "AgentDesk-claude-3558-g";
        // Disk carries a SHORT body the relay observed when it loaded.
        let state = seed_watcher_stream_state(temp.path(), channel_id, session, "hello", 100);
        let identity = InflightTurnIdentity::from_state(&state);

        // Between the relay's (now-removed) unlocked load and its save, a
        // concurrent owner-gated refresh advances the watermark to 200 AND a
        // concurrent stream lengthens the body — the race window the old
        // load→save clobbered. Seed via a direct write (the on-disk pre-condition
        // the helper must handle, not produce).
        assert!(refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            &identity,
            Some(64),
            "/tmp/agentdesk-3558.jsonl",
            None,
            200,
            RelayOwnerKind::None,
        ));
        let mut advanced = loaded_row(temp.path(), channel_id);
        advanced.full_response = "hello world longer body".to_string();
        advanced.response_sent_offset = advanced.full_response.len();
        force_write_state(temp.path(), &advanced);

        // The relay-success site patches only the watcher watermark.
        let outcome = persist_watcher_relay_watermark_locked_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            &identity,
            session,
            WatcherRelayWatermarkPatch {
                last_watcher_relayed_offset: Some(64),
                last_watcher_relayed_generation_mtime_ns: Some(11),
            },
        );
        assert_eq!(outcome, WatcherRelayWatermarkOutcome::Saved);

        let persisted = loaded_row(temp.path(), channel_id);
        assert_eq!(persisted.last_watcher_relayed_offset, Some(64));
        assert_eq!(persisted.last_watcher_relayed_generation_mtime_ns, Some(11));
        assert_eq!(
            persisted.last_offset, 200,
            "last_offset must be preserved at the concurrently-advanced value, NOT clobbered back to the relay's stale 100"
        );
        assert_eq!(
            persisted.full_response, "hello world longer body",
            "the concurrently-advanced body must NOT be re-written back to the relay's stale snapshot"
        );
        assert_eq!(
            persisted.response_sent_offset,
            "hello world longer body".len(),
            "response_sent_offset must stay at the concurrently-advanced value, never backward"
        );
        assert!(
            !persisted.terminal_delivery_committed,
            "the relay-watermark write must NOT set terminal_delivery_committed (commit owns that)"
        );
    }

    /// #3558 (codex review follow-up): a relay-watermark write must be SKIPPED
    /// when a fresh turn (row B) with a different `turn_start_offset` replaced
    /// the row between the relay's load and save — the identity guard rejects it
    /// and leaves row B untouched, so a late relay can never stamp its stale
    /// watermark over a newer turn.
    #[test]
    fn watcher_relay_watermark_skips_on_fresh_row_identity_mismatch() {
        let temp = TempDir::new().unwrap();
        let channel_id = 35_580_008;
        let session = "AgentDesk-claude-3558-h";
        let state = seed_watcher_stream_state(temp.path(), channel_id, session, "old", 50);
        let stale_identity = InflightTurnIdentity::from_state(&state);

        let mut fresh = state.clone();
        fresh.turn_start_offset = Some(999);
        fresh.full_response = "fresh".to_string();
        fresh.response_sent_offset = "fresh".len();
        fresh.last_offset = 0;
        fresh.last_watcher_relayed_offset = None;
        force_write_state(temp.path(), &fresh);

        let outcome = persist_watcher_relay_watermark_locked_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            &stale_identity,
            session,
            WatcherRelayWatermarkPatch {
                last_watcher_relayed_offset: Some(64),
                last_watcher_relayed_generation_mtime_ns: Some(7),
            },
        );
        assert_eq!(outcome, WatcherRelayWatermarkOutcome::Skipped);

        let persisted = loaded_row(temp.path(), channel_id);
        assert_eq!(
            persisted.turn_start_offset,
            Some(999),
            "fresh row B must be untouched by the stale relay-watermark write"
        );
        assert_eq!(
            persisted.last_watcher_relayed_offset, None,
            "fresh row B's relay watermark must not be stamped by the stale relay"
        );
        assert_eq!(persisted.last_offset, 0);
    }

    /// #3558 (Gemini retry non-destruction): after a same-turn retry reset
    /// (full_response="", response_sent_offset=0), a streaming write that itself
    /// carries the reset (empty body) must NOT pull the offset back up via any
    /// blanket max-merge — the patched value is preserved exactly and stays
    /// in-bounds.
    #[test]
    fn watcher_stream_progress_preserves_gemini_retry_reset() {
        let temp = TempDir::new().unwrap();
        let channel_id = 35_580_005;
        let session = "AgentDesk-claude-3558-e";
        let mut state =
            seed_watcher_stream_state(temp.path(), channel_id, session, "first attempt", 100);
        let identity = InflightTurnIdentity::from_state(&state);

        // Legitimate same-turn retry reset (mirrors reset_gemini_retry_attempt_state).
        // A reset lowers full_response/offset to 0 on purpose for the SAME turn
        // identity — the bridge persists it; seed it via a direct write so the
        // intentional backward reset does not trip the test-only monotonic
        // tripwire (the production save records it OBSERVE-ONLY, never skips).
        state.full_response = String::new();
        state.response_sent_offset = 0;
        force_write_state(temp.path(), &state);

        // Watcher streams the retried turn from empty; the patch carries the
        // post-reset body. No blanket max-merge: the offset follows the patch.
        let outcome = persist_watcher_stream_progress_locked_in_root(
            temp.path(),
            &ProviderKind::Claude,
            channel_id,
            Some(&identity),
            session,
            WatcherStreamProgressPatch {
                current_msg_id: Some(43),
                full_response: "retry body".to_string(),
                response_sent_offset: 10,
                current_tool_line: None,
                prev_tool_status: None,
                task_notification_kind: None,
                any_tool_used: false,
                has_post_tool_text: false,
                streaming_rollover_frozen_msg_ids: Vec::new(),
            },
        );
        assert_eq!(outcome, WatcherProgressOutcome::Saved);

        let persisted = loaded_row(temp.path(), channel_id);
        assert_eq!(persisted.full_response, "retry body");
        assert_eq!(
            persisted.response_sent_offset, 10,
            "post-reset offset must follow the patch, not be pulled back up to the pre-reset value"
        );
        assert!(
            persisted.response_sent_offset <= persisted.full_response.len(),
            "response_sent_offset must stay in bounds after a retry reset"
        );
    }

    /// #3933 (release-path coverage): under authority-ON — the live release config
    /// (`AGENTDESK_DELIVERY_RECORD_AUTHORITY=1`) — the LEGITIMATE Gemini/Qwen
    /// `RetryBoundary` reset (`full_response` cleared + `response_sent_offset`→0 for
    /// the SAME turn identity to re-stream) must NOT be enforce-skipped, so the
    /// re-streamed answer survives. Before the `is_legitimate_full_reset` carve-out
    /// the coarse guard returned `false` here and dropped the body live. The suite
    /// default is authority-OFF, so this path is only exercised by forcing the flag
    /// ON via the per-thread test seam.
    #[test]
    fn authority_on_permits_legit_retry_reset_3933() {
        use crate::services::discord::outbound::delivery_record as dr;
        // Share the panic-hook serialization the other tripwire tests use.
        let _serialized = monotonic_3358_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new().unwrap();
        let channel_id = 39_330_101;
        // Existing on-disk row: a streamed answer with an advanced frontier.
        let mut reset = seed_watcher_stream_state(
            temp.path(),
            channel_id,
            "AgentDesk-claude-3933a",
            "streamed answer body",
            120,
        );
        // The RetryBoundary reset rewinds the SAME turn to re-stream from empty.
        reset.full_response = String::new();
        reset.response_sent_offset = 0;
        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, channel_id);

        // Force authority-ON for THIS thread only (the release config).
        let _authority = dr::authority_test_seam::force(true);
        let verdict = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_inflight_state_for_save(
                temp.path(),
                &path,
                &reset,
                "src/services/discord/inflight.rs:test",
            )
        }));
        // The reset is a same-turn backward move. The debug tripwire relaxes ONLY
        // for an enforce-SKIP, and this write is PERMITTED (not skipped), so in
        // debug it fires — the panic itself witnesses "permitted, not
        // enforce-skipped" (a wrongly-blocked reset would relax the tripwire and
        // return `false` WITHOUT panicking, failing this assert). In release the
        // tripwire is compiled out, `validate` returns `true`, and the reset
        // persists end-to-end.
        match verdict {
            Ok(permitted) => {
                assert!(
                    permitted,
                    "authority-ON must PERMIT the legitimate retry reset, not enforce-skip it"
                );
                save_inflight_state_in_root(temp.path(), &reset).unwrap();
                let persisted = loaded_row(temp.path(), channel_id);
                assert!(
                    persisted.full_response.is_empty(),
                    "the permitted retry reset must persist (cleared body survives the re-stream)"
                );
                assert_eq!(persisted.response_sent_offset, 0);
            }
            Err(_) => assert!(
                cfg!(debug_assertions),
                "a panic is only expected from the debug tripwire on the permitted backward move"
            ),
        }
    }

    /// #3933 (release-path coverage): a GENUINE stale-snapshot backward regression
    /// — a NON-EMPTY body moving the frontier back for the SAME turn — must STILL
    /// be enforce-skipped under authority-ON. The reset carve-out must not weaken
    /// the real guard, so the committed answer already on disk survives untouched.
    /// The skipped case is exactly what the #3933 tripwire relaxation exempts, so
    /// this runs cleanly (no panic) even in debug builds.
    #[test]
    fn authority_on_still_skips_nonempty_stale_backward_write_3933() {
        use crate::services::discord::outbound::delivery_record as dr;
        let temp = TempDir::new().unwrap();
        let channel_id = 39_330_102;
        let committed_body = "the full committed answer";
        let streamed = seed_watcher_stream_state(
            temp.path(),
            channel_id,
            "AgentDesk-claude-3933b",
            committed_body,
            200,
        );
        // A stale snapshot for the SAME turn: non-empty but SHORTER body, backward
        // rso and last_offset. NOT the reset signature (the body is non-empty).
        let mut stale = streamed.clone();
        stale.full_response = "stale".to_string();
        stale.response_sent_offset = 3;
        stale.last_offset = 50;
        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, channel_id);

        let _authority = dr::authority_test_seam::force(true);
        assert!(
            !validate_inflight_state_for_save(
                temp.path(),
                &path,
                &stale,
                "src/services/discord/inflight.rs:test",
            ),
            "authority-ON must still enforce-skip a non-empty stale backward write"
        );
        // Driving the honor path, the committed body on disk must survive untouched.
        save_inflight_state_in_root(temp.path(), &stale).unwrap();
        let persisted = loaded_row(temp.path(), channel_id);
        assert_eq!(
            persisted.full_response, committed_body,
            "the enforce-skipped stale write must not clobber the committed answer"
        );
        assert_eq!(persisted.response_sent_offset, committed_body.len());
    }

    // ---- #3077: typed status-panel ownership write tests ----

    /// Seeds a single inflight row in `root` and returns it. `user_msg_id` /
    /// `current_msg_id` / `status_message_id` are caller-controlled so the
    /// guard semantics can be exercised.
    fn status_panel_test_state(
        channel_id: u64,
        user_msg_id: u64,
        current_msg_id: u64,
        tmux_session_name: Option<&str>,
        status_message_id: Option<u64>,
    ) -> InflightTurnState {
        serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": "claude",
            "channel_id": channel_id,
            "channel_name": "adk-claude",
            "request_owner_user_id": user_msg_id,
            "user_msg_id": user_msg_id,
            "current_msg_id": current_msg_id,
            "current_msg_len": 0,
            "status_message_id": status_message_id,
            "user_text": "hello",
            "source": "text",
            "session_id": "session-1",
            "tmux_session_name": tmux_session_name,
            "output_path": "/tmp/out.jsonl",
            "input_fifo_path": "/tmp/in.fifo",
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-01-01 00:00:00",
            "updated_at": "2026-01-01 00:00:00"
        }))
        .expect("status-panel test inflight state")
    }

    fn seed_status_panel_state(
        root: &Path,
        channel_id: u64,
        user_msg_id: u64,
        current_msg_id: u64,
        tmux_session_name: Option<&str>,
        status_message_id: Option<u64>,
    ) -> InflightTurnState {
        let state = status_panel_test_state(
            channel_id,
            user_msg_id,
            current_msg_id,
            tmux_session_name,
            status_message_id,
        );
        save_inflight_state_in_root(root, &state).expect("seed inflight state");
        state
    }

    fn loaded_status_message_id(root: &Path, channel_id: u64) -> Option<u64> {
        load_inflight_states_from_root(root, &ProviderKind::Claude)
            .into_iter()
            .find(|s| s.channel_id == channel_id)
            .and_then(|s| s.status_message_id)
    }

    fn loaded_status_panel_generation(root: &Path, channel_id: u64) -> Option<u64> {
        load_inflight_states_from_root(root, &ProviderKind::Claude)
            .into_iter()
            .find(|s| s.channel_id == channel_id)
            .map(|s| s.status_panel_generation)
    }

    #[test]
    fn bind_status_panel_sets_id_when_unguarded() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        seed_status_panel_state(temp.path(), 7001, 10, 11, Some("AgentDesk-claude-a"), None);

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7001,
            5555,
            &StatusPanelBindGuard::default(),
        );

        assert!(outcome.is_bound());
        assert_eq!(loaded_status_message_id(temp.path(), 7001), Some(5555));
    }

    #[test]
    fn bind_status_panel_opens_generation_epoch_only_on_fresh_bind() {
        // #3805 P2 (PR-C): the two-message watcher create stamps the panel
        // generation atomically with the fresh bind. A DEFAULT (OFF) bind never
        // touches it (byte-identical); an `AlreadyBound` re-bind never re-opens it.
        let (_lock, temp, _env_reset) = status_panel_test_root();

        // OFF / default guard → generation untouched (stays 0).
        seed_status_panel_state(temp.path(), 7101, 10, 11, Some("AgentDesk-claude-a"), None);
        let off = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7101,
            5555,
            &StatusPanelBindGuard::default(),
        );
        assert!(off.is_bound());
        assert_eq!(loaded_status_panel_generation(temp.path(), 7101), Some(0));

        // ON / fresh bind → generation bumped from the row under the bind lock.
        seed_status_panel_state(temp.path(), 7102, 10, 11, Some("AgentDesk-claude-a"), None);
        let fresh = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7102,
            6666,
            &StatusPanelBindGuard {
                bump_status_panel_generation: true,
                ..Default::default()
            },
        );
        assert_eq!(fresh.bound_status_panel_generation(), Some(1));
        assert_eq!(loaded_status_message_id(temp.path(), 7102), Some(6666));
        assert_eq!(loaded_status_panel_generation(temp.path(), 7102), Some(1));

        // ON / same-id re-bind → AlreadyBound, epoch NOT re-opened (stays 1).
        let again = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7102,
            6666,
            &StatusPanelBindGuard {
                bump_status_panel_generation: true,
                ..Default::default()
            },
        );
        assert_eq!(again, StatusPanelBindOutcome::AlreadyBound);
        assert_eq!(loaded_status_panel_generation(temp.path(), 7102), Some(1));
    }

    #[test]
    fn reanchor_bind_requires_old_panel_and_bumps_generation_from_disk() {
        // #3805 P2 (PR-D): overlapping same-identity re-anchor frames must not
        // both overwrite the row with caller-computed `seed + 1`. The old panel
        // id is the CAS compare, and the generation bump is computed from the
        // row while the flock is held.
        let (_lock, temp, _env_reset) = status_panel_test_root();
        let mut state = seed_status_panel_state(
            temp.path(),
            7103,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5000),
        );
        state.status_panel_generation = 1;
        save_inflight_state_in_root(temp.path(), &state).expect("seed generation");
        let identity = InflightTurnIdentity::from_state(&state);

        let first = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            6000,
            &StatusPanelBindGuard {
                require_identity: Some(identity.clone()),
                require_current_status_message_id: Some(5000),
                bump_status_panel_generation: true,
                ..Default::default()
            },
        );
        assert_eq!(first.bound_status_panel_generation(), Some(2));
        assert_eq!(loaded_status_message_id(temp.path(), 7103), Some(6000));
        assert_eq!(loaded_status_panel_generation(temp.path(), 7103), Some(2));

        let second = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            7000,
            &StatusPanelBindGuard {
                require_identity: Some(identity),
                require_current_status_message_id: Some(5000),
                bump_status_panel_generation: true,
                ..Default::default()
            },
        );
        assert_eq!(second, StatusPanelBindOutcome::GuardMismatch);
        assert_eq!(loaded_status_message_id(temp.path(), 7103), Some(6000));
        assert_eq!(loaded_status_panel_generation(temp.path(), 7103), Some(2));
    }

    #[test]
    fn bind_status_panel_is_idempotent_when_already_bound() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        seed_status_panel_state(
            temp.path(),
            7002,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7002,
            5555,
            &StatusPanelBindGuard::default(),
        );

        assert_eq!(outcome, StatusPanelBindOutcome::AlreadyBound);
        assert_eq!(loaded_status_message_id(temp.path(), 7002), Some(5555));
    }

    #[test]
    fn bind_status_panel_respects_user_msg_id_guard() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        seed_status_panel_state(temp.path(), 7003, 10, 11, Some("AgentDesk-claude-a"), None);

        // Guard expects a different user_msg_id (a newer turn now owns the row).
        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7003,
            5555,
            &StatusPanelBindGuard {
                require_user_msg_id: Some(99),
                ..Default::default()
            },
        );

        assert_eq!(outcome, StatusPanelBindOutcome::GuardMismatch);
        assert_eq!(loaded_status_message_id(temp.path(), 7003), None);
    }

    #[test]
    fn bind_status_panel_skips_when_real_panel_already_set() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        // A real (non-synthetic) panel id already on the row.
        seed_status_panel_state(
            temp.path(),
            7004,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(4242),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7004,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        // Carries the row's owned (DIFFERENT) panel id so the caller can adopt it.
        assert_eq!(
            outcome,
            StatusPanelBindOutcome::SkippedPanelAlreadySet(4242)
        );
        // Canonical panel preserved — not overwritten by the duplicate.
        assert_eq!(loaded_status_message_id(temp.path(), 7004), Some(4242));
    }

    #[test]
    fn bind_status_panel_same_id_is_already_bound_even_with_skip_flag() {
        // #3077 (codex P2 #1): an idempotent re-bind of the SAME panel id the row
        // already owns must classify as `AlreadyBound`, NOT
        // `SkippedPanelAlreadySet`, even when `skip_if_panel_already_set` is set.
        // Misclassifying it routed the TUI-direct caller to DELETE its own panel.
        let (_lock, temp, _env_reset) = status_panel_test_root();
        seed_status_panel_state(
            temp.path(),
            7007,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7007,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        assert_eq!(outcome, StatusPanelBindOutcome::AlreadyBound);
        assert_eq!(loaded_status_message_id(temp.path(), 7007), Some(5555));
    }

    #[test]
    fn bind_status_panel_different_id_skips_and_reports_owned_id() {
        // A DIFFERENT real panel id already set + skip flag → SkippedPanelAlreadySet
        // carrying the row's owned id (so the caller adopts the real panel).
        let (_lock, temp, _env_reset) = status_panel_test_root();
        seed_status_panel_state(
            temp.path(),
            7008,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(4242),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7008,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        assert_eq!(
            outcome,
            StatusPanelBindOutcome::SkippedPanelAlreadySet(4242)
        );
        assert_eq!(loaded_status_message_id(temp.path(), 7008), Some(4242));
    }

    #[test]
    fn bind_status_panel_overwrites_synthetic_even_with_skip_flag() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        // A synthetic-headless id does NOT count as "already set".
        seed_status_panel_state(
            temp.path(),
            7005,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(crate::services::discord::SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR + 1),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7005,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        assert!(outcome.is_bound());
        assert_eq!(loaded_status_message_id(temp.path(), 7005), Some(5555));
    }

    #[test]
    fn bind_status_panel_missing_row_reports_missing() {
        let temp = TempDir::new().unwrap();
        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7006,
            5555,
            &StatusPanelBindGuard::default(),
        );
        assert_eq!(outcome, StatusPanelBindOutcome::Missing);
    }

    #[test]
    fn clear_status_panel_if_current_clears_on_match() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        seed_status_panel_state(
            temp.path(),
            7101,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7101,
            5555,
            &StatusPanelClearGuard::default(),
        );

        assert!(cleared);
        assert_eq!(loaded_status_message_id(temp.path(), 7101), None);
    }

    #[test]
    fn clear_status_panel_if_current_preserves_newer_turns_panel_on_mismatch() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        // A newer turn already rebound the panel to 9999; a stale actor still
        // believes it owns 5555 and asks to clear it. The compare-and-clear
        // must NOT wipe the newer turn's panel.
        seed_status_panel_state(
            temp.path(),
            7102,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(9999),
        );

        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7102,
            5555,
            &StatusPanelClearGuard::default(),
        );

        assert!(!cleared);
        assert_eq!(loaded_status_message_id(temp.path(), 7102), Some(9999));
    }

    #[test]
    fn clear_status_panel_if_current_respects_extra_guards() {
        let (_lock, temp, _env_reset) = status_panel_test_root();
        seed_status_panel_state(
            temp.path(),
            7103,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        // msg-id matches, but user_msg_id/current_msg_id/tmux guards point at a
        // different turn → must NOT clear.
        let user_mismatch = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_user_msg_id: Some(99),
                ..Default::default()
            },
        );
        assert!(!user_mismatch);

        let current_mismatch = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_current_msg_id: Some(99),
                ..Default::default()
            },
        );
        assert!(!current_mismatch);

        let tmux_mismatch = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_tmux_session_name: Some("AgentDesk-claude-other".to_string()),
                ..Default::default()
            },
        );
        assert!(!tmux_mismatch);

        assert_eq!(loaded_status_message_id(temp.path(), 7103), Some(5555));

        // All guards satisfied → clears.
        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_user_msg_id: Some(10),
                require_current_msg_id: Some(11),
                require_tmux_session_name: Some("AgentDesk-claude-a".to_string()),
            },
        );
        assert!(cleared);
        assert_eq!(loaded_status_message_id(temp.path(), 7103), None);
    }

    // ---- #3351: relay-placeholder (`current_msg_id`) compare-and-clear ----

    fn loaded_current_msg_id(root: &Path, channel_id: u64) -> Option<u64> {
        load_inflight_states_from_root(root, &ProviderKind::Claude)
            .into_iter()
            .find(|s| s.channel_id == channel_id)
            .map(|s| s.current_msg_id)
    }

    #[test]
    fn clear_current_msg_if_matches_clears_on_match() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7201,
            10,
            5555,
            Some("AgentDesk-claude-a"),
            None,
        );

        let cleared = clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7201,
            5555,
            Some("AgentDesk-claude-a"),
        );

        assert!(cleared);
        assert_eq!(loaded_current_msg_id(temp.path(), 7201), Some(0));
    }

    #[test]
    fn clear_current_msg_if_matches_preserves_newer_turn_on_mismatch() {
        let temp = TempDir::new().unwrap();
        // A newer turn already advanced `current_msg_id` to 9999; a stale actor
        // asking to clear 5555 must not touch it. A zero msg_id never matches.
        seed_status_panel_state(
            temp.path(),
            7202,
            10,
            9999,
            Some("AgentDesk-claude-a"),
            None,
        );

        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7202,
            5555,
            None,
        ));
        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7202,
            0,
            None,
        ));
        assert_eq!(loaded_current_msg_id(temp.path(), 7202), Some(9999));
    }

    #[test]
    fn clear_current_msg_if_matches_respects_tmux_guard() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7203,
            10,
            5555,
            Some("AgentDesk-claude-a"),
            None,
        );

        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7203,
            5555,
            Some("AgentDesk-claude-other"),
        ));
        assert_eq!(loaded_current_msg_id(temp.path(), 7203), Some(5555));
    }

    #[test]
    fn clear_current_msg_if_matches_preserves_placeholderless_turn() {
        let temp = TempDir::new().unwrap();
        // Placeholderless Discord turn: `current_msg_id` mirrors the user's own
        // message id — never clear it (adopt-guard mirror).
        seed_status_panel_state(
            temp.path(),
            7204,
            5555,
            5555,
            Some("AgentDesk-claude-a"),
            None,
        );

        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7204,
            5555,
            Some("AgentDesk-claude-a"),
        ));
        assert_eq!(loaded_current_msg_id(temp.path(), 7204), Some(5555));
    }

    #[test]
    fn clear_status_panel_if_current_noops_on_missing_row() {
        let temp = TempDir::new().unwrap();
        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7104,
            5555,
            &StatusPanelClearGuard::default(),
        );
        assert!(!cleared);
    }

    #[test]
    fn bind_then_clear_if_current_round_trips_atomically() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(temp.path(), 7200, 10, 11, Some("AgentDesk-claude-a"), None);

        // bind then clear-if-current with the same id returns the row to None,
        // mirroring the watcher publish → orphan-cleanup lifecycle through the
        // single locked store path.
        assert!(
            bind_status_panel_in_root(
                temp.path(),
                &ProviderKind::Claude,
                7200,
                5555,
                &StatusPanelBindGuard::default(),
            )
            .is_bound()
        );
        assert_eq!(loaded_status_message_id(temp.path(), 7200), Some(5555));

        assert!(clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7200,
            5555,
            &StatusPanelClearGuard::default(),
        ));
        assert_eq!(loaded_status_message_id(temp.path(), 7200), None);
    }

    #[test]
    fn runtime_kind_round_trips_and_direct_tui_has_no_fifo_requirement() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            77,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            12,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].runtime_kind, Some(RuntimeHandoffKind::ClaudeTui));
        assert_eq!(
            loaded[0].runtime_kind_for_recovery(),
            RuntimeHandoffKind::ClaudeTui
        );
        assert!(loaded[0].input_fifo_path.is_none());
        assert!(!loaded[0].runtime_kind_for_recovery().requires_input_fifo());
    }

    /// #2235 v8 compat shape: a ClaudeTui inflight row that carries both a
    /// stamped `runtime_kind` and a populated `input_fifo_path` must
    /// round-trip cleanly under `INFLIGHT_STATE_VERSION` = 8 so an old
    /// (pre-#2213) binary rolling back over the file can still satisfy its
    /// FIFO-required recovery branch.
    #[test]
    fn inflight_v8_claude_tui_round_trips_with_fifo_for_rollback_compat() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            55,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            Some("/tmp/claude-fifo.input".to_string()),
            12,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].version, super::INFLIGHT_STATE_VERSION);
        assert_eq!(loaded[0].version, 9);
        assert_eq!(loaded[0].runtime_kind, Some(RuntimeHandoffKind::ClaudeTui));
        assert_eq!(
            loaded[0].input_fifo_path.as_deref(),
            Some("/tmp/claude-fifo.input")
        );
        assert_eq!(
            loaded[0].runtime_kind_for_recovery(),
            RuntimeHandoffKind::ClaudeTui
        );
    }

    /// #2235: rows written by a newer binary may serialize an unknown
    /// `runtime_kind` string. `deserialize_runtime_kind_tolerant` must
    /// collapse the unknown value to `None` so the whole inflight row isn't
    /// tossed as malformed JSON. The recovery engine layers the
    /// version-aware silent-skip on top of this.
    #[test]
    fn inflight_unknown_runtime_kind_string_deserializes_as_none() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(ProviderKind::Claude.as_str());
        std::fs::create_dir_all(&dir).unwrap();

        // Seed a JSON row whose `runtime_kind` is a variant string this
        // binary does not know about (`"future_runtime"`). Without the
        // tolerant deserializer this row would be deleted as malformed by
        // `load_inflight_states_from_root`.
        let valid_state = InflightTurnState::new(
            ProviderKind::Claude,
            444,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        let mut value = serde_json::to_value(&valid_state).unwrap();
        value["runtime_kind"] = serde_json::Value::String("future_runtime".to_string());
        // Also bump the on-disk version to simulate a row authored by a
        // newer binary, so the recovery-engine silent-skip guard would
        // trigger downstream of this deserialization step.
        value["version"] =
            serde_json::Value::Number(serde_json::Number::from(super::INFLIGHT_STATE_VERSION + 1));
        let path = dir.join("444.json");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1, "tolerant deser must keep the row");
        assert_eq!(loaded[0].channel_id, 444);
        assert!(
            loaded[0].runtime_kind.is_none(),
            "unknown variant must collapse to None"
        );
        assert!(
            loaded[0].version > super::INFLIGHT_STATE_VERSION,
            "version stays forward-marked for the recovery silent-skip guard"
        );
        assert!(
            loaded[0].runtime_kind_unknown_on_disk,
            "present-but-unknown runtime_kind must be distinguishable from legacy absent-field None"
        );
    }

    /// #2235: legacy v7 rows have NO `runtime_kind` field on disk at all.
    /// These must deserialize with `runtime_kind = None` AND
    /// `runtime_kind_unknown_on_disk = false`, so the recovery silent-skip
    /// guard does not regress legacy recovery flows that depend on the
    /// `runtime_kind_for_recovery` heuristic.
    #[test]
    fn inflight_legacy_v7_row_with_absent_runtime_kind_recovers_via_heuristic() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(ProviderKind::Claude.as_str());
        std::fs::create_dir_all(&dir).unwrap();

        let valid_state = InflightTurnState::new(
            ProviderKind::Claude,
            555,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        let mut value = serde_json::to_value(&valid_state).unwrap();
        // Strip the runtime_kind field entirely to mimic an on-disk legacy
        // v7 row from before #2213.
        value.as_object_mut().unwrap().remove("runtime_kind");
        value["version"] = serde_json::Value::Number(serde_json::Number::from(7u32));
        let path = dir.join("555.json");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].runtime_kind.is_none());
        assert!(
            !loaded[0].runtime_kind_unknown_on_disk,
            "absent-field legacy v7 rows must not look like a forward-unknown row"
        );
        assert_eq!(loaded[0].version, 7);
    }

    /// #2235: when an on-disk row has `runtime_kind = None` (legacy pre-v8
    /// row or a future variant this binary doesn't know about) the
    /// `runtime_kind_for_recovery` heuristic must still pick a deterministic
    /// kind. The recovery engine layered on top of this then uses
    /// `state.runtime_kind.is_none()` to switch the missing-FIFO branch to a
    /// silent debug-skip — exercised here at the data-model layer.
    #[test]
    fn inflight_unknown_runtime_kind_falls_back_without_panic() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            66,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        // Simulate the pre-v8 / unknown-runtime case: no stamped runtime_kind
        // and no FIFO path. `runtime_kind_for_recovery` should fall back to
        // ClaudeTui because tmux/output are present, allowing recovery to
        // skip silently rather than synthesizing a missing-FIFO notice.
        state.runtime_kind = None;
        state.input_fifo_path = None;

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].runtime_kind.is_none());
        assert_eq!(
            loaded[0].runtime_kind_for_recovery(),
            RuntimeHandoffKind::ClaudeTui
        );
    }

    #[test]
    fn inflight_malformed_json_graceful_skip() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let dir = root.join(ProviderKind::Claude.as_str());
        std::fs::create_dir_all(&dir).unwrap();

        let valid_state = InflightTurnState::new(
            ProviderKind::Claude,
            111,
            Some("adk-claude".to_string()),
            222,
            333,
            444,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        let valid_path = dir.join("111.json");
        std::fs::write(&valid_path, serde_json::to_string(&valid_state).unwrap()).unwrap();

        let malformed_path = dir.join("999.json");
        std::fs::write(&malformed_path, "{ malformed json ]").unwrap();

        let loaded = load_inflight_states_from_root(root, &ProviderKind::Claude);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].channel_id, 111);
        assert!(valid_path.exists());
        assert!(!malformed_path.exists());
    }

    fn build_inflight_for_guard_tests(
        provider: ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
    ) -> InflightTurnState {
        InflightTurnState::new(
            provider,
            channel_id,
            Some("adk".to_string()),
            42,
            100,
            user_msg_id,
            "user prompt".to_string(),
            None,
            Some("AgentDesk-claude-adk".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    type EnvReset = crate::config::TestEnvVarGuard;

    fn set_agentdesk_root_for_test(path: &Path) -> EnvReset {
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            path,
        )
    }

    fn status_panel_test_root() -> (std::sync::MutexGuard<'static, ()>, TempDir, EnvReset) {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let env_reset = set_agentdesk_root_for_test(temp.path());
        (lock, temp, env_reset)
    }

    /// #2427 D/A wire — happy path. When the on-disk inflight has a
    /// matching `user_msg_id` and is neither a planned-restart marker
    /// nor a rebind origin, the explicit signal removes it.
    #[test]
    fn clear_inflight_state_if_matches_removes_matching_turn() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        let user_msg_id = state.user_msg_id;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            user_msg_id,
        );
        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    /// #3041 P1-3 (Part a, B1): the identity-guarded save must NOT let a stale write
    /// clobber a NEWER turn that has taken over the inflight row (e.g. a fast
    /// follow-up turn on the same channel between the watcher's compute and its
    /// write). A mismatched identity yields `IdentityMismatch` and the newer turn's
    /// row is preserved. (The frame-carried B1 commit fence removed the racy
    /// delegated-terminal-end inflight persist; this keeps the generic guard covered
    /// via a still-live field.)
    #[test]
    fn identity_guarded_save_rejects_stale_write_against_newer_turn() {
        let temp = TempDir::new().unwrap();
        // The original turn (user_msg_id = 100).
        let mut original = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        original.user_msg_id = 100;
        let original_identity = InflightTurnIdentity::from_state(&original);

        // A NEWER turn (distinct user_msg_id) now owns the row on disk.
        let mut newer = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 200);
        newer.user_msg_id = 200;
        save_inflight_state_in_root(temp.path(), &newer).unwrap();

        // Stale write under the OLD identity → must be rejected, leaving the newer
        // turn intact.
        let mut stale_persist = original.clone();
        stale_persist.last_watcher_relayed_offset = Some(256);
        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &stale_persist,
            &original_identity,
            original.turn_start_offset,
        );
        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);

        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_msg_id, 200, "newer turn must be preserved");
        assert_eq!(
            rows[0].last_watcher_relayed_offset, None,
            "the newer turn must NOT inherit the old turn's stale write"
        );
    }

    /// #2427 Pitfall #1 — stale TurnCompleted carrying the previous
    /// turn's `user_msg_id` must NOT delete the next turn's inflight.
    #[test]
    fn clear_inflight_state_if_matches_protects_next_turn_against_stale_signal() {
        let temp = TempDir::new().unwrap();
        let next_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        save_inflight_state_in_root(temp.path(), &next_turn).unwrap();

        // Stale completion for previous turn user_msg_id = 50 arrives now.
        let outcome =
            clear_inflight_state_if_matches_in_root(temp.path(), &ProviderKind::Claude, 321, 50);
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);

        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].user_msg_id, 100);
    }

    #[test]
    fn tmux_response_guard_clears_matching_delivered_idle_relay() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.full_response = "done from idle relay".to_string();
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_tmux_response_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            "AgentDesk-claude-adk",
            "done from idle relay",
        );

        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    #[test]
    fn tmux_response_guard_preserves_new_turn_with_different_response() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        state.user_msg_id = 101;
        state.full_response = String::new();
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_tmux_response_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            "AgentDesk-claude-adk",
            "previous idle relay response",
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].user_msg_id, 101);
    }

    #[test]
    fn identity_guard_preserves_same_named_respawn() {
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();
        let old_identity = InflightTurnIdentity::from_state(&old_turn);

        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.user_text = "fresh prompt".to_string();
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        let outcome = clear_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
        );
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);

        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].started_at, "2026-05-17 10:00:05");
        assert_eq!(
            still_there[0].tmux_session_name, old_turn.tmux_session_name,
            "test must cover same-named respawn"
        );
    }

    // #3419 R3 (codex MEDIUM): the plain identity-guarded clear must use the SAME
    // key as the timeout decision, which compares the FULL `InflightTurnIdentity`
    // (including `turn_start_offset`). Two rows that share user_msg_id + started_at
    // + tmux_session_name but differ only by `turn_start_offset` are DIFFERENT
    // turns (the offset disambiguates consecutive same-second turns). Clearing
    // against the OTHER offset must no-op so a stale clear cannot wipe the live
    // row. Dropping `turn_start_offset` from `matches_state` reopens this TOCTOU
    // and breaks this test.
    #[test]
    fn identity_guard_clear_respects_turn_start_offset() {
        let temp = TempDir::new().unwrap();
        let on_disk = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        // Same user_msg_id + started_at + session, but a DIFFERENT turn_start_offset.
        let mut stale_offset_identity = InflightTurnIdentity::from_state(&on_disk);
        assert_ne!(
            stale_offset_identity.turn_start_offset,
            Some(999),
            "guard fixture must differ from the probed offset"
        );
        stale_offset_identity.turn_start_offset = Some(999);

        let outcome = clear_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &stale_offset_identity,
        );
        assert_eq!(
            outcome,
            GuardedClearOutcome::UserMsgMismatch,
            "an offset-only-diff identity must NOT clear the live row"
        );
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(
            still_there.len(),
            1,
            "the live row survives the stale clear"
        );
        assert_eq!(still_there[0].turn_start_offset, on_disk.turn_start_offset);

        // Sanity: the matching offset DOES clear (the key is offset-sensitive, not
        // offset-blind).
        let matching_identity = InflightTurnIdentity::from_state(&on_disk);
        assert_eq!(
            clear_inflight_state_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Claude,
                321,
                &matching_identity,
            ),
            GuardedClearOutcome::Cleared,
            "the exact-offset identity clears the row"
        );
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    #[test]
    fn identity_delivery_clear_removes_matching_turn() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        let identity = InflightTurnIdentity::from_state(&state);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let (outcome, mirrored) = clear_inflight_state_if_matches_identity_after_delivery_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            state.turn_start_offset,
            "hello\nworld",
            "hello\nworld".len(),
            99,
        );

        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(mirrored);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    #[test]
    fn identity_delivery_clear_does_not_overwrite_fresh_turn() {
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();
        let old_identity = InflightTurnIdentity::from_state(&old_turn);

        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.user_text = "fresh prompt".to_string();
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        let (outcome, mirrored) = clear_inflight_state_if_matches_identity_after_delivery_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
            old_turn.turn_start_offset,
            "stale delivered response",
            "stale delivered response".len(),
            99,
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert!(!mirrored);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].started_at, fresh_turn.started_at);
        assert_eq!(still_there[0].user_text, "fresh prompt");
        assert!(still_there[0].full_response.is_empty());
        assert_eq!(still_there[0].response_sent_offset, 0);
    }

    #[test]
    fn identity_delivery_clear_checks_turn_start_offset() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        let identity = InflightTurnIdentity::from_state(&state);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let (outcome, mirrored) = clear_inflight_state_if_matches_identity_after_delivery_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            Some(999),
            "stale delivered response",
            "stale delivered response".len(),
            99,
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert!(!mirrored);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert!(still_there[0].full_response.is_empty());
        assert_eq!(still_there[0].response_sent_offset, 0);
    }

    #[test]
    fn identity_heartbeat_refresh_updates_matching_turn_under_lock() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        let identity = InflightTurnIdentity::from_state(&state);
        let output_path = state.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            state.turn_start_offset,
            &output_path,
            Some(state.current_msg_id),
            123,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 123);
    }

    #[test]
    fn identity_heartbeat_refresh_does_not_overwrite_fresh_turn() {
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.current_msg_id = 0;
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        let old_identity = InflightTurnIdentity::from_state(&old_turn);
        let output_path = old_turn.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();

        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.current_msg_id = 0;
        fresh_turn.user_msg_id = 101;
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.output_path = Some(output_path.clone());
        fresh_turn.last_offset = 20;
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
            old_turn.turn_start_offset,
            &output_path,
            None,
            123,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].user_msg_id, 101);
        assert_eq!(loaded[0].started_at, "2026-05-17 10:00:05");
        assert_eq!(loaded[0].last_offset, 20);
    }

    #[test]
    fn identity_heartbeat_refresh_rejects_backward_offset_same_identity() {
        // #3017 I6 (last_offset_monotonic): a backward watermark write for the
        // SAME turn identity is rejected so a stale transcript tail cannot be
        // replayed; the on-disk offset is left untouched.
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.last_offset = 200;
        let identity = InflightTurnIdentity::from_state(&state);
        let output_path = state.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            state.turn_start_offset,
            &output_path,
            Some(state.current_msg_id),
            150,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 200);
    }

    // #3358: pin the EXACT incident at the save level. The synthetic inflight's
    // birth offset is the relay-cursor base AND the `turn_start_offset` identity
    // key (`InflightTurnState::new`: turn_start_offset == last_offset, rso == 0).
    //
    // The on-disk row carries the watcher's COMMITTED frontier as last_offset
    // (the watcher is the single authority — #3017 `confirmed_end_offset`). When
    // the synthetic re-claims/refreshes the row, it writes its BIRTH offset back:
    //   * Pre-fix: birth == lagging `relay_last_offset()` (2821677) < committed
    //     frontier (2838484). Same identity (the watcher row is still keyed by
    //     this turn's anchor + this birth turn_start_offset) → BACKWARD write →
    //     the `last_offset` + `response_sent_offset` monotonicity guards fire (the
    //     incident's ERROR triple). This is the REPRODUCE witness.
    //   * Post-fix: `synthetic_start_offset_carry_forward` births the synthetic at
    //     the committed frontier (2838484) — equal to the on-disk last_offset, so
    //     the re-claim is a forward/equal write and NO invariant fires.
    fn build_synth_3358(birth: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            321,
            Some("adk-cc".to_string()),
            1,
            1_514_752_860_691_370_014, // synthetic user_msg_id (anchor id)
            9002,
            "new synthetic prompt".to_string(),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            birth,
        )
    }

    /// #3358: the two monotonicity tests catch panics via `catch_unwind` while
    /// sharing the process-global panic hook with every parallel test thread —
    /// serialize them so a sibling's hook traffic cannot interleave (rare
    /// parallel-run flake observed on loaded machines).
    fn monotonic_3358_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn synthetic_lagging_birth_reproduces_backward_regression_3358() {
        let _serialized = monotonic_3358_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // REPRODUCE: a lagging birth re-claim over the committed frontier is a
        // same-identity backward write that trips the monotonicity guard. This is
        // the pre-fix incident; it MUST still be flagged so the guard's protective
        // value is preserved for genuine regressions.
        let temp = TempDir::new().unwrap();
        let relay_last_offset: u64 = 2_821_677; // lagging birth (pre-fix).
        let committed_frontier: u64 = 2_838_484; // watcher confirmed delivery.

        // On-disk row: SAME identity as the lagging birth, last_offset already at
        // the committed frontier (the watcher advanced it forward).
        let mut on_disk = build_synth_3358(relay_last_offset);
        on_disk.full_response = "X".repeat(20_000);
        on_disk.response_sent_offset = 18_000;
        on_disk.last_offset = committed_frontier;
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        // Lagging-birth re-claim: turn_start_offset == last_offset == lagging
        // (2821677), rso == 0 — BACKWARD vs the committed frontier, same identity.
        let lagging_reseed = build_synth_3358(relay_last_offset);
        assert_eq!(lagging_reseed.turn_start_offset, Some(relay_last_offset));
        let lag = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_inflight_state_for_save(
                temp.path(),
                &inflight_state_path(temp.path(), &ProviderKind::Claude, 321),
                &lagging_reseed,
                "src/services/discord/inflight.rs:test",
            );
        }));
        assert!(
            lag.is_err() || cfg!(not(debug_assertions)),
            "lagging-birth re-claim must trip the monotonicity guard (incident + genuine-regression witness)"
        );
    }

    #[test]
    fn synthetic_carry_forward_skipped_on_generation_mismatch_3358() {
        // #3358 round 2 — Finding 1 guard, at the production-helper level.
        //
        // After a tmux wrapper RESTART the output stream legitimately resets to
        // offset 0. The committed watermark from the PREVIOUS generation is stale;
        // the claim choke-point detects the generation mismatch and passes `None`
        // for the committed frontier. The helper MUST then fall back to the
        // synthetic's own (lagging) birth offset — NOT lift it over the stale
        // watermark, which would treat future bytes below that watermark as
        // already delivered (CONTENT SKIP, strictly worse than the original
        // ERROR-only bug). On HEAD (helper took a bare `u64` and always clamped)
        // there was no way to express "stale → do not clamp", so this guard
        // failed.
        let relay_last_offset: u64 = 100; // fresh post-restart birth (lagging).
        let stale_frontier: u64 = 2_838_484; // pre-restart, NUMERICALLY higher.

        // Generation mismatch → `None`: NO clamp, born at its own cursor.
        let birth =
            crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
                relay_last_offset,
                None,
            );
        assert_eq!(
            birth, relay_last_offset,
            "a stale (different-generation) watermark must NOT clamp the new synthetic forward"
        );

        // Same generation → `Some(..)`: clamp DOES carry the frontier forward.
        let same_gen_birth =
            crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
                relay_last_offset,
                Some(stale_frontier),
            );
        assert_eq!(
            same_gen_birth, stale_frontier,
            "a same-generation committed frontier must still carry forward (the #3358 fix)"
        );
    }

    #[test]
    fn identity_heartbeat_refresh_advances_forward_offset_same_identity() {
        // #3017 I6: a forward watermark write for the same identity advances.
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.last_offset = 200;
        let identity = InflightTurnIdentity::from_state(&state);
        let output_path = state.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            state.turn_start_offset,
            &output_path,
            Some(state.current_msg_id),
            250,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 250);
    }

    #[test]
    fn identity_heartbeat_refresh_standby_yields_to_watcher_owner() {
        // #3017 I6 (last_offset_owner_gated): a StandbyRelay caller must not
        // advance the watermark while the persisted owner is the live Watcher.
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.last_offset = 100;
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        let identity = InflightTurnIdentity::from_state(&state);
        let output_path = state.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            state.turn_start_offset,
            &output_path,
            Some(state.current_msg_id),
            250,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 100);
    }

    #[test]
    fn identity_heartbeat_refresh_allows_fresh_turn_reset() {
        // #3017 I6 fresh-turn exemption: a NEW turn identity legitimately
        // resets the watermark to a smaller offset; the identity guards reject
        // the refresh BEFORE the monotonic clamp ever runs, so the standby
        // caller simply does not clobber the new turn.
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.current_msg_id = 0;
        old_turn.user_msg_id = 100;
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        old_turn.last_offset = 500;
        old_turn.turn_start_offset = Some(0);
        let old_identity = InflightTurnIdentity::from_state(&old_turn);
        let output_path = old_turn.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();

        // A fresh turn: new user_msg_id AND a new turn_start_offset that
        // legitimately resets the watermark to a smaller value.
        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.current_msg_id = 0;
        fresh_turn.user_msg_id = 101;
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.output_path = Some(output_path.clone());
        fresh_turn.last_offset = 10;
        fresh_turn.turn_start_offset = Some(10);
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        // The standby caller still believes it is the OLD turn; the identity
        // guards reject it, leaving the fresh turn's small offset intact.
        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
            old_turn.turn_start_offset,
            &output_path,
            None,
            123,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].user_msg_id, 101);
        assert_eq!(loaded[0].last_offset, 10);
    }

    #[test]
    fn offset_monotonic_severity_downgrades_only_when_enforce_skips() {
        use crate::services::observability::InvariantSeverity;
        // #3552: when the #3416 enforce guard will skip the backward write the
        // violation is already handled → WARN (no ERROR noise). When it will NOT
        // skip (enforce OFF → write persists) it stays ERROR (a genuine breach).
        assert_eq!(
            offset_monotonic_invariant_severity(true),
            InvariantSeverity::Warn
        );
        assert_eq!(
            offset_monotonic_invariant_severity(false),
            InvariantSeverity::Error
        );
    }

    #[test]
    fn validate_save_records_backward_last_offset_violation_same_identity() {
        // #3017 I6 OBSERVE-ONLY on the save path: a backward last_offset for
        // the same turn identity records a `last_offset_monotonic` violation
        // (and trips the debug_assert) but does NOT skip the write — a legit
        // fresh-turn reset must still be able to persist.
        //
        // Force authority OFF so this test pins the original observe-only
        // branch regardless of a release-like process env. Authority ON is the
        // #3416 enforce branch: the same violation is recorded, then the write is
        // skipped and #3933 intentionally relaxes the debug tripwire.
        let _authority =
            crate::services::discord::outbound::delivery_record::authority_test_seam::force(false);
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.last_offset = 300;
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // Same identity (user_msg_id + turn_start_offset) but a backward
        // last_offset → records a violation. Run with the debug_assert
        // disabled by catching the panic so we can assert observability fired.
        let mut backward = existing.clone();
        backward.last_offset = 100;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_inflight_state_for_save(
                temp.path(),
                &path,
                &backward,
                "src/services/discord/inflight.rs:test",
            );
        }));
        // In debug builds the debug_assert fires; in release it returns.
        // Either way the invariant record was emitted before the assert.
        assert!(result.is_err() || cfg!(not(debug_assertions)));
    }

    #[test]
    fn validate_save_allows_backward_last_offset_for_fresh_turn() {
        // #3017 I6: a DIFFERENT turn identity lowering last_offset is exempt —
        // the save path must not flag a legit fresh-turn reset.
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.last_offset = 300;
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        let mut fresh = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh.user_msg_id = 101;
        fresh.last_offset = 10;

        // No panic — different identity is exempt from the monotonic clamp.
        // #3089 B3: a permitted save returns `true` (write proceeds). This pins
        // the happy-path verdict so a mutation flipping the default to `false`
        // (which would silently drop legit fresh-turn writes) is caught.
        let permitted = validate_inflight_state_for_save(
            temp.path(),
            &path,
            &fresh,
            "src/services/discord/inflight.rs:test",
        );
        assert!(permitted);
    }

    #[test]
    fn validate_save_records_backward_response_sent_offset_violation_same_identity() {
        // #3154 OBSERVE-ONLY on the save path: a backward response_sent_offset
        // for the SAME turn identity records a `response_sent_offset_monotonic`
        // violation (and trips the debug_assert) but does NOT skip the write —
        // mirrors the last_offset_monotonic precedent.
        //
        // Force authority OFF so this test pins the original observe-only
        // branch regardless of a release-like process env. Authority ON is the
        // #3416 enforce branch: the same violation is recorded, then the write is
        // skipped and #3933 intentionally relaxes the debug tripwire.
        let _authority =
            crate::services::discord::outbound::delivery_record::authority_test_seam::force(false);
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.full_response = "hello world".to_string();
        existing.response_sent_offset = 8;
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // Same identity (user_msg_id + turn_start_offset) but a backward
        // response_sent_offset → records a violation. The debug_assert fires in
        // debug builds; catch the panic so we can assert observability fired.
        let mut backward = existing.clone();
        backward.response_sent_offset = 3;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_inflight_state_for_save(
                temp.path(),
                &path,
                &backward,
                "src/services/discord/inflight.rs:test",
            );
        }));
        // In debug builds the debug_assert fires; in release it returns.
        // Either way the invariant record was emitted before the assert.
        assert!(result.is_err() || cfg!(not(debug_assertions)));
    }

    #[test]
    fn validate_save_allows_response_sent_offset_reset_for_fresh_turn() {
        // #3154: a DIFFERENT turn identity resetting response_sent_offset to 0
        // (as InflightTurnState::new does) is exempt — the save path must not
        // flag a legit new-turn reset as a backward regression.
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.full_response = "prior turn output".to_string();
        existing.response_sent_offset = 12; // prior turn had N > 0
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // A fresh turn: new user_msg_id AND a new turn_start_offset, with
        // response_sent_offset reset to 0 (the InflightTurnState::new default).
        let mut fresh = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh.user_msg_id = 101;
        fresh.turn_start_offset = Some(99);
        assert_eq!(fresh.response_sent_offset, 0);

        // No panic — different identity is exempt from the monotonic clamp.
        validate_inflight_state_for_save(
            temp.path(),
            &path,
            &fresh,
            "src/services/discord/inflight.rs:test",
        );
    }

    #[test]
    fn validate_save_allows_synthetic_overwrite_after_user_turn_3154() {
        // #3154 replay: a prior USER turn persisted response_sent_offset > 0,
        // then a wakeup/background (TUI-direct synthetic) turn resets inflight
        // via InflightTurnState::new (new identity, response_sent_offset 0).
        // This is a LEGITIMATE new-turn transition and must NOT be flagged as a
        // response_sent_offset_monotonic regression.
        let temp = TempDir::new().unwrap();

        // Prior USER turn with response_sent_offset > 0.
        let mut user_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 555);
        user_turn.user_msg_id = 555;
        user_turn.turn_start_offset = Some(0);
        user_turn.full_response = "user turn response body".to_string();
        user_turn.response_sent_offset = 15;
        save_inflight_state_in_root(temp.path(), &user_turn).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // Synthetic turn freshly constructed via InflightTurnState::new — a new
        // identity (different user_msg_id / turn_start_offset) and
        // response_sent_offset 0.
        let synthetic = InflightTurnState::new(
            ProviderKind::Claude,
            321,
            Some("adk".to_string()),
            42,
            0, // synthetic user_msg_id
            0,
            "synthetic wakeup".to_string(),
            None,
            Some("AgentDesk-claude-adk".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            900, // new last_offset → new turn_start_offset
        );
        assert_eq!(synthetic.response_sent_offset, 0);
        assert_ne!(synthetic.turn_start_offset, user_turn.turn_start_offset);

        // No panic — the fresh synthetic identity is exempt from the monotonic
        // clamp, so no response_sent_offset_monotonic violation is recorded.
        validate_inflight_state_for_save(
            temp.path(),
            &path,
            &synthetic,
            "src/services/discord/inflight.rs:test",
        );
    }

    #[test]
    fn authority_guard_distinguishes_legit_reset_from_stale_regression_3933() {
        // #3933 (supersedes the #3860-era "would suppress" doc-guard): under
        // authority-ON the enforce guard MUST tell apart the LEGITIMATE Gemini/Qwen
        // RetryBoundary reset (turn_bridge/retry_state.rs clears `full_response` and
        // rewinds rso→0 for the SAME identity to re-stream) from a genuine
        // stale-snapshot backward regression (a non-empty body moving the frontier
        // back). The release runs AGENTDESK_DELIVERY_RECORD_AUTHORITY=1, so the old
        // coarse guard dropped the re-streamed body live; the `is_legitimate_full_reset`
        // signal carves the reset out while keeping the real regression blocked.
        use crate::services::discord::outbound::delivery_record as dr;
        // authority ON + same-turn backward move + legit reset signature → PERMIT
        // (the retry reset persists and the re-streamed body survives).
        assert!(!dr::authority_blocks_backward_inflight_write(
            true, false, true, true
        ));
        assert!(!dr::authority_blocks_backward_inflight_write(
            true, true, false, true
        ));
        // authority ON + same-turn backward move + NON-reset (non-empty body) → BLOCK
        // (a genuine stale-snapshot regression stays suppressed).
        assert!(dr::authority_blocks_backward_inflight_write(
            true, false, true, false
        ));
        assert!(dr::authority_blocks_backward_inflight_write(
            true, true, false, false
        ));
        // authority OFF (compiled default) → never blocks, reset flag irrelevant.
        assert!(!dr::authority_blocks_backward_inflight_write(
            false, false, true, false
        ));
        // authority ON but fully monotonic → not blocked (forward writes pass).
        assert!(!dr::authority_blocks_backward_inflight_write(
            true, true, true, false
        ));
    }

    #[test]
    fn delivery_response_sent_offset_stays_on_utf8_boundary() {
        let response = "안녕";
        let first_char_middle = 1;

        assert_eq!(
            normalize_response_sent_offset(response, first_char_middle),
            0
        );
        assert_eq!(
            normalize_response_sent_offset(response, response.len() + 100),
            response.len()
        );
    }

    #[test]
    fn guarded_clear_and_save_race_preserves_fresh_state() {
        let temp = TempDir::new().unwrap();
        let root = std::sync::Arc::new(temp.path().to_path_buf());

        for iteration in 0..20 {
            let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Codex, 777, 100);
            old_turn.started_at = format!("2026-05-17 10:00:{iteration:02}");
            save_inflight_state_in_root(root.as_ref(), &old_turn).unwrap();
            let old_identity = InflightTurnIdentity::from_state(&old_turn);

            let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Codex, 777, 101);
            fresh_turn.started_at = format!("2026-05-17 10:01:{iteration:02}");
            fresh_turn.user_text = "fresh prompt".to_string();

            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let clear_root = root.clone();
            let clear_barrier = barrier.clone();
            let clear_handle = std::thread::spawn(move || {
                clear_barrier.wait();
                clear_inflight_state_if_matches_identity_in_root(
                    clear_root.as_ref(),
                    &ProviderKind::Codex,
                    777,
                    &old_identity,
                )
            });

            barrier.wait();
            save_inflight_state_in_root(root.as_ref(), &fresh_turn).unwrap();
            let _ = clear_handle.join().expect("clear thread should not panic");

            let loaded = load_inflight_states_from_root(root.as_ref(), &ProviderKind::Codex);
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].started_at, fresh_turn.started_at);
            assert_eq!(loaded[0].user_text, "fresh prompt");
        }
    }

    /// #2427 — planned-restart markers must survive the explicit-signal
    /// hook because their lifetime is owned by the next dcserver boot's
    /// recovery. We bypass `load_inflight_states_from_root` here (which
    /// has its own retention-eviction side-effect) and assert directly
    /// on the file system that the row is intact after the guarded
    /// clear refused to touch it.
    #[test]
    fn clear_inflight_state_if_matches_preserves_planned_restart() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Codex, 555, 333);
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);
        let user_msg_id = state.user_msg_id;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_in_root(
            temp.path(),
            &ProviderKind::Codex,
            555,
            user_msg_id,
        );
        assert_eq!(outcome, GuardedClearOutcome::PlannedRestartSkipped);

        let provider_dir = temp.path().join(ProviderKind::Codex.as_str());
        let path = provider_dir.join("555.json");
        assert!(
            path.exists(),
            "planned-restart marker file should survive guarded clear"
        );
    }

    /// #2427 Pitfall #5 — rebind_origin rows are owned by the
    /// `/api/inflight/rebind` API. The explicit signal must NOT touch
    /// them even when user_msg_id matches.
    #[test]
    fn clear_inflight_state_if_matches_preserves_rebind_origin() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Gemini, 901, 444);
        state.rebind_origin = true;
        let user_msg_id = state.user_msg_id;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_in_root(
            temp.path(),
            &ProviderKind::Gemini,
            901,
            user_msg_id,
        );
        assert_eq!(outcome, GuardedClearOutcome::RebindOriginSkipped);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Gemini).len(),
            1
        );
    }

    /// `expected_user_msg_id = 0` is the "no guard available" sentinel —
    /// refuse to clear so the helper never accidentally deletes a row
    /// it cannot authenticate against.
    #[test]
    fn clear_inflight_state_if_matches_refuses_zero_guard() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Qwen, 8, 12_345);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome =
            clear_inflight_state_if_matches_in_root(temp.path(), &ProviderKind::Qwen, 8, 0);
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Qwen).len(),
            1
        );
    }

    /// #3161 (codex P1): the zero-owned guarded clear removes a genuine
    /// zero-id-owned row (recovery / external-input turn whose on-disk
    /// `user_msg_id` is 0). Recovery cleanup must keep working.
    #[test]
    fn clear_inflight_state_if_matches_zero_owned_clears_zero_id_row() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 9, 0);
        state.user_msg_id = 0;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_zero_owned_in_root(
            temp.path(),
            &ProviderKind::Claude,
            9,
        );
        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    /// #3161 (codex P1): the zero-owned guarded clear must NOT delete a NEWER
    /// real (non-zero) owner's row. A zero-id turn finalizing after a non-zero
    /// owner wrote its row yields `UserMsgMismatch` and the row survives.
    #[test]
    fn clear_inflight_state_if_matches_zero_owned_preserves_nonzero_owner() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 9, 4242);
        state.user_msg_id = 4242;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_zero_owned_in_root(
            temp.path(),
            &ProviderKind::Claude,
            9,
        );
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].user_msg_id, 4242);
    }

    #[test]
    fn generation_guarded_sweeper_clear_preserves_row_that_progressed_after_snapshot() {
        let temp = TempDir::new().unwrap();
        let snapshot = build_inflight_for_guard_tests(ProviderKind::Claude, 10, 4242);
        save_inflight_state_in_root(temp.path(), &snapshot).unwrap();
        let observed = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude)
            .into_iter()
            .next()
            .unwrap();

        let mut progressed = observed.clone();
        progressed.current_msg_id = progressed.current_msg_id.saturating_add(1);
        progressed.updated_at = "2099-01-01 00:00:01".to_string();
        progressed.save_generation = observed.save_generation.saturating_add(1);
        force_write_state(temp.path(), &progressed);

        let outcome =
            super::clear_store::clear_inflight_state_if_matches_identity_generation_in_root(
                temp.path(),
                &ProviderKind::Claude,
                observed.channel_id,
                &InflightTurnIdentity::from_state(&observed),
                observed.effective_finalizer_turn_id(),
                &observed.updated_at,
                observed.save_generation,
            );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        let surviving = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(surviving.len(), 1);
        assert_eq!(surviving[0].save_generation, progressed.save_generation);
        assert_eq!(surviving[0].current_msg_id, progressed.current_msg_id);
    }

    /// No on-disk row → `Missing`. Idempotency safety net.
    #[test]
    fn clear_inflight_state_if_matches_missing_is_noop() {
        let temp = TempDir::new().unwrap();
        let outcome =
            clear_inflight_state_if_matches_in_root(temp.path(), &ProviderKind::Claude, 42, 999);
        assert_eq!(outcome, GuardedClearOutcome::Missing);
    }

    // ---------------------------------------------------------------------
    // #3041 P1-2 (codex P1-2 R3): identity-guarded epilogue re-save. On a
    // delivery-lease `Skip` the watcher (holder) owns the inflight lifecycle
    // and clears the row on its OWN success. The bridge's epilogue must NOT
    // resurrect a holder-cleared row; it must refresh a still-present matching
    // row so retry survives when the holder FAILED.
    // ---------------------------------------------------------------------

    /// Skip → holder SUCCEEDED and cleared inflight (no row on disk). The bridge
    /// epilogue's identity-guarded save must NOT resurrect it (`Missing`) — no
    /// stale leak.
    #[test]
    fn skip_save_does_not_resurrect_holder_cleared_inflight() {
        let temp = TempDir::new().unwrap();
        // The holder already removed the row on its success path → nothing on disk.
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        let expected = InflightTurnIdentity::from_state(&state);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &state,
            &expected,
            state.turn_start_offset,
        );

        assert_eq!(
            outcome,
            GuardedSaveOutcome::Missing,
            "holder-cleared inflight must NOT be resurrected by the bridge skip epilogue"
        );
        assert!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty(),
            "no row may be recreated for an already-delivered turn"
        );
    }

    /// Skip → holder FAILED (NotDelivered) and did NOT clear; the turn-start row
    /// is still on disk with matching identity. The bridge epilogue's
    /// identity-guarded save refreshes it (`Saved`) so retry can re-deliver —
    /// no black-hole.
    #[test]
    fn skip_save_preserves_inflight_when_holder_did_not_clear() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        // The bridge accumulated more of the answer during the turn; it preserves
        // this updated copy for retry. Identity (user_msg_id/started_at/tmux) is
        // unchanged, so the guarded save must land.
        state.full_response = "partially delivered answer, retry me".to_string();
        let expected = InflightTurnIdentity::from_state(&state);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &state,
            &expected,
            state.turn_start_offset,
        );

        assert_eq!(
            outcome,
            GuardedSaveOutcome::Saved,
            "a still-present matching row must be refreshed so retry survives a holder failure"
        );
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].full_response,
            "partially delivered answer, retry me"
        );
    }

    /// Skip → a NEWER turn (different `user_msg_id`) already wrote its inflight
    /// before the preserving bridge's epilogue ran. The guarded save must NOT
    /// clobber the fresh turn (`IdentityMismatch`).
    #[test]
    fn skip_save_does_not_clobber_newer_turn() {
        let temp = TempDir::new().unwrap();
        // Newer turn currently owns the row on disk. (NB: the guard-test helper's
        // 3rd arg feeds `current_msg_id`; set the real `user_msg_id` explicitly so
        // the two turns differ on the identity field the guard actually checks.)
        let mut newer = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 0);
        newer.user_msg_id = 999;
        save_inflight_state_in_root(temp.path(), &newer).unwrap();

        // The preserving bridge is still holding the PREVIOUS turn (user_msg_id
        // 777). Its identity no longer matches the on-disk newer turn.
        let mut preserved = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 0);
        preserved.user_msg_id = 777;
        let expected = InflightTurnIdentity::from_state(&preserved);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &preserved,
            &expected,
            preserved.turn_start_offset,
        );

        assert_eq!(
            outcome,
            GuardedSaveOutcome::IdentityMismatch,
            "a preserved older turn must NOT overwrite a newer turn's inflight"
        );
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].user_msg_id, 999,
            "the newer turn's inflight must remain intact"
        );
    }

    /// Skip → the on-disk row's `turn_start_offset` no longer matches (a newer
    /// turn reusing the same `user_msg_id`/session at a different offset). The
    /// guarded save must refuse (`IdentityMismatch`).
    #[test]
    fn skip_save_checks_turn_start_offset() {
        let temp = TempDir::new().unwrap();
        let mut on_disk = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        on_disk.turn_start_offset = Some(500);
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        // Same identity (user_msg_id/started_at/tmux) as on_disk so ONLY the
        // turn_start_offset differs — isolating the offset guard.
        let mut preserved = on_disk.clone();
        preserved.turn_start_offset = Some(0);
        let expected = InflightTurnIdentity::from_state(&preserved);

        // The preserving bridge expects offset 0 but disk shows 500.
        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &preserved,
            &expected,
            Some(0),
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
    }

    #[test]
    fn existing_rebind_adoption_persists_paths_for_planned_restart() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut on_disk = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        on_disk.user_msg_id = 777;
        on_disk.current_msg_id = 778;
        on_disk.set_restart_mode(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        let expected = InflightTurnIdentity::from_state(&on_disk);
        let mut adopted = on_disk.clone();
        adopted.tmux_session_name = Some("AgentDesk-claude-adk-restored".to_string());
        adopted.output_path = Some("/tmp/restored-output.jsonl".to_string());
        adopted.input_fifo_path = Some("/tmp/restored-input.fifo".to_string());
        adopted.set_relay_owner_kind(RelayOwnerKind::Watcher);

        let outcome = save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
            temp.path(),
            &adopted,
            &expected,
            on_disk.turn_start_offset,
        );

        assert_eq!(outcome, GuardedSaveOutcome::Saved);
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].tmux_session_name.as_deref(),
            Some("AgentDesk-claude-adk-restored")
        );
        assert_eq!(
            rows[0].output_path.as_deref(),
            Some("/tmp/restored-output.jsonl")
        );
        assert_eq!(
            rows[0].input_fifo_path.as_deref(),
            Some("/tmp/restored-input.fifo")
        );
        assert_eq!(
            rows[0].effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        assert_eq!(
            rows[0].restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
    }

    #[test]
    fn existing_rebind_adoption_merges_into_fresh_on_disk_row() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut on_disk = build_inflight_for_guard_tests(ProviderKind::Claude, 323, 777);
        on_disk.user_msg_id = 777;
        on_disk.current_msg_id = 778;
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        let expected = InflightTurnIdentity::from_state(&on_disk);
        let mut adopted = on_disk.clone();
        adopted.tmux_session_name = Some("AgentDesk-claude-adk-restored".to_string());
        adopted.output_path = Some("/tmp/restored-output.jsonl".to_string());
        adopted.input_fifo_path = Some("/tmp/restored-input.fifo".to_string());
        adopted.set_relay_owner_kind(RelayOwnerKind::Watcher);

        let mut progressed = on_disk.clone();
        progressed.last_offset = 4096;
        progressed.last_watcher_relayed_offset = Some(2048);
        progressed.full_response = "newer streamed text".to_string();
        save_inflight_state_in_root(temp.path(), &progressed).unwrap();

        let outcome = save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
            temp.path(),
            &adopted,
            &expected,
            on_disk.turn_start_offset,
        );

        assert_eq!(outcome, GuardedSaveOutcome::Saved);
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].tmux_session_name.as_deref(),
            Some("AgentDesk-claude-adk-restored")
        );
        assert_eq!(
            rows[0].output_path.as_deref(),
            Some("/tmp/restored-output.jsonl")
        );
        assert_eq!(
            rows[0].input_fifo_path.as_deref(),
            Some("/tmp/restored-input.fifo")
        );
        assert_eq!(
            rows[0].effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        assert_eq!(rows[0].last_offset, 4096);
        assert_eq!(rows[0].last_watcher_relayed_offset, Some(2048));
        assert_eq!(rows[0].full_response, "newer streamed text");
    }

    #[test]
    fn existing_rebind_adoption_with_offset_rebase_persists_normalized_cursor_base() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut on_disk = build_inflight_for_guard_tests(ProviderKind::Codex, 324, 777);
        on_disk.user_msg_id = 777;
        on_disk.current_msg_id = 778;
        on_disk.set_restart_mode(InflightRestartMode::DrainRestart);
        on_disk.output_path = Some("/tmp/raw-rollout.jsonl".to_string());
        on_disk.last_offset = 4096;
        on_disk.turn_start_offset = Some(1024);
        on_disk.last_watcher_relayed_offset = Some(2048);
        on_disk.last_watcher_relayed_generation_mtime_ns = Some(9);
        on_disk.full_response = "already relayed text".to_string();
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        let expected = InflightTurnIdentity::from_state(&on_disk);
        let mut adopted = on_disk.clone();
        adopted.tmux_session_name = Some("AgentDesk-codex-adk-restored".to_string());
        adopted.output_path = Some("/tmp/normalized-rebind.jsonl".to_string());
        adopted.input_fifo_path = None;
        adopted.last_offset = 0;
        adopted.turn_start_offset = Some(0);
        adopted.last_watcher_relayed_offset = None;
        adopted.last_watcher_relayed_generation_mtime_ns = None;
        adopted.set_relay_owner_kind(RelayOwnerKind::Watcher);

        let outcome =
            save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
                temp.path(),
                &adopted,
                &expected,
                on_disk.turn_start_offset,
                on_disk.last_offset,
            );

        assert_eq!(outcome, GuardedSaveOutcome::Saved);
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].tmux_session_name.as_deref(),
            Some("AgentDesk-codex-adk-restored")
        );
        assert_eq!(
            rows[0].output_path.as_deref(),
            Some("/tmp/normalized-rebind.jsonl")
        );
        assert_eq!(rows[0].last_offset, 0);
        assert_eq!(rows[0].turn_start_offset, Some(0));
        assert_eq!(rows[0].last_watcher_relayed_offset, None);
        assert_eq!(rows[0].last_watcher_relayed_generation_mtime_ns, None);
        assert_eq!(rows[0].full_response, "already relayed text");
        assert_eq!(
            rows[0].effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
    }

    #[test]
    fn existing_rebind_adoption_with_offset_rebase_rejects_progressed_raw_cursor() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut on_disk = build_inflight_for_guard_tests(ProviderKind::Codex, 325, 777);
        on_disk.user_msg_id = 777;
        on_disk.current_msg_id = 778;
        on_disk.output_path = Some("/tmp/raw-rollout.jsonl".to_string());
        on_disk.last_offset = 4096;
        on_disk.turn_start_offset = Some(1024);
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        let expected = InflightTurnIdentity::from_state(&on_disk);
        let mut adopted = on_disk.clone();
        adopted.tmux_session_name = Some("AgentDesk-codex-adk-restored".to_string());
        adopted.output_path = Some("/tmp/normalized-rebind.jsonl".to_string());
        adopted.last_offset = 0;
        adopted.turn_start_offset = Some(0);
        adopted.set_relay_owner_kind(RelayOwnerKind::Watcher);

        let mut progressed = on_disk.clone();
        progressed.last_offset = 8192;
        progressed.last_watcher_relayed_offset = Some(6144);
        save_inflight_state_in_root(temp.path(), &progressed).unwrap();

        let outcome =
            save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
                temp.path(),
                &adopted,
                &expected,
                on_disk.turn_start_offset,
                on_disk.last_offset,
            );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].output_path.as_deref(),
            Some("/tmp/raw-rollout.jsonl")
        );
        assert_eq!(rows[0].last_offset, 8192);
        assert_eq!(rows[0].turn_start_offset, Some(1024));
        assert_eq!(rows[0].last_watcher_relayed_offset, Some(6144));
        assert_eq!(rows[0].effective_relay_owner_kind(), RelayOwnerKind::None);
    }

    #[test]
    fn clear_rebind_origin_identity_clears_matching_synthetic_row() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut state = build_inflight_for_guard_tests(ProviderKind::Codex, 326, 0);
        state.current_msg_id = 0;
        state.rebind_origin = true;
        state.turn_start_offset = Some(0);
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let expected = InflightTurnIdentity::from_state(&state);
        let outcome = clear_rebind_origin_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Codex,
            state.channel_id,
            &expected,
            state.turn_nonce.as_deref(),
        );

        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Codex).is_empty());
    }

    #[test]
    fn clear_rebind_origin_identity_preserves_non_rebind_turn() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut state = build_inflight_for_guard_tests(ProviderKind::Codex, 327, 0);
        state.current_msg_id = 0;
        state.turn_start_offset = Some(0);
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let expected = InflightTurnIdentity::from_state(&state);
        let outcome = clear_rebind_origin_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Codex,
            state.channel_id,
            &expected,
            state.turn_nonce.as_deref(),
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Codex).len(),
            1
        );
    }

    #[test]
    fn clear_rebind_origin_identity_preserves_mismatched_synthetic_row() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut state = build_inflight_for_guard_tests(ProviderKind::Codex, 328, 0);
        state.current_msg_id = 0;
        state.rebind_origin = true;
        state.turn_start_offset = Some(0);
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let mut expected = InflightTurnIdentity::from_state(&state);
        expected.turn_start_offset = Some(99);
        let outcome = clear_rebind_origin_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Codex,
            state.channel_id,
            &expected,
            state.turn_nonce.as_deref(),
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Codex).len(),
            1
        );
    }

    #[test]
    fn existing_rebind_adoption_does_not_clobber_rebind_origin() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let mut on_disk = build_inflight_for_guard_tests(ProviderKind::Claude, 322, 777);
        on_disk.user_msg_id = 777;
        on_disk.current_msg_id = 778;
        on_disk.rebind_origin = true;
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        let expected = InflightTurnIdentity::from_state(&on_disk);
        let mut adopted = on_disk.clone();
        adopted.tmux_session_name = Some("AgentDesk-claude-adk-restored".to_string());
        adopted.output_path = Some("/tmp/restored-output.jsonl".to_string());
        adopted.input_fifo_path = Some("/tmp/restored-input.fifo".to_string());
        adopted.set_relay_owner_kind(RelayOwnerKind::Watcher);

        let outcome = save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
            temp.path(),
            &adopted,
            &expected,
            on_disk.turn_start_offset,
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].rebind_origin);
        assert_eq!(rows[0].output_path.as_deref(), Some("/tmp/out.jsonl"));
        assert_eq!(rows[0].effective_relay_owner_kind(), RelayOwnerKind::None);
    }

    #[cfg(unix)]
    #[test]
    fn load_inflight_states_revalidates_malformed_row_under_lock() {
        let temp = TempDir::new().unwrap();
        let path = inflight_state_path(temp.path(), &ProviderKind::Codex, 18_001);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ definitely not valid json").unwrap();
        let lock = lock_inflight_state_path(&path).unwrap();
        let root = temp.path().to_path_buf();

        let loader =
            std::thread::spawn(move || load_inflight_states_from_root(&root, &ProviderKind::Codex));

        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut fresh = build_inflight_for_guard_tests(ProviderKind::Codex, 18_001, 88_001);
        fresh.user_msg_id = 88_001;
        std::fs::write(&path, serde_json::to_string_pretty(&fresh).unwrap()).unwrap();
        drop(lock);

        let states = loader.join().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].user_msg_id, 88_001);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Codex).len(),
            1
        );
    }

    /// Process-wide mutex so the two halves of the alive/dead override
    /// regression do not race against each other when cargo test runs them
    /// in parallel (the override is global state). `pub(super)` so the
    /// #3293 `recovery_relay_attempts_tests` module serializes on it too.
    pub(super) fn stale_override_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// 2026-05-26 adk-cdx incident regression: a DrainRestart inflight whose
    /// file mtime aged past 1800s but whose tmux pane is still alive must
    /// NOT be removed. Wiping it strands the live CLI's eventual response.
    #[test]
    fn stale_drain_restart_preserved_when_tmux_pane_alive() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&["AgentDesk-codex-adk-cdx-stale-alive-77"]));

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            77,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-77".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-alive-77".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);

        let result = super::stale_removal_reason(&state, 2000, 7);
        super::set_test_tmux_alive_override(None);
        assert!(
            result.is_none(),
            "alive tmux pane must override stale-age removal; got {:?}",
            result
        );
    }

    /// Mirror of the above: when the same aged DrainRestart row has NO live
    /// tmux pane, the existing stale-removal still fires.
    #[test]
    fn stale_drain_restart_removed_when_tmux_pane_dead() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&[])); // empty override = nothing alive

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            78,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-78".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-dead-78".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);

        let result = super::stale_removal_reason(&state, 2000, 7);
        super::set_test_tmux_alive_override(None);
        let reason = result.expect("dead-pane DrainRestart past 1800s must be removed");
        assert!(
            reason.contains("removing stale drain_restart"),
            "unexpected removal reason: {reason}"
        );
    }

    /// 2026-05-28 adk-cdx relay gap regression: normal, non-restart inflight
    /// rows must also be preserved while their tmux pane is alive. Otherwise a
    /// long-running Codex turn can finish after the 300s cleanup and have its
    /// terminal response suppressed because the inflight anchor vanished.
    #[test]
    fn stale_normal_inflight_preserved_when_tmux_pane_alive() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&["AgentDesk-codex-adk-cdx-stale-alive-79"]));

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            79,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-79".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-alive-79".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        let result = super::stale_removal_reason(&state, super::INFLIGHT_MAX_AGE_SECS + 1, 7);
        super::set_test_tmux_alive_override(None);
        assert!(
            result.is_none(),
            "alive tmux pane must preserve normal inflight rows; got {:?}",
            result
        );
    }

    #[test]
    fn stale_normal_inflight_removed_when_tmux_pane_dead() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&[]));

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            80,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-80".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-dead-80".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        let result = super::stale_removal_reason(&state, super::INFLIGHT_MAX_AGE_SECS + 1, 7);
        super::set_test_tmux_alive_override(None);
        let reason = result.expect("dead-pane normal inflight past 300s must be removed");
        assert!(
            reason.contains("removing stale inflight state file"),
            "unexpected removal reason: {reason}"
        );
    }
}

#[cfg(test)]
mod orphan_lock_reap_tests {
    //! #3641: orphan `.json.lock` sidecars are not seen by the `.json` row scans.
    use super::{ORPHAN_LOCK_REAP_MIN_AGE_SECS, reap_orphan_inflight_locks_in_root};
    use crate::services::provider::ProviderKind;
    use filetime::{FileTime, set_file_mtime};
    use std::path::Path;
    use tempfile::TempDir;

    fn write_file_with_age(path: &Path, now_unix: i64, age_secs: i64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"lock").unwrap();
        set_file_mtime(
            path,
            FileTime::from_unix_time(now_unix.saturating_sub(age_secs), 0),
        )
        .unwrap();
    }

    #[test]
    fn reaps_only_old_orphan_json_lock_files() {
        let temp = TempDir::new().unwrap();
        let now_unix = 1_800_000_000;
        let old_age = ORPHAN_LOCK_REAP_MIN_AGE_SECS + 10;
        let recent_age = ORPHAN_LOCK_REAP_MIN_AGE_SECS - 10;
        let provider_dir = temp.path().join(ProviderKind::Codex.as_str());

        let old_orphan_lock = provider_dir.join("101.json.lock");
        write_file_with_age(&old_orphan_lock, now_unix, old_age);

        let matched_json = provider_dir.join("202.json");
        let matched_lock = provider_dir.join("202.json.lock");
        std::fs::write(&matched_json, b"{}").unwrap();
        write_file_with_age(&matched_lock, now_unix, old_age);

        let recent_orphan_lock = provider_dir.join("303.json.lock");
        write_file_with_age(&recent_orphan_lock, now_unix, recent_age);

        let quarantine_marker = provider_dir.join("404.json.rebind-stall-123");
        write_file_with_age(&quarantine_marker, now_unix, old_age);

        let non_json_lock = provider_dir.join("505.lock");
        write_file_with_age(&non_json_lock, now_unix, old_age);

        let removed = reap_orphan_inflight_locks_in_root(temp.path(), now_unix);

        assert_eq!(removed, 1, "only the old orphan .json.lock is reaped");
        assert!(
            !old_orphan_lock.exists(),
            "old orphan lock with no matching .json must be removed"
        );
        assert!(
            matched_json.exists(),
            "matching .json state row must never be touched"
        );
        assert!(
            matched_lock.exists(),
            "lock with matching .json state row must be kept even when old"
        );
        assert!(
            recent_orphan_lock.exists(),
            "recent orphan lock must stay below the age floor"
        );
        assert!(
            quarantine_marker.exists(),
            "quarantine marker must not match the .json.lock sweep"
        );
        assert!(
            non_json_lock.exists(),
            "non-.json.lock files are out of scope"
        );
    }
}

#[cfg(test)]
mod wave_a_cleanup_tests {
    //! #2437 (#2427 C wire) — unit tests for the boot-time generation
    //! bulk invalidate. The B wire shares `clear_inflight_state_if_matches`
    //! with #2427's D / A wires and is already covered by the
    //! `clear_inflight_state_if_matches_*` tests in the parent mod.
    use super::{
        InflightTurnState, inflight_state_path, invalidate_stale_generation_in_root,
        load_inflight_states_from_root, lock_inflight_state_path, save_inflight_state_in_root,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    fn make_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            user_msg_id,
            user_msg_id + 1000,
            "hello".to_string(),
            None,
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    #[test]
    fn invalidate_stale_generation_evicts_non_planned_old_generations() {
        // C wire: a row whose `restart_generation` does not match the
        // boot-time `current_generation` AND that is not a planned
        // restart must be evicted before recovery runs.
        let temp = TempDir::new().unwrap();

        let mut row_old = make_state(501, 11);
        row_old.restart_generation = Some(3);
        save_inflight_state_in_root(temp.path(), &row_old).expect("save");

        let mut row_current = make_state(502, 22);
        row_current.restart_generation = Some(5);
        save_inflight_state_in_root(temp.path(), &row_current).expect("save");

        // Pre-condition: both rows on disk.
        let before = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(before.len(), 2);

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 5);
        assert_eq!(removed.len(), 1, "only the old-gen row should be removed");
        assert_eq!(removed[0], (501, Some(3)));

        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].channel_id, 502);
    }

    #[test]
    fn invalidate_stale_generation_preserves_planned_restart_rows() {
        // DrainRestart / HotSwapHandoff rows have their own
        // generation-mismatch handling in `stale_removal_reason` (auto-
        // evicts at load time with extended retention) — the C wire
        // must defer to that path and NOT double-evict.
        //
        // We stamp `restart_generation = Some(0)` to match the unit-
        // test environment's `load_generation()` reading (no generation
        // file → 0), so the load path itself does not auto-evict the
        // row. Then we ask `invalidate_stale_generation_in_root` to
        // run with a different "current_generation" — the helper must
        // still skip the row because `restart_mode.is_some()`, NOT
        // because the generations happen to match.
        // `load_generation()` reads the PROCESS-WIDE `AGENTDESK_ROOT_DIR`, so
        // serialize on the shared env lock and point the root at our own temp
        // dir for the whole test. Otherwise a concurrent root-mutating test can
        // flip the env between this read and the load-path read below, making
        // `current_runtime_gen` inconsistent and tripping the assertions.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );

        // With the root isolated to `temp` (no generation file → 0), the load
        // path's `stale_removal_reason` planned-restart branch hits its
        // generation-match arm and does not auto-evict.
        let current_runtime_gen = super::super::runtime_store::load_generation();

        let mut planned = make_state(601, 33);
        planned.set_restart_mode(InflightRestartMode::DrainRestart);
        planned.restart_generation = Some(current_runtime_gen);
        save_inflight_state_in_root(temp.path(), &planned).expect("save");

        // Pre-condition: row survives the load path.
        let before = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(
            before.len(),
            1,
            "load must not auto-evict same-gen planned restart"
        );

        // Now ask the C wire helper to use a "current_generation"
        // value that DEFINITELY mismatches the row's stamp. The helper
        // must still skip the row because `restart_mode.is_some()`.
        let mismatched_gen = current_runtime_gen.wrapping_add(9_999);
        let removed =
            invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, mismatched_gen);
        assert!(
            removed.is_empty(),
            "planned-restart rows must NOT be evicted by C wire bulk invalidate \
             even when their restart_generation mismatches the current generation"
        );

        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert!(after[0].restart_mode.is_some());
    }

    #[test]
    fn invalidate_stale_generation_preserves_rebind_origin_rows() {
        let temp = TempDir::new().unwrap();

        let mut rebind = make_state(701, 44);
        rebind.rebind_origin = true;
        rebind.restart_generation = Some(1);
        save_inflight_state_in_root(temp.path(), &rebind).expect("save");

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 9);
        assert!(removed.is_empty());
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert!(after[0].rebind_origin);
    }

    #[test]
    fn invalidate_stale_generation_preserves_current_generation_rows() {
        let temp = TempDir::new().unwrap();

        let mut fresh = make_state(801, 55);
        fresh.restart_generation = Some(7);
        save_inflight_state_in_root(temp.path(), &fresh).expect("save");

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 7);
        assert!(
            removed.is_empty(),
            "rows whose restart_generation matches current_generation must NOT be evicted"
        );

        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn invalidate_stale_generation_preserves_unstamped_rows() {
        // Codex review HIGH on PR #2460: normal `InflightTurnState::new`
        // sets `restart_generation = None`. Evicting unstamped rows here
        // would clear every healthy current-generation row at boot.
        // Unstamped rows are preserved; the intake-time staleness threshold
        // path is what bounds genuinely abandoned legacy rows.
        let temp = TempDir::new().unwrap();

        let unstamped = make_state(901, 66);
        assert!(unstamped.restart_generation.is_none());
        save_inflight_state_in_root(temp.path(), &unstamped).expect("save");

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 4);
        assert!(removed.is_empty());
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn invalidate_stale_generation_empty_dir_is_no_op() {
        let temp = TempDir::new().unwrap();
        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 1);
        assert!(removed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn invalidate_stale_generation_revalidates_row_under_lock() {
        let temp = TempDir::new().unwrap();

        let mut stale = make_state(951, 77);
        stale.restart_generation = Some(1);
        save_inflight_state_in_root(temp.path(), &stale).expect("save stale");

        let path = inflight_state_path(temp.path(), &ProviderKind::Codex, stale.channel_id);
        let lock = lock_inflight_state_path(&path).unwrap();
        let root = temp.path().to_path_buf();
        let invalidator = std::thread::spawn(move || {
            invalidate_stale_generation_in_root(&root, &ProviderKind::Codex, 2)
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut fresh = make_state(951, 78);
        fresh.restart_generation = Some(2);
        std::fs::write(&path, serde_json::to_string_pretty(&fresh).unwrap()).unwrap();
        drop(lock);

        let removed = invalidator.join().unwrap();
        assert!(
            removed.is_empty(),
            "fresh same-generation row written before the delete lock was acquired must survive"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].restart_generation, Some(2));
        assert_eq!(after[0].user_msg_id, 78);
    }
}

#[cfg(test)]
mod rebind_origin_reap_tests {
    //! #3581: bounded reap of abandoned `rebind_origin` orphans.
    //!
    //! The predicate `should_reap_abandoned_rebind_origin` must fire ONLY on the
    //! exact STALL-WATCHDOG orphan signature (rebind_origin + ExternalAdopted +
    //! owner None + user_msg_id 0 + current_msg_id 0 + never-progressed +
    //! never-delivered) AND only once past its deadline OR born in a prior
    //! generation. Every single live/adopted signal (owner, offset advance,
    //! user_msg_id, sent response, planned restart) must independently block the
    //! reap so a genuinely-live rebind is never destroyed (#3154 / #3540
    //! no-regression pins).
    use super::{
        DEAD_WATCHER_PROVEN_DEAD_SECS, InflightTurnState, REBIND_ORIGIN_DEADLINE_SECS_DEFAULT,
        RebindReapOutcome, RelayOwnerKind, TurnSource, WatcherLiveness,
        invalidate_stale_generation_in_root, load_inflight_states_from_root,
        proven_dead_from_signals, reap_abandoned_rebind_origin_locked_in_root,
        reap_dead_watcher_rebind_origin_locked_in_root, save_inflight_state_in_root,
        should_reap_abandoned_rebind_origin, should_reap_dead_watcher_rebind_origin,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::platform::tmux::PaneLiveness;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    /// A bare reapable rebind-origin row: born at offset `last_offset`,
    /// never adopted, never progressed, no owner, deadline default, stamped at
    /// `birth_generation`.
    fn reapable_rebind(
        channel_id: u64,
        last_offset: u64,
        birth_generation: u64,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            0, // request_owner
            0, // user_msg_id
            0, // current_msg_id
            "/api/inflight/rebind".to_string(),
            None,
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            last_offset,
        );
        state.rebind_origin = true;
        state.turn_source = TurnSource::ExternalAdopted;
        state.rebind_origin_created_at_unix = Some(super::now_unix());
        state.rebind_origin_deadline_secs = Some(REBIND_ORIGIN_DEADLINE_SECS_DEFAULT);
        state.rebind_origin_birth_generation = Some(birth_generation);
        // `new()` already sets last_offset == turn_start_offset; assert the
        // never-progressed invariant the predicate depends on.
        assert_eq!(state.last_offset, state.turn_start_offset.unwrap());
        state
    }

    const CURRENT_GEN: u64 = 9;
    const PAST_DEADLINE: u64 = REBIND_ORIGIN_DEADLINE_SECS_DEFAULT + 5;
    const WITHIN_DEADLINE: u64 = REBIND_ORIGIN_DEADLINE_SECS_DEFAULT - 5;

    #[test]
    fn reaps_abandoned_rebind_past_deadline() {
        // (a) Born on a non-empty output file (offset > 0), never adopted /
        // progressed, past deadline → reap. This is the exact #3581 wedge row.
        let state = reapable_rebind(1, 4096, CURRENT_GEN);
        assert!(should_reap_abandoned_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn preserves_fresh_rebind_within_deadline() {
        // (b) Same signature but within deadline and same generation → preserve.
        let state = reapable_rebind(2, 4096, CURRENT_GEN);
        assert!(!should_reap_abandoned_rebind_origin(
            &state,
            WITHIN_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn live_protect_offset_progress_never_reaped() {
        // (c-1) Watcher advanced past the birth offset → never reaped even past
        // the deadline. last_offset != turn_start_offset.
        let mut state = reapable_rebind(3, 4096, CURRENT_GEN);
        state.last_offset = state.turn_start_offset.unwrap() + 100;
        assert!(!should_reap_abandoned_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn live_protect_owner_watcher_never_reaped() {
        // (c-2) A live relay owner (MonitorTriggered watcher rebind shape) →
        // never reaped. Test both the typed field and the legacy bool.
        let mut typed = reapable_rebind(4, 4096, CURRENT_GEN);
        typed.set_relay_owner_kind(RelayOwnerKind::Watcher);
        assert!(!should_reap_abandoned_rebind_origin(
            &typed,
            PAST_DEADLINE,
            CURRENT_GEN
        ));

        let mut legacy = reapable_rebind(5, 4096, CURRENT_GEN);
        legacy.relay_owner_kind = RelayOwnerKind::None;
        legacy.watcher_owns_live_relay = true; // legacy bool only
        assert!(!should_reap_abandoned_rebind_origin(
            &legacy,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn live_protect_adopted_user_msg_never_reaped() {
        // (c-3) Adopted (user_msg_id != 0) → never reaped.
        let mut state = reapable_rebind(6, 4096, CURRENT_GEN);
        state.user_msg_id = 42;
        assert!(!should_reap_abandoned_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn live_protect_delivered_response_never_reaped() {
        // (c-4) Any delivered text (response_sent_offset > 0 or non-empty
        // full_response) → never reaped.
        let mut sent = reapable_rebind(7, 4096, CURRENT_GEN);
        sent.response_sent_offset = 10;
        assert!(!should_reap_abandoned_rebind_origin(
            &sent,
            PAST_DEADLINE,
            CURRENT_GEN
        ));

        let mut accumulated = reapable_rebind(8, 4096, CURRENT_GEN);
        accumulated.full_response = "partial answer".to_string();
        assert!(!should_reap_abandoned_rebind_origin(
            &accumulated,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn live_protect_anchor_or_terminal_never_reaped() {
        // Anchor placeholder present (current_msg_id != 0) or terminal commit →
        // never reaped.
        let mut anchored = reapable_rebind(9, 4096, CURRENT_GEN);
        anchored.current_msg_id = 777;
        assert!(!should_reap_abandoned_rebind_origin(
            &anchored,
            PAST_DEADLINE,
            CURRENT_GEN
        ));

        let mut committed = reapable_rebind(10, 4096, CURRENT_GEN);
        committed.terminal_delivery_committed = true;
        assert!(!should_reap_abandoned_rebind_origin(
            &committed,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn reaps_on_generation_mismatch_within_deadline() {
        // (d) Born in a prior generation → reap even within the deadline.
        let state = reapable_rebind(11, 4096, 1);
        assert!(should_reap_abandoned_rebind_origin(
            &state,
            WITHIN_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn legacy_row_reaps_via_mtime_age() {
        // (e) Legacy row: no created_at / birth_generation stamps. Reaps only
        // via the supplied (file-mtime) age; preserved while within deadline.
        let mut legacy = reapable_rebind(12, 4096, CURRENT_GEN);
        legacy.rebind_origin_created_at_unix = None;
        legacy.rebind_origin_deadline_secs = None; // falls back to env default
        legacy.rebind_origin_birth_generation = None;

        assert!(
            should_reap_abandoned_rebind_origin(&legacy, PAST_DEADLINE, CURRENT_GEN),
            "legacy row past mtime-age deadline must reap"
        );
        assert!(
            !should_reap_abandoned_rebind_origin(&legacy, WITHIN_DEADLINE, CURRENT_GEN),
            "legacy row within deadline must be preserved"
        );
    }

    #[test]
    fn planned_restart_rebind_never_reaped() {
        // (f) restart_mode set → planned restart owns retention; never reaped.
        let mut state = reapable_rebind(13, 4096, CURRENT_GEN);
        state.set_restart_mode(InflightRestartMode::DrainRestart);
        assert!(!should_reap_abandoned_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn non_rebind_row_never_reaped() {
        // A normal (non-rebind) row that happens to match every other conjunct
        // is out of scope entirely.
        let mut state = reapable_rebind(14, 4096, CURRENT_GEN);
        state.rebind_origin = false;
        assert!(!should_reap_abandoned_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
    }

    #[test]
    fn invalidate_stale_generation_reaps_prior_generation_rebind_orphan() {
        // Boot-time integration: a rebind orphan stamped from a prior
        // generation is reaped by `invalidate_stale_generation_in_root` even
        // though it has no `restart_generation` stamp (the old skip path would
        // have preserved it forever).
        let temp = TempDir::new().unwrap();
        let orphan = reapable_rebind(2001, 4096, 1); // birth gen 1
        save_inflight_state_in_root(temp.path(), &orphan).expect("save");

        let removed =
            invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, CURRENT_GEN);
        assert_eq!(
            removed.len(),
            1,
            "prior-generation rebind orphan must be reaped"
        );
        assert_eq!(removed[0], (2001, Some(1)));
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert!(after.is_empty());
    }

    #[test]
    fn invalidate_stale_generation_preserves_same_generation_fresh_rebind() {
        // A same-generation rebind orphan whose file mtime is fresh (age 0)
        // must survive the boot-time pass — neither the deadline nor the
        // generation disjunct fires.
        let temp = TempDir::new().unwrap();
        let fresh = reapable_rebind(2002, 4096, CURRENT_GEN);
        save_inflight_state_in_root(temp.path(), &fresh).expect("save");

        let removed =
            invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, CURRENT_GEN);
        assert!(
            removed.is_empty(),
            "fresh same-generation rebind row must survive the boot-time pass"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert!(after[0].rebind_origin);
    }

    #[test]
    fn invalidate_stale_generation_preserves_live_owned_rebind_prior_generation() {
        // Even with a prior-generation stamp, a rebind row that has a live
        // owner (Watcher) must NOT be reaped at boot — the live-protection
        // conjunction overrides the generation disjunct.
        let temp = TempDir::new().unwrap();
        let mut live = reapable_rebind(2003, 4096, 1); // prior gen
        live.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(temp.path(), &live).expect("save");

        let removed =
            invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, CURRENT_GEN);
        assert!(
            removed.is_empty(),
            "owner-Watcher rebind must survive boot reap even from a prior generation"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
    }

    // ----------------------------------------------------------------------
    // #3581 (codex TOCTOU fix): locked re-validate boundary for the periodic
    // placeholder-sweeper reap path. The sweeper passes an UNLOCKED snapshot to
    // `should_reap_abandoned_rebind_origin`; between that snapshot and the
    // delete, a normal intake / TUI claim can persist a brand-new live inflight
    // at the same sidecar path. `reap_abandoned_rebind_origin_locked_in_root`
    // must reload + re-validate identity + eligibility under the lock and skip
    // the unlink when the on-disk row is no longer the snapshotted orphan.
    // ----------------------------------------------------------------------

    #[test]
    fn locked_reap_unlinks_orphan_that_is_still_the_same_row() {
        // (b) The on-disk row is unchanged since the snapshot and is still a
        // prior-generation orphan → the locked re-validate succeeds and unlinks.
        let temp = TempDir::new().unwrap();
        let orphan = reapable_rebind(3001, 4096, 1); // prior gen → reap disjunct
        save_inflight_state_in_root(temp.path(), &orphan).expect("save");
        // Pre-check passes on the unlocked snapshot (mirrors the sweeper).
        assert!(should_reap_abandoned_rebind_origin(&orphan, 0, CURRENT_GEN));

        let outcome = reap_abandoned_rebind_origin_locked_in_root(
            temp.path(),
            &ProviderKind::Codex,
            &orphan,
            CURRENT_GEN,
        );
        assert_eq!(
            outcome,
            RebindReapOutcome::Reaped,
            "unchanged prior-generation orphan must be unlinked under the lock"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert!(after.is_empty(), "orphan file must be gone after reap");
    }

    #[test]
    fn locked_reap_skips_when_row_replaced_by_new_live_turn() {
        // (a) THE RACE: the sweeper snapshots an abandoned orphan, but before it
        // takes the lock a normal intake persists a brand-new LIVE turn at the
        // same channel path. The locked re-validate must DETECT the replacement
        // (live row no longer reapable) and skip the unlink so the new live turn
        // survives.
        let temp = TempDir::new().unwrap();
        let snapshot = reapable_rebind(3002, 4096, 1); // prior gen, reapable
        save_inflight_state_in_root(temp.path(), &snapshot).expect("save snapshot");
        assert!(should_reap_abandoned_rebind_origin(
            &snapshot,
            0,
            CURRENT_GEN
        ));

        // Simulate the racing intake: overwrite the same path with a live,
        // adopted, current-generation turn (NOT reapable). Same channel_id.
        let mut live = reapable_rebind(3002, 4096, CURRENT_GEN);
        live.rebind_origin = false; // a normal intake turn, not a rebind orphan
        live.turn_source = TurnSource::Managed;
        live.user_msg_id = 9999; // adopted → live-protected
        save_inflight_state_in_root(temp.path(), &live).expect("save live replacement");

        let outcome = reap_abandoned_rebind_origin_locked_in_root(
            temp.path(),
            &ProviderKind::Codex,
            &snapshot,
            CURRENT_GEN,
        );
        assert_eq!(
            outcome,
            RebindReapOutcome::Skipped,
            "a live replacement turn must NOT be reaped by a stale snapshot"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1, "the live replacement turn must survive");
        assert_eq!(after[0].user_msg_id, 9999, "survivor is the live turn");
        assert!(!after[0].rebind_origin);
    }

    #[test]
    fn locked_reap_skips_when_orphan_replaced_by_fresh_rebind_orphan() {
        // (a') Subtle variant: the replacement is ALSO a structurally-abandoned
        // rebind orphan, but a NEW birth (different created_at/generation). The
        // bare `should_reap_*` re-check could still fire on it, so the identity
        // guard is what blocks the wrong unlink — proving identity re-validation
        // (not just predicate re-run) is load-bearing.
        let temp = TempDir::new().unwrap();
        let snapshot = reapable_rebind(3003, 4096, 1); // birth gen 1
        save_inflight_state_in_root(temp.path(), &snapshot).expect("save snapshot");

        // Racing rebind respawn: same channel, fresh birth (gen 2, new
        // created_at, different turn_start_offset) but still prior to CURRENT_GEN
        // so the predicate alone would happily reap it.
        let mut respawn = reapable_rebind(3003, 8192, 2); // different offset + gen
        respawn.rebind_origin_created_at_unix =
            Some(snapshot.rebind_origin_created_at_unix.unwrap() + 100);
        save_inflight_state_in_root(temp.path(), &respawn).expect("save respawn");
        // Sanity: the predicate WOULD reap the respawn on its own (gen 2 != 9).
        assert!(should_reap_abandoned_rebind_origin(
            &respawn,
            0,
            CURRENT_GEN
        ));

        let outcome = reap_abandoned_rebind_origin_locked_in_root(
            temp.path(),
            &ProviderKind::Codex,
            &snapshot, // stale snapshot drives the reap
            CURRENT_GEN,
        );
        assert_eq!(
            outcome,
            RebindReapOutcome::Skipped,
            "a freshly-reborn orphan must not be reaped under the prior snapshot's identity"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1, "the respawned row must survive this pass");
        assert_eq!(after[0].rebind_origin_birth_generation, Some(2));
    }

    #[test]
    fn locked_reap_missing_when_file_already_gone() {
        // Idempotency: a snapshot whose file was already removed (e.g. a peer
        // sweep / claim cleared it) yields Missing, never a spurious Reaped.
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(ProviderKind::Codex.as_str())).unwrap();
        let snapshot = reapable_rebind(3004, 4096, 1);
        // Deliberately do NOT save the row.
        let outcome = reap_abandoned_rebind_origin_locked_in_root(
            temp.path(),
            &ProviderKind::Codex,
            &snapshot,
            CURRENT_GEN,
        );
        assert_eq!(outcome, RebindReapOutcome::Missing);
    }

    // ----------------------------------------------------------------------
    // #3635: dead-watcher rebind-origin reap. A Watcher-owned rebind orphan
    // (the #897 birth shape) is invisible to `should_reap_abandoned_rebind_origin`
    // (its `== None` owner conjunct can never hold), so it leaked forever even
    // after the watcher died. `should_reap_dead_watcher_rebind_origin` reaps it
    // ONLY when a runtime-liveness probe proves the watcher dead. A LIVE watcher
    // (tmux pane up or recent runtime activity) is never reaped — the injected
    // `WatcherLiveness` oracle lets us pin both directions without real tmux.
    // ----------------------------------------------------------------------

    /// Test [`WatcherLiveness`] oracle: returns a fixed proven-dead verdict so a
    /// unit test can pin the alive vs proven-dead branch without spawning tmux or
    /// touching jsonl/.generation files.
    struct StubLiveness {
        proven_dead: bool,
    }
    impl WatcherLiveness for StubLiveness {
        fn is_proven_dead(&self, _state: &InflightTurnState) -> bool {
            self.proven_dead
        }
    }
    const DEAD: StubLiveness = StubLiveness { proven_dead: true };
    const ALIVE: StubLiveness = StubLiveness { proven_dead: false };

    /// A Watcher-owned reapable rebind orphan: identical to `reapable_rebind`
    /// but with `relay_owner_kind = Watcher` (the #897 birth shape). This is the
    /// row the None-owner predicate can never touch.
    fn watcher_owned_rebind(
        channel_id: u64,
        last_offset: u64,
        birth_generation: u64,
    ) -> InflightTurnState {
        let mut state = reapable_rebind(channel_id, last_offset, birth_generation);
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        // Sanity: the legacy None-owner predicate refuses this row outright.
        assert!(!should_reap_abandoned_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN
        ));
        state
    }

    #[test]
    fn dead_watcher_rebind_reaps_when_proven_dead() {
        // (A) Watcher-owned + past deadline + proven dead → reap. This is the
        // exact #3635 leaked-row signature finally made reapable.
        let state = watcher_owned_rebind(7001, 4096, CURRENT_GEN);
        assert!(should_reap_dead_watcher_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));
    }

    #[test]
    fn idle_live_pane_rebind_reaped_at_deadline_3879() {
        // (a) #3879 REGRESSION GUARD — proven-dead/idle-stuck reaped at deadline.
        //
        // The live evidence: a watcher with a LIVE tmux pane (an idle TUI sitting
        // at `❯`) but NO recent runtime activity never adopted its empty
        // rebind-origin and was never reaped, leaving the placeholder LIVE for 64
        // min (32× the 120s deadline) and ABORTing every new TUI-direct turn.
        // The fixed `is_proven_dead` core now classifies `(Live, !activity)` as
        // proven dead, so the (correct) structural+deadline predicate finally
        // fires — but ONLY past the deadline (the re-adopt window stays open until
        // then).
        let proven_dead =
            proven_dead_from_signals(PaneLiveness::Live, /* activity_recent */ false);
        assert!(
            proven_dead,
            "#3879: an idle live-pane watcher (no recent activity) must be proven dead"
        );
        let oracle = StubLiveness { proven_dead };
        let state = watcher_owned_rebind(7201, 4096, CURRENT_GEN);
        assert!(
            should_reap_dead_watcher_rebind_origin(&state, PAST_DEADLINE, CURRENT_GEN, &oracle),
            "#3879: idle live-pane rebind-origin is reaped once past the deadline"
        );
        assert!(
            !should_reap_dead_watcher_rebind_origin(&state, WITHIN_DEADLINE, CURRENT_GEN, &oracle),
            "within the deadline the re-adopt window is still open — never reaped early"
        );
    }

    #[test]
    fn active_or_unknown_watcher_rebind_not_reaped_readopt_preserved_3879() {
        // (b) #3879 REGRESSION GUARD — re-adopt path preserved (#897/#3635).
        //
        // A watcher producing RECENT runtime activity (jsonl/.generation/rollout
        // writes) is genuinely working / re-adoptable and is NEVER proven dead,
        // so its rebind-origin survives even past the deadline — distinguishing
        // "temporarily quiet but re-adoptable" from "idle-stuck/dead".
        assert!(
            !proven_dead_from_signals(PaneLiveness::Live, /* activity_recent */ true),
            "live pane WITH recent activity = working/re-adoptable ⇒ preserved"
        );
        // A just-restarting watcher touches `.generation` before its pane
        // re-appears: DeadOrAbsent + recent activity must still preserve.
        assert!(
            !proven_dead_from_signals(PaneLiveness::DeadOrAbsent, /* activity_recent */ true),
            "restarting watcher (recent activity, pane not yet up) ⇒ preserved"
        );
        // An UNKNOWN probe (transient tmux hiccup) preserves regardless of activity.
        assert!(!proven_dead_from_signals(PaneLiveness::ProbeError, false));
        assert!(!proven_dead_from_signals(PaneLiveness::ProbeError, true));

        // End-to-end: the active-watcher verdict preserves the row past deadline.
        let oracle = StubLiveness {
            proven_dead: proven_dead_from_signals(
                PaneLiveness::Live,
                /* activity_recent */ true,
            ),
        };
        let state = watcher_owned_rebind(7202, 4096, CURRENT_GEN);
        assert!(
            !should_reap_dead_watcher_rebind_origin(&state, PAST_DEADLINE, CURRENT_GEN, &oracle),
            "active/re-adoptable watcher is NOT reaped — #897/#3635 re-adopt preserved"
        );
    }

    #[test]
    fn dead_watcher_rebind_reaps_on_prior_generation_when_dead() {
        // (A') Generation disjunct mirrors the None-owner predicate: a prior-
        // generation dead-watcher orphan reaps even within the deadline.
        let state = watcher_owned_rebind(7002, 4096, 1); // prior gen
        assert!(should_reap_dead_watcher_rebind_origin(
            &state,
            WITHIN_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));
    }

    #[test]
    fn live_watcher_rebind_never_reaped_when_alive() {
        // (B) THE LIVE-PROTECTION INVARIANT (#3154/#3540 homologue): a Watcher
        // that is still alive (tmux pane up OR recent runtime activity) is NEVER
        // reaped, even past the deadline and even from a prior generation.
        let past = watcher_owned_rebind(7003, 4096, CURRENT_GEN);
        assert!(!should_reap_dead_watcher_rebind_origin(
            &past,
            PAST_DEADLINE,
            CURRENT_GEN,
            &ALIVE
        ));
        let prior_gen = watcher_owned_rebind(7004, 4096, 1);
        assert!(!should_reap_dead_watcher_rebind_origin(
            &prior_gen,
            WITHIN_DEADLINE,
            CURRENT_GEN,
            &ALIVE
        ));
    }

    #[test]
    fn dead_watcher_rebind_never_reaped_within_deadline_same_generation() {
        // (B') Even proven-dead, a current-generation orphan within its deadline
        // is preserved — the deadline/generation disjunct gates first, before
        // liveness. Matches the None-owner predicate's timing exactly.
        let state = watcher_owned_rebind(7005, 4096, CURRENT_GEN);
        assert!(!should_reap_dead_watcher_rebind_origin(
            &state,
            WITHIN_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));
    }

    #[test]
    fn dead_watcher_reap_respects_structural_conjunction() {
        // (C) Every non-owner live signal still independently blocks the reap,
        // even with a proven-dead liveness verdict: adoption, anchor, terminal
        // commit, streamed bytes, offset progress, planned restart.
        let mut adopted = watcher_owned_rebind(7006, 4096, CURRENT_GEN);
        adopted.user_msg_id = 42;
        assert!(!should_reap_dead_watcher_rebind_origin(
            &adopted,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));

        let mut anchored = watcher_owned_rebind(7007, 4096, CURRENT_GEN);
        anchored.current_msg_id = 777;
        assert!(!should_reap_dead_watcher_rebind_origin(
            &anchored,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));

        let mut committed = watcher_owned_rebind(7008, 4096, CURRENT_GEN);
        committed.terminal_delivery_committed = true;
        assert!(!should_reap_dead_watcher_rebind_origin(
            &committed,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));

        let mut sent = watcher_owned_rebind(7009, 4096, CURRENT_GEN);
        sent.response_sent_offset = 10;
        assert!(!should_reap_dead_watcher_rebind_origin(
            &sent,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));

        let mut progressed = watcher_owned_rebind(7010, 4096, CURRENT_GEN);
        progressed.last_offset = progressed.turn_start_offset.unwrap() + 100;
        assert!(!should_reap_dead_watcher_rebind_origin(
            &progressed,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));

        let mut planned = watcher_owned_rebind(7011, 4096, CURRENT_GEN);
        planned.set_restart_mode(InflightRestartMode::DrainRestart);
        assert!(!should_reap_dead_watcher_rebind_origin(
            &planned,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));
    }

    #[test]
    fn none_owner_orphan_not_matched_by_dead_watcher_gate() {
        // The two predicates are disjoint on the owner conjunct: a None-owner
        // orphan (handled by the legacy predicate) is NOT in scope for the
        // dead-watcher gate even when proven dead, so no double-reap path.
        let none_owner = reapable_rebind(7012, 4096, CURRENT_GEN);
        assert_eq!(
            none_owner.effective_relay_owner_kind(),
            RelayOwnerKind::None
        );
        assert!(!should_reap_dead_watcher_rebind_origin(
            &none_owner,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));
    }

    #[test]
    fn non_rebind_row_never_matched_by_dead_watcher_gate() {
        let mut state = watcher_owned_rebind(7013, 4096, CURRENT_GEN);
        state.rebind_origin = false;
        assert!(!should_reap_dead_watcher_rebind_origin(
            &state,
            PAST_DEADLINE,
            CURRENT_GEN,
            &DEAD
        ));
    }

    #[test]
    fn locked_dead_watcher_reap_unlinks_when_unlocked_probe_already_proved_dead() {
        // End-to-end through the locked re-validate helper after the unlocked
        // liveness probe already proved death: an unchanged Watcher orphan is
        // unlinked by the cheap fs-only locked checks.
        let temp = TempDir::new().unwrap();
        let orphan = watcher_owned_rebind(7101, 4096, 1); // prior gen → reap disjunct
        save_inflight_state_in_root(temp.path(), &orphan).expect("save");
        assert!(should_reap_dead_watcher_rebind_origin(
            &orphan,
            0,
            CURRENT_GEN,
            &DEAD
        ));

        let outcome = reap_dead_watcher_rebind_origin_locked_in_root(
            temp.path(),
            &ProviderKind::Codex,
            &orphan,
            CURRENT_GEN,
        );
        assert_eq!(outcome, RebindReapOutcome::Reaped);
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert!(after.is_empty(), "dead-watcher orphan must be gone");
    }

    #[test]
    fn locked_dead_watcher_reap_skips_when_runtime_activity_resumes() {
        // TOCTOU: the snapshot was proven-dead by the unlocked tmux probe, but
        // between that probe and the lock the watcher resumed writing runtime
        // files. The locked helper must observe that cheap fs-only activity
        // signal and skip the unlink without spawning tmux under the lock.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );

        let snapshot = watcher_owned_rebind(7102, 4096, 1); // prior gen, snapshot looked dead
        save_inflight_state_in_root(temp.path(), &snapshot).expect("save");
        let session = snapshot
            .tmux_session_name
            .as_deref()
            .expect("watcher-owned test row has a tmux session");
        let generation_path =
            crate::services::tmux_common::session_temp_path(session, "generation");
        if let Some(parent) = std::path::Path::new(&generation_path).parent() {
            std::fs::create_dir_all(parent).expect("create runtime sessions dir");
        }
        std::fs::write(&generation_path, "resumed").expect("touch generation marker");

        let outcome = reap_dead_watcher_rebind_origin_locked_in_root(
            temp.path(),
            &ProviderKind::Codex,
            &snapshot,
            CURRENT_GEN,
        );
        assert_eq!(
            outcome,
            RebindReapOutcome::Skipped,
            "a watcher that resumed runtime writes mid-sweep must NOT be reaped"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1, "the resumed watcher's row must survive");
        assert!(after[0].rebind_origin);
    }

    #[test]
    fn boot_path_preserves_dead_watcher_rebind() {
        // The boot path (`invalidate_stale_generation_in_root`) is intentionally
        // NOT wired to the dead-watcher liveness gate: at cold start a just-
        // restarted live watcher's synthetic session reads as dead, so applying
        // the gate at boot would risk reaping a row a recovering watcher is about
        // to re-adopt. A Watcher-owned dead-shape orphan from a PRIOR generation
        // must therefore survive the boot pass entirely (only the warm 30s
        // sweeper, with a real runtime probe, may reap it). This is the keystone
        // `invalidate_stale_generation_preserves_live_owned_rebind_prior_generation`
        // invariant, re-pinned for the #3635 owner-Watcher dead shape.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );

        let dead_shape = watcher_owned_rebind(7103, 4096, 1); // prior gen
        save_inflight_state_in_root(temp.path(), &dead_shape).expect("save");

        let removed =
            invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, CURRENT_GEN);
        assert!(
            removed.is_empty(),
            "boot must NOT reap a Watcher-owned rebind (no liveness gate at boot)"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert!(after[0].rebind_origin);
    }

    #[test]
    fn proven_dead_window_is_conservative() {
        // The proven-dead floor must be far larger than the stall-watchdog's
        // 120s positive-liveness window so a live-but-between-turns watcher is
        // never false-classified as dead.
        assert!(
            DEAD_WATCHER_PROVEN_DEAD_SECS >= 600,
            "proven-dead floor must be conservative (>= 10 minutes)"
        );
    }
}

#[cfg(test)]
mod recovery_relay_attempts_tests {
    //! #3293: the `recovery_relay_attempts` restart-budget counter.
    //!
    //! The field is an additive `#[serde(default)]` column (NO
    //! `INFLIGHT_STATE_VERSION` bump, #2235 convention): legacy rows must
    //! deserialize with `0`, the value must round-trip, survive the
    //! boot-time `mark_all_inflight_states_restart_mode` re-marking pass
    //! (the infinite-WARN-loop carrier), and never weaken the pane-alive
    //! stale-removal guard.
    use super::{
        InflightTurnState, RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET, inflight_state_path,
        load_inflight_states_from_root, save_inflight_state_in_root,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    fn make_state(channel_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-3293".to_string()),
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    /// #3860/#3293: isolate the process-global runtime root for a test. Any test
    /// whose path reaches `InflightTurnState::new` (via `make_state`) or
    /// `set_restart_mode` must hold one of these: both call
    /// `runtime_store::load_generation`, which resolves `AGENTDESK_ROOT_DIR` and
    /// trips the live-release safety assert when it is unset (→ `~/.adk/release`)
    /// or already points at the live store. Holding the shared env lock also
    /// makes such tests order-independent (the prior failure mode: these tests
    /// passed only when a sibling env-touching test happened to have a tempdir
    /// root set at the same moment).
    fn isolated_root_env(temp: &TempDir) -> crate::config::TestEnvVarGuard {
        crate::config::set_agentdesk_root_for_test(temp.path())
    }

    #[test]
    fn budget_is_three_restarts() {
        assert_eq!(RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET, 3);
    }

    #[test]
    fn legacy_row_without_field_deserializes_to_zero() {
        // A pre-#3293 on-disk row has no `recovery_relay_attempts` key; the
        // additive field must default to 0 instead of failing the parse
        // (which would GC the row as malformed).
        let state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 8,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 8,
            "current_msg_id": 9,
            "current_msg_len": 0,
            "user_text": "hello",
            "source": "text",
            "session_id": null,
            "tmux_session_name": "AgentDesk-codex-adk-cdx",
            "output_path": "/tmp/out.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-05-17 10:00:00",
            "updated_at": "2026-05-17 10:00:00"
        }))
        .expect("legacy row without recovery_relay_attempts must deserialize");
        assert_eq!(state.recovery_relay_attempts, 0);
    }

    #[test]
    fn counter_round_trips_through_serde() {
        let mut state = make_state(3293);
        state.recovery_relay_attempts = 2;
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed: InflightTurnState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.recovery_relay_attempts, 2);
    }

    #[test]
    fn counter_survives_disk_round_trip_in_isolated_root() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_931);
        state.recovery_relay_attempts = 2;
        save_inflight_state_in_root(temp.path(), &state).expect("save");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].recovery_relay_attempts, 2);
    }

    #[test]
    fn finalizer_turn_id_uses_injected_prompt_and_round_trips() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_932);
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.injected_prompt_message_id = Some(555_777);
        save_inflight_state_in_root(temp.path(), &state).expect("save");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded[0].finalizer_turn_id, 555_777);

        let reloaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(reloaded[0].finalizer_turn_id, loaded[0].finalizer_turn_id);
    }

    #[test]
    fn missing_finalizer_turn_id_is_backfilled_and_restart_stable() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_933);
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.injected_prompt_message_id = None;
        let path = inflight_state_path(temp.path(), &ProviderKind::Codex, state.channel_id);
        save_inflight_state_in_root(temp.path(), &state).expect("save");
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw.as_object_mut().unwrap().remove("finalizer_turn_id");
        std::fs::write(&path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_ne!(loaded[0].finalizer_turn_id, 0);
        let backfilled = loaded[0].finalizer_turn_id;
        let reloaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(reloaded[0].finalizer_turn_id, backfilled);
    }

    #[test]
    fn guarded_clear_accepts_finalizer_turn_id_for_zero_user_msg_id() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_934);
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.finalizer_turn_id = 812_934;
        save_inflight_state_in_root(temp.path(), &state).expect("save");

        let outcome = super::clear_inflight_state_if_matches_in_root(
            temp.path(),
            &ProviderKind::Codex,
            state.channel_id,
            state.finalizer_turn_id,
        );
        assert_eq!(outcome, super::GuardedClearOutcome::Cleared);
    }

    /// The boot-time `mark_all_inflight_states_restart_mode` pass rewrites
    /// every row (the carrier of the pre-#3293 infinite WARN loop). The
    /// counter must survive that rewrite or the budget could never trip.
    #[test]
    fn counter_survives_restart_mode_remarking() {
        // `mark_all_inflight_states_restart_mode` resolves the root from the
        // process-global `AGENTDESK_ROOT_DIR`; serialize against every other
        // env-touching test and restore the previous value on exit.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = TempDir::new().unwrap();
        let _restore = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );

        // `mark_all_…` scans `$AGENTDESK_ROOT_DIR/runtime/discord_inflight`;
        // seed the row under that exact root.
        let inflight_root = super::inflight_runtime_root().expect("env root must resolve");
        let mut state = make_state(932_932);
        state.recovery_relay_attempts = 2;
        save_inflight_state_in_root(&inflight_root, &state).expect("save");

        let updated = super::mark_all_inflight_states_restart_mode(
            &ProviderKind::Codex,
            InflightRestartMode::DrainRestart,
        );
        assert_eq!(updated, 1, "the seeded row must be re-marked");

        let loaded = load_inflight_states_from_root(&inflight_root, &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        assert_eq!(
            loaded[0].recovery_relay_attempts, 2,
            "re-marking must not reset the restart-relay budget counter"
        );
    }

    /// #3860: model the shutdown bulk restart-mode mark racing a draining
    /// watcher. The marker conceptually observed the row at rso=10; the watcher
    /// then advanced the durable delivery frontier to rso=40 before the marker
    /// wrote. The RMW marker must re-read the FRESH frontier (40) under the
    /// flock and never rewind it to 10 — otherwise the replacement watcher
    /// re-relays `full_response[10..40]`, a duplicate Discord send.
    #[test]
    fn restart_marker_rmw_preserves_concurrently_advanced_frontier_3860() {
        let temp = TempDir::new().unwrap();
        // `set_restart_mode`/`make_state` resolve the global runtime root; isolate
        // it so the test is order-independent and never touches the live store.
        let _env = isolated_root_env(&temp);
        let provider = ProviderKind::Codex;

        // The marker's stale view (rso=10) — what `load_inflight_states_from_root`
        // returned at shutdown before the watcher advanced.
        let mut early = make_state(555);
        early.full_response = "Y".repeat(10);
        early.response_sent_offset = 10;
        early.last_offset = 10;
        save_inflight_state_in_root(temp.path(), &early).unwrap();
        let path = inflight_state_path(temp.path(), &provider, 555);

        // The draining watcher advances the durable frontier (forward, same
        // identity) to rso=40 before the marker writes.
        let mut advanced = early.clone();
        advanced.full_response = "Y".repeat(40);
        advanced.response_sent_offset = 40;
        advanced.last_offset = 40;
        save_inflight_state_in_root(temp.path(), &advanced).unwrap();

        // The marker writes LAST (the regression-prone ordering). RMW re-reads
        // the on-disk frontier (40) rather than carrying the stale rso=10.
        assert!(super::set_inflight_restart_mode_under_lock(
            &path,
            InflightRestartMode::DrainRestart
        ));

        let loaded = load_inflight_states_from_root(temp.path(), &provider);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].response_sent_offset, 40,
            "RMW marker must keep the watcher's advanced frontier, not the stale rso=10"
        );
        assert_eq!(loaded[0].last_offset, 40, "last_offset must not regress");
        assert_eq!(loaded[0].full_response.len(), 40);
        assert_eq!(
            loaded[0].restart_mode,
            Some(InflightRestartMode::DrainRestart),
            "the marker still records the restart mode"
        );
    }

    /// #3860 end-to-end: `mark_all_inflight_states_restart_mode` (the boot/
    /// shutdown bulk marker) must preserve a frontier a draining watcher
    /// advanced and still set restart_mode on every live row.
    #[test]
    fn mark_all_restart_mode_preserves_advanced_frontier_3860() {
        let temp = TempDir::new().unwrap();
        let _env = isolated_root_env(&temp);

        let inflight_root = super::inflight_runtime_root().expect("env root must resolve");
        let mut state = make_state(932_940);
        state.full_response = "Z".repeat(40);
        state.response_sent_offset = 40;
        state.last_offset = 40;
        save_inflight_state_in_root(&inflight_root, &state).expect("seed advanced frontier");

        let updated = super::mark_all_inflight_states_restart_mode(
            &ProviderKind::Codex,
            InflightRestartMode::DrainRestart,
        );
        assert_eq!(updated, 1, "the seeded row must be re-marked");

        let loaded = load_inflight_states_from_root(&inflight_root, &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].response_sent_offset, 40,
            "bulk restart-mode mark must not regress the delivery frontier"
        );
        assert_eq!(loaded[0].last_offset, 40);
        assert_eq!(
            loaded[0].restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
    }

    /// #3860 SAFETY: the bulk restart-mode marker must NOT undo a legitimate
    /// same-turn frontier reset. A Gemini/Qwen RetryBoundary clears
    /// `full_response` and rewinds `response_sent_offset` to 0 for the SAME
    /// identity (turn_bridge/retry_state.rs) to re-stream the turn. If the marker
    /// then resurrected an older frontier the re-streamed body would be
    /// suppressed (or double-relayed). The RMW marker re-reads the FRESH on-disk
    /// row, so the legitimate rso=0 reset is preserved verbatim — independent of
    /// the AGENTDESK_DELIVERY_RECORD_AUTHORITY guard (this path bypasses it).
    #[test]
    fn restart_marker_preserves_legitimate_frontier_reset_3860() {
        let temp = TempDir::new().unwrap();
        let _env = isolated_root_env(&temp);
        let provider = ProviderKind::Codex;

        // The turn_bridge persisted the retry reset: body cleared, frontier at 0.
        let mut reset = make_state(556);
        reset.full_response.clear();
        reset.response_sent_offset = 0;
        reset.last_offset = 0;
        save_inflight_state_in_root(temp.path(), &reset).unwrap();
        let path = inflight_state_path(temp.path(), &provider, 556);

        assert!(super::set_inflight_restart_mode_under_lock(
            &path,
            InflightRestartMode::DrainRestart
        ));

        let loaded = load_inflight_states_from_root(temp.path(), &provider);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].response_sent_offset, 0,
            "the marker must preserve the legitimate retry reset, not resurrect an old frontier"
        );
        assert!(
            loaded[0].full_response.is_empty(),
            "the cleared body must survive the restart-mode mark for the re-stream"
        );
        assert_eq!(
            loaded[0].restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
    }

    /// #3860 edge: a row removed between the unlocked enumeration and the RMW
    /// (e.g. a concurrent clear/finalize) must be skipped gracefully — the RMW
    /// re-read returns `None`, so the marker reports no write and never
    /// resurrects a stale row at the vacated path.
    #[test]
    fn restart_marker_skips_deleted_row_3860() {
        let temp = TempDir::new().unwrap();
        let _env = isolated_root_env(&temp);
        let provider = ProviderKind::Codex;

        let state = make_state(557);
        save_inflight_state_in_root(temp.path(), &state).unwrap();
        let path = inflight_state_path(temp.path(), &provider, 557);
        assert!(path.exists());

        // The row vanishes after enumeration, before the RMW write.
        std::fs::remove_file(&path).unwrap();

        assert!(
            !super::set_inflight_restart_mode_under_lock(&path, InflightRestartMode::DrainRestart),
            "a deleted row must report no write"
        );
        assert!(
            !path.exists(),
            "the marker must not resurrect a stale row for a path that was cleared"
        );
        assert!(
            load_inflight_states_from_root(temp.path(), &provider).is_empty(),
            "no inflight row should exist after the skip"
        );
    }

    /// #3860 edge: if the on-disk row is unparseable when the RMW re-reads it,
    /// the marker must skip it gracefully (no panic) and leave the bytes
    /// untouched rather than clobbering them with a regenerated state.
    #[test]
    fn restart_marker_skips_corrupt_row_3860() {
        let temp = TempDir::new().unwrap();
        let _env = isolated_root_env(&temp);
        let provider = ProviderKind::Codex;

        let state = make_state(558);
        save_inflight_state_in_root(temp.path(), &state).unwrap();
        let path = inflight_state_path(temp.path(), &provider, 558);

        // Corrupt the row to an unparseable blob.
        let corrupt: &[u8] = b"{ this is not valid inflight json";
        std::fs::write(&path, corrupt).unwrap();

        assert!(
            !super::set_inflight_restart_mode_under_lock(&path, InflightRestartMode::DrainRestart),
            "an unparseable row must report no write (graceful skip, no panic)"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            corrupt,
            "the marker must not overwrite an unparseable row with a regenerated state"
        );
    }

    /// #3293 invariant: the counter must not interact with the pane-alive
    /// stale-removal guard — a row carrying a saturated counter whose tmux
    /// pane is still alive is preserved by `stale_removal_reason`.
    #[test]
    fn saturated_counter_does_not_weaken_pane_alive_stale_guard() {
        let _guard = super::stall_recovery_tests::stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&["AgentDesk-codex-adk-cdx-932933"]));

        let mut state = make_state(932_933);
        state.tmux_session_name = Some("AgentDesk-codex-adk-cdx-932933".to_string());
        state.recovery_relay_attempts = 99;

        let result = super::stale_removal_reason(&state, super::INFLIGHT_MAX_AGE_SECS + 1, 7);
        super::set_test_tmux_alive_override(None);
        assert!(
            result.is_none(),
            "alive tmux pane must preserve the row regardless of the relay counter; got {result:?}"
        );
    }
}
