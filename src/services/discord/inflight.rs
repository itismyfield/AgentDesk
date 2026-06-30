//! Inflight turn state persistence.
//!
//! `response_sent_offset`, `current_msg_id`, and `last_watcher_relayed_offset`
//! participate in the relay state contract documented in
//! `docs/relay-state-contract.md` (#1222 / #1224). Any change that touches
//! relay producers/consumers must keep the invariants there satisfied.

pub(in crate::services::discord) mod anchor_repost;
pub(in crate::services::discord) mod budget;
mod finalizer_identity;
mod model;

// #3479: the pure domain model moved to `model.rs`; re-export every public
// item at its original visibility so existing `inflight::*` / `super::*`
// references across the discord module resolve unchanged.
pub(in crate::services::discord) use model::{
    InflightTurnIdentity, InflightTurnState, RelayOwnerKind, TurnSource, optional_message_id,
};

mod store;

// #3479: the FS path layout + flock guard moved to `store.rs`. `inflight_state_path`
// / `lock_inflight_state_path` are re-exported at the module-tree visibility so
// root call sites and `super::*` references in sibling child modules (e.g.
// `budget`) resolve unchanged; `inflight_provider_dir` stays inflight-private
// (root callers only) and is brought in via a plain import. `InflightStateFileLock`
// is named nowhere outside `store` (it only flows as a return type), so it keeps
// its module-tree visibility there without a parent re-export.
use store::inflight_provider_dir;
pub(in crate::services::discord::inflight) use store::inflight_state_path;
pub(crate) use store::lock_inflight_state_path;

// #3715: move the rebind-origin dead-watcher/orphan-lock helpers into a capped
// sibling module so this hot state parent stays below the frozen production-LoC
// baseline without changing call-site names.
mod rebind_reap;
use self::rebind_reap::{
    RuntimeWatcherLiveness, WatcherLiveness, dead_watcher_rebind_structurally_reapable,
    inflight_json_path_for_lock, is_inflight_json_lock_path, metadata_mtime_unix_secs,
    rebind_origin_age_secs, watcher_runtime_activity_recent,
};
#[cfg(test)]
use self::rebind_reap::{proven_dead_from_signals, should_reap_dead_watcher_rebind_origin};

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

use finalizer_identity::{
    backfill_finalizer_turn_id_under_lock, parse_inflight_state_content,
    parse_inflight_state_content_with_finalizer_backfill, read_inflight_state_content,
};
use std::collections::HashMap;
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

/// #1446 stall-deadlock recovery: an inflight state is treated as "stale"
/// (i.e. the dispatch that wrote it almost certainly already terminated
/// without cleanup) when its persisted `updated_at` has not advanced for
/// this many seconds. THREAD-GUARD uses this exact threshold; the
/// stall-watchdog uses `2x` to stay strictly more conservative than any
/// caller that has already observed the state directly.
///
/// `updated_at` is rewritten on every `save_inflight_state` call but is
/// **not** a true heartbeat — a healthy foreground model/tool call can
/// legitimately go silent for multiple minutes (long Bash, slow LLM
/// stream, large Read).
///
/// History: aligned with `placeholder_sweeper::ABANDON_THRESHOLD_SECS` (then
/// 300s) until #2427 (#2436 / #2437 / #2438) added explicit-signal wires (pane
/// death, heartbeat-gap sweeper, generation-mismatch invalidate, TurnCompleted
/// guard), relaxing the sweeper to a pure safety net (abandon timer 1800s). 300s
/// is retained here because it gates **new**-dispatch THREAD-GUARD + the
/// stall-watchdog (#1446): both want fast recovery once an explicit signal failed
/// to fire (a false-positive cleanup of a live turn is far worse than delay).
pub(super) const INFLIGHT_STALENESS_THRESHOLD_SECS: u64 = 300;

/// #3581: default deadline (seconds) after which an unadopted, never-progressed
/// `rebind_origin` row is reaped. Far shorter than the placeholder sweeper's
/// 1800s safety net so the wedge-causing orphan drains within a handful of
/// sweep ticks, but comfortably longer than the ~8s TUI-direct adoption
/// backstop window so a legitimate `/api/inflight/rebind` → TUI-adopt handoff
/// is never raced. Overridable via `AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS`.
pub(super) const REBIND_ORIGIN_DEADLINE_SECS_DEFAULT: u64 = 120;

/// #3641: orphan sidecar lock files are cosmetic after process death because
/// `flock` releases with the fd, but stale paths can accumulate forever once the
/// matching `.json` state row has been cleaned/quarantined. Keep the reap
/// conservative so a fresh lock created just before its `.json` row is written is
/// never touched.
pub(super) const ORPHAN_LOCK_REAP_MIN_AGE_SECS: i64 = 3600;

/// #3581: floor for the env-overridden rebind-origin deadline. Guards against a
/// pathologically small (or zero) override reaping rows before the adoption
/// backstop window can complete.
const REBIND_ORIGIN_DEADLINE_SECS_MIN: u64 = 30;

/// #3635: minimum quiescence (seconds) of *all* runtime liveness signals before
/// a Watcher-owned rebind-origin row is treated as "proven dead" and made
/// eligible for the dead-watcher reap path. Deliberately far larger than the
/// stall-watchdog's positive-liveness window
/// (`STALL_WATCHDOG_POSITIVE_LIVENESS_SECS` = 120s): a live Watcher that is
/// merely between turns (mailbox idle, no fresh jsonl byte) must NEVER be
/// false-classified as dead and reaped, because #3154/#3540 require a live
/// Watcher rebind to survive so it can re-adopt the session across a restart.
/// A 10-minute conservative floor means a false-negative (a genuinely dead
/// watcher whose reap is merely delayed) is the only failure mode — never a
/// false-positive (a live watcher reaped). The orphan is harmless while it
/// lingers (#3631/PR #3634 classifies it idle), so delay is acceptable.
pub(super) const DEAD_WATCHER_PROVEN_DEAD_SECS: u64 = 600;

/// #3581: resolve the rebind-origin reap deadline. Reads
/// `AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS`; on absence / parse failure falls
/// back to [`REBIND_ORIGIN_DEADLINE_SECS_DEFAULT`]. Any explicit value is
/// clamped up to [`REBIND_ORIGIN_DEADLINE_SECS_MIN`] so an operator cannot
/// accidentally configure a reap that races the adoption backstop.
pub(super) fn rebind_origin_deadline_secs_env() -> u64 {
    std::env::var("AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|secs| secs.max(REBIND_ORIGIN_DEADLINE_SECS_MIN))
        .unwrap_or(REBIND_ORIGIN_DEADLINE_SECS_DEFAULT)
}

/// #3581: current Unix epoch seconds (wall clock). Used to stamp a
/// rebind-origin row's birth time at creation.
pub(super) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// #3581: decide whether an abandoned `rebind_origin` inflight row is safe to
/// reap. The predicate is a strict conjunction of "never-progressed,
/// never-adopted, never-owned" signals so that a genuinely-live rebind
/// (MonitorTriggered watcher rebind: `relay_owner_kind = Watcher`) or a row
/// that has started relaying / been adopted is NEVER reaped:
///
///   * `rebind_origin` — only rebind-origin rows are in scope.
///   * `turn_source == ExternalAdopted` — pins the predicate to the
///     `recovery_engine` birth site; the `tmux` MonitorTriggered rebind
///     (`turn_source = MonitorTriggered`, owner Watcher) is excluded twice.
///   * `effective_relay_owner_kind() == None` — no live relay owner (also
///     absorbs the legacy `watcher_owns_live_relay` bool).
///   * `user_msg_id == 0 && current_msg_id == 0` — never adopted / no anchor.
///   * `!terminal_delivery_committed` — not finalised.
///   * `response_sent_offset == 0 && full_response.is_empty()` — nothing was
///     ever streamed to Discord.
///   * `last_offset == turn_start_offset` — the watcher never advanced past
///     the birth offset (NOTE: a fresh rebind row is born with
///     `last_offset == turn_start_offset == file_len`, which can be > 0 — the
///     "no progress" test is offset equality, NOT `last_offset == 0`).
///   * `restart_mode.is_none()` — planned restart / hot-swap rows own their
///     own retention and are never reaped here.
///
/// When every structural conjunct holds, the row is reaped iff it is past its
/// deadline OR it was born in a prior generation. `age_secs` is supplied by the
/// caller (file-mtime age in the sweeper path) so legacy rows with no
/// `rebind_origin_created_at_unix` stamp still age out via mtime.
pub(super) fn should_reap_abandoned_rebind_origin(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> bool {
    if !state.rebind_origin {
        return false;
    }
    let structurally_abandoned = state.turn_source == TurnSource::ExternalAdopted
        && state.effective_relay_owner_kind() == RelayOwnerKind::None
        && state.user_msg_id == 0
        && state.current_msg_id == 0
        && !state.terminal_delivery_committed
        && state.response_sent_offset == 0
        && state.full_response.is_empty()
        && state.last_offset == state.turn_start_offset.unwrap_or(state.last_offset)
        && state.restart_mode.is_none();
    if !structurally_abandoned {
        return false;
    }

    let deadline = state
        .rebind_origin_deadline_secs
        .unwrap_or_else(rebind_origin_deadline_secs_env);
    let past_deadline = age_secs >= deadline;
    let stale_generation = state
        .rebind_origin_birth_generation
        .is_some_and(|birth| birth != current_generation);
    past_deadline || stale_generation
}

/// #3581: operator-visibility event for a reaped abandoned rebind-origin row
/// (#3561 lifecycle stream). Mirrors the `evict_stale_generation` shape so the
/// two boot-time reap reasons aggregate side by side.
pub(super) fn emit_reap_abandoned_rebind_origin(
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
    reason: &str,
) {
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        state.channel_id,
        state.dispatch_id.as_deref(),
        None,
        None,
        "reap_abandoned_rebind_origin",
        serde_json::json!({
            "reason": reason,
            "age_secs": age_secs,
            "deadline_secs": state
                .rebind_origin_deadline_secs
                .unwrap_or_else(rebind_origin_deadline_secs_env),
            "birth_generation": state.rebind_origin_birth_generation,
            "current_generation": current_generation,
            "turn_source": state.turn_source.as_str(),
            "tmux_session_name": state.tmux_session_name,
        }),
    );
}

pub(super) fn inflight_runtime_root() -> Option<PathBuf> {
    discord_inflight_root()
}

/// #3641: boot-time cleanup for orphan `discord_inflight/{provider}/*.json.lock`
/// sidecars whose matching `.json` state file no longer exists. The lock file is
/// never authoritative by itself: live turns are protected by the existence of
/// the `.json` row, and fresh first-acquire locks are protected by the age floor.
pub(super) fn reap_orphan_inflight_locks_in_root(root: &Path, now_unix: i64) -> usize {
    let mut removed = 0usize;
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let provider_dir = inflight_provider_dir(root, &provider);
        let provider_name = provider.as_str();
        let Ok(lock_entries) = fs::read_dir(&provider_dir) else {
            continue;
        };

        for lock_entry in lock_entries {
            let Ok(lock_entry) = lock_entry else {
                continue;
            };
            let lock_path = lock_entry.path();
            if !is_inflight_json_lock_path(&lock_path) {
                continue;
            }
            let Some(json_path) = inflight_json_path_for_lock(&lock_path) else {
                continue;
            };
            if json_path.exists() {
                continue;
            }

            let Ok(metadata) = lock_entry.metadata() else {
                tracing::warn!(
                    provider = provider_name,
                    path = %lock_path.display(),
                    "#3641 failed to stat inflight orphan-lock candidate"
                );
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(mtime_unix) = metadata_mtime_unix_secs(&metadata) else {
                continue;
            };
            let age_secs = now_unix.saturating_sub(mtime_unix).max(0);
            if age_secs < ORPHAN_LOCK_REAP_MIN_AGE_SECS {
                continue;
            }
            if json_path.exists() {
                continue;
            }

            match fs::remove_file(&lock_path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        provider = provider_name,
                        path = %lock_path.display(),
                        error = %error,
                        "#3641 failed to remove orphan inflight lock file"
                    );
                }
            }
        }
    }

    if removed > 0 {
        tracing::info!(
            removed,
            root = %root.display(),
            "#3641 reaped orphan inflight lock file(s)"
        );
    }
    removed
}

pub(super) fn reap_orphan_inflight_locks() -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    reap_orphan_inflight_locks_in_root(&root, now_unix())
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
    fs::remove_file(path).is_ok()
}

/// #3581 (codex TOCTOU fix): outcome of a locked rebind-origin reap attempt so
/// callers (and tests) can distinguish "reaped" from "skipped because the row
/// was replaced/no-longer-eligible" from "the file was already gone".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RebindReapOutcome {
    /// The row was re-validated under the lock and unlinked.
    Reaped,
    /// The on-disk row no longer satisfies the reap predicate (e.g. a live
    /// intake/claim replaced it between the unlocked snapshot and the lock) —
    /// deletion was intentionally skipped.
    Skipped,
    /// The state file was already absent (idempotent no-op) or unreadable.
    Missing,
    /// The advisory lock could not be acquired — the caller should retry later.
    LockUnavailable,
}

/// #3581 (codex TOCTOU fix): true when the on-disk `locked` row is still the
/// *same* abandoned rebind-origin orphan identified by the unlocked `snapshot`.
///
/// The placeholder sweeper (and the boot invalidator) pass an unlocked snapshot
/// to `should_reap_abandoned_rebind_origin`; between that read and acquiring the
/// sidecar lock a normal intake / TUI claim can persist a brand-new live
/// inflight at the same `(provider, channel_id)` path. Re-running the reap
/// predicate alone is not sufficient: a *new* rebind-origin orphan could be born
/// (different birth stamp/generation) that also looks structurally abandoned.
/// We therefore additionally require the row's birth identity to be unchanged
/// (`rebind_origin_created_at_unix` + `rebind_origin_birth_generation`), so a
/// replacement turn is never mistaken for the snapshotted orphan.
fn rebind_row_identity_unchanged(snapshot: &InflightTurnState, locked: &InflightTurnState) -> bool {
    locked.rebind_origin == snapshot.rebind_origin
        && locked.rebind_origin_created_at_unix == snapshot.rebind_origin_created_at_unix
        && locked.rebind_origin_birth_generation == snapshot.rebind_origin_birth_generation
        && locked.turn_start_offset == snapshot.turn_start_offset
}

/// #3581 (codex TOCTOU fix): reap an abandoned rebind-origin orphan under the
/// sidecar lock with a re-validate-then-unlink contract. This is the shared
/// implementation behind both the periodic placeholder-sweeper path and the
/// boot-time `invalidate_stale_generation` path so the two stay consistent.
///
/// Contract:
///   1. Acquire the sidecar advisory lock for the row's path (the same
///      non-reentrant `flock` every intake/claim/persist helper takes).
///   2. **Reload** the current on-disk row under the lock.
///   3. Confirm the reloaded row is still the *same* orphan as `snapshot`
///      (`rebind_row_identity_unchanged`) AND still satisfies
///      `should_reap_abandoned_rebind_origin` with a freshly recomputed age
///      (created-at preferred, mtime fallback for legacy rows).
///   4. Unlink **only** when both checks hold; otherwise skip (a live intake /
///      claim replaced the orphan since the snapshot).
///
/// Returns the [`RebindReapOutcome`]; the caller emits observability on
/// [`RebindReapOutcome::Reaped`].
fn reap_abandoned_rebind_origin_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> RebindReapOutcome {
    let path = inflight_state_path(root, provider, snapshot.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return RebindReapOutcome::LockUnavailable;
    };
    let Some(locked) = read_inflight_state_content(&path) else {
        return RebindReapOutcome::Missing;
    };
    // The unlocked snapshot may have raced a replacement turn. Re-validate the
    // current row's birth identity AND its eligibility under a freshly computed
    // age before touching the file.
    if !rebind_row_identity_unchanged(snapshot, &locked) {
        return RebindReapOutcome::Skipped;
    }
    let age_secs = rebind_origin_age_secs(&path, &locked);
    if !should_reap_abandoned_rebind_origin(&locked, age_secs, current_generation) {
        return RebindReapOutcome::Skipped;
    }
    match fs::remove_file(&path) {
        Ok(()) => RebindReapOutcome::Reaped,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RebindReapOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = snapshot.channel_id,
                error = %error,
                "#3581 rebind reap remove_file failed under lock; treating as Missing"
            );
            RebindReapOutcome::Missing
        }
    }
}

/// #3581 (codex TOCTOU fix): env-rooted wrapper around
/// [`reap_abandoned_rebind_origin_locked_in_root`] for the periodic
/// placeholder-sweeper path. Returns `true` iff the row was re-validated under
/// the lock and unlinked (so the sweeper only counts/emits genuine reaps).
pub(super) fn reap_abandoned_rebind_origin_locked(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    reap_abandoned_rebind_origin_locked_in_root(&root, provider, snapshot, current_generation)
        == RebindReapOutcome::Reaped
}

/// #3635: locked re-validate-then-unlink for the *dead-watcher* rebind-origin
/// reap path. The expensive tmux liveness probe runs OUTSIDE this lock (see
/// [`sweep_reap_dead_watcher_rebind_origin`], off the async runtime via
/// `spawn_blocking`); under the lock we re-check only CHEAP, fs-only conditions —
/// no tmux subprocess — so the per-channel sidecar lock is never held across
/// blocking subprocess I/O (codex review ISSUE 2).
///
/// What the lock DOES close: the row-replacement race — a replacement turn that
/// rewrote this sidecar path is rejected by the birth-identity guard, and a row
/// that progressed (structural conjunction) is rejected. What it does NOT fully
/// close (codex review ISSUE 3): a watcher whose tmux *pane* re-appears between
/// the unlocked probe and this unlink. That window is tiny and the consequence
/// is benign — the reaped artifact is a zero-msg-id orphan that a genuinely
/// re-adopting watcher simply re-creates. As a last cheap guard we re-read the
/// runtime-activity mtime under the lock, so a watcher that resumed *writing*
/// (the most common resurrection signal) is still observed and the reap Skips.
fn reap_dead_watcher_rebind_origin_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> RebindReapOutcome {
    let path = inflight_state_path(root, provider, snapshot.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return RebindReapOutcome::LockUnavailable;
    };
    let Some(locked) = read_inflight_state_content(&path) else {
        return RebindReapOutcome::Missing;
    };
    // Birth-identity guard (same as the None-owner path): a replacement turn at
    // the same sidecar path must never be mistaken for the snapshotted orphan.
    if !rebind_row_identity_unchanged(snapshot, &locked) {
        return RebindReapOutcome::Skipped;
    }
    let age_secs = rebind_origin_age_secs(&path, &locked);
    // Re-check the cheap, fs-only half of the predicate under the lock (structure
    // + deadline/generation). The tmux liveness probe already ran unlocked; we do
    // NOT re-spawn it here (ISSUE 2 — no subprocess under the sidecar lock).
    if !dead_watcher_rebind_structurally_reapable(&locked, age_secs, current_generation) {
        return RebindReapOutcome::Skipped;
    }
    // Last cheap guard: a watcher that resumed writing its jsonl/`.generation`
    // since the unlocked probe reads as recent activity ⇒ alive ⇒ Skip. Pure fs
    // stat (no subprocess), safe under the lock.
    if let Some(session) = locked.tmux_session_name.as_deref() {
        let session = session.trim();
        if !session.is_empty() && watcher_runtime_activity_recent(session) {
            return RebindReapOutcome::Skipped;
        }
    }
    match fs::remove_file(&path) {
        Ok(()) => RebindReapOutcome::Reaped,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RebindReapOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = snapshot.channel_id,
                error = %error,
                "#3635 dead-watcher rebind reap remove_file failed under lock; treating as Missing"
            );
            RebindReapOutcome::Missing
        }
    }
}

/// #3635: env-rooted wrapper around
/// [`reap_dead_watcher_rebind_origin_locked_in_root`] for the periodic
/// placeholder-sweeper path. Returns `true` iff the row was re-validated under
/// the lock (cheap fs-only re-checks; no tmux subprocess) and unlinked.
pub(super) fn reap_dead_watcher_rebind_origin_locked(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    reap_dead_watcher_rebind_origin_locked_in_root(&root, provider, snapshot, current_generation)
        == RebindReapOutcome::Reaped
}

/// #3635: the placeholder sweeper's single-call entry point for the dead-watcher
/// rebind-origin reap. Returns `true` only when the row was genuinely unlinked.
/// Three stages, ordered cheapest-first:
///   1. fs-only structural gate ([`dead_watcher_rebind_structurally_reapable`]) —
///      most rows fail here with no tmux probe;
///   2. the production [`RuntimeWatcherLiveness`] proven-dead probe, run on a
///      `spawn_blocking` thread (it spawns tmux subprocesses) so the async
///      sweeper runtime is never blocked, and OUTSIDE any lock (codex review
///      ISSUE 2);
///   3. the locked re-validate ([`reap_dead_watcher_rebind_origin_locked`]),
///      which re-checks only cheap fs-only conditions under the sidecar lock.
///
/// Deliberately NOT called from the boot path
/// (`invalidate_stale_generation_in_root`): at cold start a just-restarted live
/// watcher's session reads as dead, so the liveness gate only ever fires in the
/// warm 30s sweeper where a real runtime probe is meaningful (#3154/#3540 / the
/// keystone boot-preservation invariant).
pub(super) async fn sweep_reap_dead_watcher_rebind_origin(
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> bool {
    // (1) Cheap, fs-only structural gate first — skip the subprocess entirely for
    // the common case.
    if !dead_watcher_rebind_structurally_reapable(state, age_secs, current_generation) {
        return false;
    }
    // (2) The proven-dead probe spawns tmux subprocesses; run it off the async
    // runtime via `spawn_blocking`, outside any lock. A join failure (panic /
    // runtime shutdown) is treated as "unknown ⇒ preserve".
    let probe_state = state.clone();
    let proven_dead =
        tokio::task::spawn_blocking(move || RuntimeWatcherLiveness.is_proven_dead(&probe_state))
            .await
            .unwrap_or(false);
    if !proven_dead {
        return false;
    }
    // (3) Locked re-validate (cheap fs-only re-checks; no subprocess under lock).
    reap_dead_watcher_rebind_origin_locked(provider, state, current_generation)
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Parse the persisted `started_at` (`now_string` localtime form) back into
/// a Unix timestamp. Returns `None` for unparseable values so callers can
/// fall back to a wall-clock derived approximation.
pub(super) fn parse_started_at_unix(started_at: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|local| local.timestamp())
}

/// Parse a persisted `updated_at` field (same `now_string` localtime form
/// as `started_at`) back into a Unix timestamp. Wrapper kept distinct from
/// `parse_started_at_unix` purely for call-site readability — both fields
/// share the same encoding but represent different lifecycle moments.
pub(super) fn parse_updated_at_unix(updated_at: &str) -> Option<i64> {
    parse_started_at_unix(updated_at)
}

/// #1446 stall-deadlock recovery: returns `true` when the persisted
/// `updated_at` of an inflight state is older than
/// `threshold_secs` seconds relative to `now_unix_secs`.
///
/// Returns `false` if `updated_at` is unparseable — staleness should never
/// be inferred from missing data. This keeps the helper safe to call from
/// the THREAD-GUARD and stall-watchdog paths even when a partially
/// migrated state file is on disk.
pub(super) fn inflight_state_is_stale(
    state: &InflightTurnState,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    let Some(updated_at_unix) = parse_updated_at_unix(&state.updated_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(updated_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

fn inflight_state_started_at_is_stale(
    state: &InflightTurnState,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    let Some(started_at_unix) = parse_started_at_unix(&state.started_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(started_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

/// A TUI-direct `ExternalInput` row can be born as a bridge-owned synthetic
/// claim before the bridge tail has created the real response placeholder. If
/// dcserver restarts in that narrow window, the tail is gone and the row has no
/// live relay owner left. Treat that shape as stale after the normal inflight
/// threshold so prompt scanners / idle relays / health recovery do not block
/// forever on a row that cannot make progress.
pub(super) fn ownerless_external_input_inflight_is_stale_at(
    state: &InflightTurnState,
    now_unix_secs: i64,
) -> bool {
    state.turn_source == TurnSource::ExternalInput
        && state.effective_relay_owner_kind() == RelayOwnerKind::None
        && state.injected_prompt_message_id.is_some()
        && state.current_msg_id == 0
        && state.response_sent_offset == 0
        && state.full_response.trim().is_empty()
        && state.last_watcher_relayed_offset.is_none()
        && !state.terminal_delivery_committed
        && (inflight_state_is_stale(state, now_unix_secs, INFLIGHT_STALENESS_THRESHOLD_SECS)
            || (state.restart_mode.is_some()
                && inflight_state_started_at_is_stale(
                    state,
                    now_unix_secs,
                    INFLIGHT_STALENESS_THRESHOLD_SECS,
                )))
}

pub(super) fn ownerless_external_input_inflight_is_stale(state: &InflightTurnState) -> bool {
    ownerless_external_input_inflight_is_stale_at(state, now_unix())
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

fn validate_inflight_state_for_save(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    code_location: &'static str,
) -> bool {
    let offset_in_bounds = state.response_sent_offset <= state.full_response.len()
        && state
            .full_response
            .is_char_boundary(state.response_sent_offset);
    record_inflight_invariant(
        offset_in_bounds,
        state,
        "response_sent_offset_in_bounds",
        code_location,
        "inflight response_sent_offset must stay within full_response",
        serde_json::json!({
            "response_sent_offset": state.response_sent_offset,
            "full_response_len": state.full_response.len(),
            "path": path.display().to_string(),
        }),
    );
    debug_assert!(
        offset_in_bounds,
        "inflight response_sent_offset must stay within full_response"
    );

    let Ok(existing_content) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(existing) = serde_json::from_str::<InflightTurnState>(&existing_content) else {
        return true;
    };

    // #3154 — OBSERVE-ONLY on the bridge/watcher save path. A legit fresh-turn
    // reset (different user_msg_id or turn_start_offset) resets
    // response_sent_offset to 0 on purpose (see InflightTurnState::new), so the
    // check is gated by SAME turn identity; only a backward move within the same
    // turn is a violation. We do not skip the write here (that would drop a
    // legit fresh turn); this mirrors the last_offset_monotonic precedent below.
    let same_turn_identity = existing.user_msg_id == state.user_msg_id
        && existing.turn_start_offset == state.turn_start_offset;
    let monotonic_offset =
        !same_turn_identity || state.response_sent_offset >= existing.response_sent_offset;
    // I6 (last_offset_monotonic) — OBSERVE-ONLY on the bridge/watcher save
    // path. A legit fresh-turn reset (different user_msg_id or
    // turn_start_offset) lowers last_offset on purpose, so the check is gated
    // by SAME turn identity; only a backward move within the same turn is a
    // violation. We do not skip the write here (that would drop a legit fresh
    // turn); the enforcing variant lives in the standby/refresh path.
    let last_offset_monotonic = !same_turn_identity || state.last_offset >= existing.last_offset;

    // #3552: when the #3416 enforce guard (below) will SKIP this backward write
    // and preserve the offset (zero data loss), the offset-monotonic violation
    // has already been safely handled — record it at WARN instead of ERROR so
    // the paired `#3416 enforce` WARN is the only operator-facing log, killing
    // the duplicate ERROR-log noise. When enforce is OFF the backward write
    // actually persists below, so the violation stays ERROR (a genuine breach).
    // Computed BEFORE the records so the severity is correct; the enforce branch
    // itself (skip + return false) is unchanged.
    use crate::services::discord::outbound::delivery_record as dr;
    let authority = dr::delivery_record_authority_enabled();
    let enforce_skips_backward_write = dr::authority_blocks_backward_inflight_write(
        authority,
        monotonic_offset,
        last_offset_monotonic,
    );
    let offset_monotonic_severity =
        offset_monotonic_invariant_severity(enforce_skips_backward_write);

    record_inflight_invariant_with_severity(
        monotonic_offset,
        state,
        "response_sent_offset_monotonic",
        code_location,
        "inflight response_sent_offset must not move backwards for the same turn identity",
        serde_json::json!({
            "previous": existing.response_sent_offset,
            "next": state.response_sent_offset,
            "same_turn_identity": same_turn_identity,
            "path": path.display().to_string(),
        }),
        offset_monotonic_severity,
    );
    debug_assert!(
        monotonic_offset,
        "inflight response_sent_offset must not move backwards for the same turn identity"
    );

    record_inflight_invariant_with_severity(
        last_offset_monotonic,
        state,
        "last_offset_monotonic",
        code_location,
        "inflight last_offset must not move backwards for the same turn identity",
        serde_json::json!({
            "previous": existing.last_offset,
            "next": state.last_offset,
            "same_turn_identity": same_turn_identity,
            "path": path.display().to_string(),
        }),
        offset_monotonic_severity,
    );
    debug_assert!(
        last_offset_monotonic,
        "inflight last_offset must not move backwards for the same turn identity"
    );

    let same_tmux_owner = existing.tmux_session_name.is_none()
        || state.tmux_session_name.is_none()
        || existing.tmux_session_name == state.tmux_session_name;
    record_inflight_invariant(
        same_tmux_owner,
        state,
        "inflight_tmux_one_to_one",
        code_location,
        "inflight state for a channel must not drift between tmux sessions",
        serde_json::json!({
            "previous_tmux_session_name": existing.tmux_session_name.as_deref(),
            "next_tmux_session_name": state.tmux_session_name.as_deref(),
            "root": root.display().to_string(),
            "path": path.display().to_string(),
        }),
    );

    // #3416 (#3089 B3): observe→ENFORCE under the durable-authority flag (no-op
    // when OFF); see dr::authority_blocks_backward_inflight_write. The violation
    // itself was already recorded by the monotonic record_inflight_invariant
    // above (downgraded to WARN for this skipped-write case — see #3552).
    if enforce_skips_backward_write {
        tracing::warn!(
            "#3416 enforce: skipped backward inflight write at {}",
            path.display()
        );
        return false;
    }
    true
}

pub(super) fn save_inflight_state(state: &InflightTurnState) -> Result<(), String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_in_root(&root, state)
}

/// #897 counter-model review P2 #1 — atomic "create, don't overwrite"
/// variant of `save_inflight_state`. Used by `POST /api/inflight/rebind` so a
/// concurrent legitimate turn that wins the mailbox race between the rebind
/// handler's existence check and its write cannot have its canonical
/// inflight file silently overwritten by the synthetic rebind state
/// (`user_msg_id=0`, placeholder ids zeroed). Returns `InflightAlreadyExists`
/// when the target path is already occupied — the handler translates that
/// into HTTP 409 and the operator retries (or leaves it to the live turn).
#[derive(Debug)]
pub(super) enum CreateNewInflightError {
    /// A state file already exists at the target path — another path wrote
    /// it between the caller's preflight check and this call.
    AlreadyExists,
    /// Filesystem or serialization failure.
    Internal(String),
}

impl std::fmt::Display for CreateNewInflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => write!(f, "inflight state already exists"),
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

pub(super) fn save_inflight_state_create_new(
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    let Some(root) = inflight_runtime_root() else {
        return Err(CreateNewInflightError::Internal(
            "Home directory not found".to_string(),
        ));
    };
    save_inflight_state_create_new_in_root(&root, state)
}

/// Test-visible inner form of `save_inflight_state_create_new`. Takes an
/// explicit root so unit tests can exercise the O_CREAT|O_EXCL semantics
/// without tripping over `AGENTDESK_ROOT_DIR` env-var races.
fn save_inflight_state_create_new_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    let Some(provider) = state.provider_kind() else {
        return Err(CreateNewInflightError::Internal(format!(
            "Unknown provider '{}'",
            state.provider
        )));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
    }
    let _lock = lock_inflight_state_path(&path).map_err(CreateNewInflightError::Internal)?;
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_create_new_in_root",
    );
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated)
        .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;

    // `OpenOptions::create_new(true)` is the canonical atomic check-and-
    // create primitive on POSIX (O_CREAT | O_EXCL). No reliance on a
    // preceding `load_inflight_state` — the kernel itself serializes this.
    use std::io::Write;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(json.as_bytes())
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            file.sync_all()
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CreateNewInflightError::AlreadyExists)
        }
        Err(e) => Err(CreateNewInflightError::Internal(e.to_string())),
    }
}

fn save_inflight_state_in_root(root: &Path, state: &InflightTurnState) -> Result<(), String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_inflight_state_path(&path)?;
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_in_root",
    ) {
        return Ok(());
    }
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

/// #3107 codex re-review (P1): atomic compare-and-set save. Writes `state` ONLY
/// when no inflight row exists for `(provider, channel_id)`, returning `true` iff
/// it wrote. The watcher self-heal re-acquire previously did a non-atomic
/// `load(...).is_some()` preflight + unconditional save: a concurrent intake
/// could create a REAL inflight in the gap, and the synthetic `user_msg_id = 0`
/// save would clobber it (lost turn). This closes the window by doing the check
/// AND write under the same `lock_inflight_state_path` flock the other save/clear
/// paths serialize on, so the synthetic row is written only when there is
/// genuinely no inflight at the moment of the atomic write.
pub(super) fn save_inflight_state_if_absent(state: &InflightTurnState) -> Result<bool, String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_if_absent_in_root(&root, state)
}

fn save_inflight_state_if_absent_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<bool, String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Hold the sidecar flock across the existence check AND the write so a
    // concurrent intake `save_inflight_state_in_root` (which takes the same
    // lock) cannot land a real inflight in the gap. `path.exists()` under the
    // lock is the compare; `atomic_write` is the set.
    let _lock = lock_inflight_state_path(&path)?;
    if path.exists() {
        return Ok(false);
    }
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_absent_in_root",
    );
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    Ok(true)
}

/// Outcome of [`save_inflight_state_if_matches_identity`] — the #3041 P1-2 R3
/// identity-guarded re-save used on a delivery-lease `Skip` epilogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum GuardedSaveOutcome {
    /// On-disk row still matched the turn identity; the row was rewritten.
    Saved,
    /// No inflight row existed (the lease HOLDER already cleared it on its
    /// success path). We do NOT resurrect it — the turn is already delivered.
    Missing,
    /// A row existed but its identity did NOT match (a newer turn replaced it,
    /// or a planned-restart / rebind-origin marker now owns the row). We do
    /// NOT clobber it.
    IdentityMismatch,
    /// Filesystem / serialization error during the write.
    IoError,
}

/// #3041 P1-2 (codex P1-2 R3): identity-guarded re-save for the bridge's
/// delivery-lease `Skip` epilogue. On a Skip the live HOLDER (watcher) owns the
/// turn and CLEARS the row on success, so the bridge epilogue must NOT blindly
/// `save_inflight_state`: if the holder's clear won the race, a blind re-save
/// would resurrect a STALE row for an already-delivered turn (recovery then sees
/// it delivered, never clears, leaks the row). This closes the window the same
/// way `clear_inflight_state_if_matches` (#2427 D-wire) does: under the lock,
/// write only when the row is STILL present AND its `(user_msg_id, started_at,
/// tmux_session_name)` identity (+ `turn_start_offset` when known) matches. Gone
/// (`Missing`) or replaced by a newer turn / restart-rebind marker
/// (`IdentityMismatch`) → no-op; holder FAILED + didn't clear → still present &
/// matching → refresh (`Saved`). Same flock + atomic_write primitives as the
/// rest of the module (Windows-safe).
pub(in crate::services::discord) fn save_inflight_state_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: u64,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
        expected_last_offset,
    )
}

fn save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    save_existing_inflight_rebind_adoption_impl_in_root(
        root,
        state,
        expected,
        expected_turn_start_offset,
        None,
    )
}

fn save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: u64,
) -> GuardedSaveOutcome {
    save_existing_inflight_rebind_adoption_impl_in_root(
        root,
        state,
        expected,
        expected_turn_start_offset,
        Some(expected_last_offset),
    )
}

fn save_existing_inflight_rebind_adoption_impl_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset_for_rebase: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedSaveOutcome::IdentityMismatch;
    };
    if on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode != state.restart_mode {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    if expected_last_offset_for_rebase
        .is_some_and(|expected_last| on_disk.last_offset != expected_last)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let mut updated = on_disk;
    updated.tmux_session_name = state.tmux_session_name.clone();
    updated.output_path = state.output_path.clone();
    updated.input_fifo_path = state.input_fifo_path.clone();
    updated.set_relay_owner_kind(state.effective_relay_owner_kind());
    if expected_last_offset_for_rebase.is_some() {
        updated.last_offset = state.last_offset;
        updated.turn_start_offset = state.turn_start_offset;
        updated.last_watcher_relayed_offset = state.last_watcher_relayed_offset;
        updated.last_watcher_relayed_generation_mtime_ns =
            state.last_watcher_relayed_generation_mtime_ns;
    }
    updated.ensure_finalizer_turn_id();
    let _ = validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_existing_inflight_rebind_adoption_if_matches_identity_in_root",
    );
    updated.updated_at = now_string();
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "existing inflight rebind adoption save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// Root-explicit inner form of [`save_inflight_state_if_matches_identity`] for
/// unit tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(super) fn save_inflight_state_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    // Hold the sidecar flock across the read AND the write so a concurrent
    // holder `clear_inflight_state` (which takes the same lock) cannot land its
    // remove between our identity check and our write.
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    // Holder already cleared the row on its success path → do NOT resurrect.
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        // Malformed row: treat like a mismatch and do not clobber — the loader
        // eviction path GCs malformed payloads on the next read.
        return GuardedSaveOutcome::IdentityMismatch;
    };
    // A newer turn (different identity) or a planned-restart / rebind-origin
    // marker now owns the row — never overwrite it with this preserved turn.
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    // #3089 B3: verdict observe-only here — this path already identity/offset-
    // gates above; the #3416 backward vector is the plain overwrite tails.
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    let _ = validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_matches_identity_in_root",
    );
    updated.updated_at = now_string();
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

// ---------------------------------------------------------------------------
// #3077: typed status-panel ownership writes.
//
// `status_message_id` is the de-facto "this turn owns status panel M" pointer,
// and several independent actors (turn-bridge completion fallback, tmux watcher
// TUI-direct publish/orphan-cleanup, placeholder sweeper) used to mutate it via
// a non-atomic `load_inflight_state(...)` → `state.status_message_id = …` →
// `save_inflight_state(...)` triple. Because the read and the write were not
// serialized against each other, a newer turn that rebound the panel between a
// stale actor's load and its blind `= None` could have its panel silently
// orphaned (the #3099/#3100/#3105/#3107 panel-lifecycle race family).
//
// These helpers centralize that read-modify-write behind intentful operations
// that hold the same `lock_inflight_state_path` sidecar flock across the whole
// compare-and-set, exactly like `save_inflight_state_if_absent`. Callers no
// longer touch the field directly; they declare *what* they own and *under
// which precondition*, and the store enforces it atomically.

/// Per-turn precondition for `bind_status_panel`. Lets each caller carry its
/// own ownership invariant into the lock-held read-modify-write so the guard
/// check and the write cannot be split by a concurrent writer (TOCTOU).
#[derive(Debug, Clone, Default)]
pub(in crate::services::discord) struct StatusPanelBindGuard {
    /// Bind only when the on-disk row still belongs to this `user_msg_id`.
    /// `None` means "do not guard on user_msg_id" (used by callers that have
    /// already established identity another way). Mirrors the turn-bridge
    /// status-panel-v2 completion fallback guard.
    pub require_user_msg_id: Option<u64>,
    /// Bind only when the on-disk row still matches this full turn identity
    /// (user_msg_id + started_at + tmux_session_name). Mirrors the tmux
    /// watcher TUI-direct publish guard that defeats turn handoff during the
    /// Discord send await.
    pub require_identity: Option<InflightTurnIdentity>,
    /// When true, do not overwrite a real (non-synthetic) panel id already on
    /// the row — an overlapping actor already published the canonical panel
    /// and our send is a duplicate (#3003 reclaim path). Synthetic-headless
    /// ids do not count as "already set".
    pub skip_if_panel_already_set: bool,
}

/// Outcome of a `bind_status_panel` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum StatusPanelBindOutcome {
    /// The row was found, passed the guard, and now carries `msg_id`.
    Bound,
    /// The row already carried `msg_id`; nothing was written.
    AlreadyBound,
    /// The row exists but a DIFFERENT real panel id is already set and
    /// `skip_if_panel_already_set` was requested — left untouched. Carries the
    /// row's currently-owned (real) panel id as observed under the same flock,
    /// so the caller can adopt the row's actual panel without a second
    /// (racy) re-read of the inflight row.
    SkippedPanelAlreadySet(u64),
    /// No inflight row exists for `(provider, channel_id)`.
    Missing,
    /// The on-disk row did not satisfy `require_user_msg_id` /
    /// `require_identity` — left untouched (a different turn now owns the row).
    GuardMismatch,
    /// Filesystem / serialization failure while persisting the bind.
    IoError,
}

/// Intentful "this turn now owns status panel `msg_id`" write. Performs the
/// guard check and the field set atomically under the inflight sidecar flock,
/// so it is consistent with `save_inflight_state` / `save_inflight_state_if_absent`.
pub(in crate::services::discord) fn bind_status_panel(
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelBindGuard,
) -> StatusPanelBindOutcome {
    let Some(root) = inflight_runtime_root() else {
        return StatusPanelBindOutcome::IoError;
    };
    bind_status_panel_in_root(&root, provider, channel_id, msg_id, guard)
}

fn bind_status_panel_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelBindGuard,
) -> StatusPanelBindOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return StatusPanelBindOutcome::IoError;
    }
    // Hold the sidecar flock across load → guard → set so a concurrent
    // writer cannot land between the guard check and the write.
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return StatusPanelBindOutcome::IoError;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return StatusPanelBindOutcome::Missing;
    };
    if let Some(expected) = guard.require_user_msg_id
        && state.user_msg_id != expected
    {
        return StatusPanelBindOutcome::GuardMismatch;
    }
    if let Some(expected) = guard.require_identity.as_ref()
        && !expected.matches_state(&state)
    {
        return StatusPanelBindOutcome::GuardMismatch;
    }
    // Same-id re-bind is idempotent and must classify as `AlreadyBound`
    // REGARDLESS of `skip_if_panel_already_set` — an idempotent re-bind of the
    // panel this row already owns is a no-op, not a "duplicate skip". Checking
    // the skip flag first (#3077 codex P2 #1) misclassified a same-id re-bind as
    // `SkippedPanelAlreadySet`, which the TUI-direct caller then routed to a
    // DELETE of the row's own already-bound panel. Order: same-id → AlreadyBound;
    // else a DIFFERENT real id already set + skip flag → SkippedPanelAlreadySet.
    if state.status_message_id == Some(msg_id) {
        return StatusPanelBindOutcome::AlreadyBound;
    }
    if guard.skip_if_panel_already_set && status_panel_id_is_real(state.status_message_id) {
        // Safe: `status_panel_id_is_real` only returns true for `Some(real)`,
        // and we already handled `Some(msg_id)` above, so this is a DIFFERENT
        // real panel id. Carry it so the caller adopts the row's owned panel.
        return StatusPanelBindOutcome::SkippedPanelAlreadySet(
            state.status_message_id.unwrap_or_default(),
        );
    }
    state.status_message_id = Some(msg_id);
    match persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:bind_status_panel_in_root",
    ) {
        Ok(()) => StatusPanelBindOutcome::Bound,
        Err(_) => StatusPanelBindOutcome::IoError,
    }
}

/// Per-turn precondition for `clear_status_panel_if_current`. The msg-id
/// compare-and-clear is unconditional; these add the caller's extra ownership
/// guards (so a sweeper/cleanup that loaded a stale snapshot does not clear a
/// row a newer turn already advanced).
#[derive(Debug, Clone, Default)]
pub(in crate::services::discord) struct StatusPanelClearGuard {
    /// Clear only when the on-disk row still belongs to this `user_msg_id`.
    pub require_user_msg_id: Option<u64>,
    /// Clear only when the on-disk row still carries this `current_msg_id`
    /// (placeholder sweeper convergence guard).
    pub require_current_msg_id: Option<u64>,
    /// Clear only when the on-disk row's tmux session still matches (watcher
    /// orphan-cleanup guard).
    pub require_tmux_session_name: Option<String>,
}

/// Compare-and-clear: clears `status_message_id` ONLY when it currently equals
/// `msg_id` (and every guard precondition holds). Returns `true` iff it
/// cleared. This is the #3077 hardening — a blind `= None` becomes a
/// compare-and-clear so a panel a *newer* turn rebound is never wiped by a
/// stale actor that loaded an older snapshot.
pub(in crate::services::discord) fn clear_status_panel_if_current(
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelClearGuard,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    clear_status_panel_if_current_in_root(&root, provider, channel_id, msg_id, guard)
}

fn clear_status_panel_if_current_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelClearGuard,
) -> bool {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return false;
    };
    if state.status_message_id != Some(msg_id) {
        return false;
    }
    if let Some(expected) = guard.require_user_msg_id
        && state.user_msg_id != expected
    {
        return false;
    }
    if let Some(expected) = guard.require_current_msg_id
        && state.current_msg_id != expected
    {
        return false;
    }
    if let Some(expected) = guard.require_tmux_session_name.as_deref()
        && state.tmux_session_name.as_deref() != Some(expected)
    {
        return false;
    }
    state.status_message_id = None;
    persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:clear_status_panel_if_current_in_root",
    )
    .is_ok()
}

/// #3351: compare-and-clear for the persisted relay-placeholder id, mirroring
/// `clear_status_panel_if_current` (#3077): exact `msg_id` only, placeholderless
/// turns (`current_msg_id == user_msg_id`) untouched, optional tmux-session guard.
pub(in crate::services::discord) fn clear_current_msg_if_matches(
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    require_tmux_session_name: Option<&str>,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    clear_current_msg_if_matches_in_root(
        &root,
        provider,
        channel_id,
        msg_id,
        require_tmux_session_name,
    )
}

fn clear_current_msg_if_matches_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    require_tmux_session_name: Option<&str>,
) -> bool {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return false;
    };
    if msg_id == 0 || state.current_msg_id != msg_id {
        // A newer turn already advanced the anchor — never wipe it.
        return false;
    }
    if state.user_msg_id != 0 && state.current_msg_id == state.user_msg_id {
        // Placeholderless turn: anchor mirrors the user's own message id.
        return false;
    }
    if let Some(expected) = require_tmux_session_name
        && state.tmux_session_name.as_deref() != Some(expected)
    {
        return false;
    }
    state.current_msg_id = 0;
    persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:clear_current_msg_if_matches_in_root",
    )
    .is_ok()
}

/// `true` when `id` is a real Discord panel id (present and not a synthetic
/// headless placeholder). Mirrors `turn_bridge::normalize_status_panel_message_id`
/// without pulling in the serenity `MessageId` newtype.
fn status_panel_id_is_real(id: Option<u64>) -> bool {
    match id {
        Some(value) => !super::is_synthetic_headless_message_id_raw(value),
        None => false,
    }
}

/// Reads + deserializes the inflight row at `path` while the caller holds the
/// sidecar flock. Returns `None` on a missing/malformed file (same lenient
/// posture as `load_inflight_state`).
fn load_inflight_state_unlocked(path: &Path) -> Option<InflightTurnState> {
    let data = fs::read_to_string(path).ok()?;
    parse_inflight_state_content(&data).ok()
}

/// Shared lock-held persist tail: validate, stamp `updated_at`, atomic-write.
/// Caller must already hold `lock_inflight_state_path`.
fn persist_under_lock(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> Result<(), String> {
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(root, path, &updated, caller) {
        return Ok(());
    }
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(path, &json)
}

pub(crate) fn clear_inflight_state(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    fs::remove_file(path).is_ok()
}

/// Outcome of an explicit-signal cleanup attempt that is guarded against
/// racing the next turn's inflight write (#2427 Pitfall #1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardedClearOutcome {
    /// File matched the expected `user_msg_id` and was removed.
    Cleared,
    /// File existed but a different `user_msg_id` was on disk — the next
    /// turn already wrote its inflight, so we leave it alone.
    UserMsgMismatch,
    /// File on disk is a planned-restart marker (`restart_mode` set). The
    /// caller is an explicit cleanup signal that fired for the previous
    /// generation, so the marker must be preserved for recovery.
    PlannedRestartSkipped,
    /// File on disk is a rebind origin (`rebind_origin = true`). Its
    /// lifetime is owned by `/api/inflight/rebind`, not the watcher /
    /// turn-bridge, so the cleanup signal does not apply.
    RebindOriginSkipped,
    /// No inflight file existed (already cleared by a peer / never written).
    Missing,
    /// Filesystem error during the final `remove_file` step. Distinguished
    /// from `Missing` so callers can surface the cleanup failure (warn/error
    /// log + do NOT cancel the watcher, since the inflight is still on
    /// disk and the next sweeper tick will retry). Codex review HIGH on
    /// PR #2460: previously these errors were silently bucketed as Missing,
    /// hiding broken cleanup from the operator while the 1800s safety-net
    /// did the real work.
    IoError,
}

/// Idempotent inflight cleanup driven by an *explicit* turn-completion
/// signal (`TurnCompleted` emit, pane death detection, etc.). This is the
/// #2427 D / A wire — the regular completion-path hook may have already
/// cleared the file (Cleared turns into Missing), so we only act when the
/// on-disk inflight still describes the turn we believe just finished.
///
/// Guards:
/// * `expected_user_msg_id` — required to defeat the Pitfall #1 race. It
///   matches either the Discord `user_msg_id` or the row's `finalizer_turn_id`;
///   `0` is treated as "no guard available" and refused.
/// * `restart_mode = Some(_)` — preserved (planned drain/hot-swap turns
///   must survive across the dcserver restart they were saved for).
/// * `rebind_origin = true` — preserved (Pitfall #5).
pub(crate) fn clear_inflight_state_if_matches(
    provider: &ProviderKind,
    channel_id: u64,
    expected_user_msg_id: u64,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_in_root(&root, provider, channel_id, expected_user_msg_id)
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_identity_in_root(&root, provider, channel_id, expected)
}

pub(in crate::services::discord) fn clear_rebind_origin_inflight_state_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_rebind_origin_inflight_state_if_matches_identity_in_root(
        &root, provider, channel_id, expected,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_after_delivery(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    full_response: &str,
    response_sent_offset: usize,
    last_offset: u64,
) -> (GuardedClearOutcome, bool) {
    let Some(root) = inflight_runtime_root() else {
        return (GuardedClearOutcome::Missing, false);
    };
    clear_inflight_state_if_matches_identity_after_delivery_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
        full_response,
        response_sent_offset,
        last_offset,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn refresh_inflight_last_offset_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    output_path: &str,
    expected_current_msg_id: Option<u64>,
    last_offset: u64,
    caller_owner: RelayOwnerKind,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    refresh_inflight_last_offset_if_matches_identity_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
        output_path,
        expected_current_msg_id,
        last_offset,
        caller_owner,
    )
}

/// Root-explicit variant for unit tests. Production callers should use
/// [`clear_inflight_state_if_matches`].
pub(super) fn clear_inflight_state_if_matches_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected_user_msg_id: u64,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = parse_inflight_state_content(&data) else {
        // Malformed file: treat like Missing — the loader-side eviction
        // will GC the malformed payload on the next read.
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if !state.matches_finalizer_turn_id(expected_user_msg_id) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    // #2450: save and guarded-clear share the same sidecar lock, so the
    // read/validate/unlink sequence below cannot race a concurrent
    // atomic-write rename for a fresh turn.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(pre) = fs::metadata(&path) else {
            return GuardedClearOutcome::Missing;
        };
        let Ok(post) = fs::metadata(&path) else {
            return GuardedClearOutcome::Missing;
        };
        if pre.dev() != post.dev() || pre.ino() != post.ino() {
            return GuardedClearOutcome::UserMsgMismatch;
        }
        // Final re-read + re-validate before unlink keeps the older
        // corruption/mismatch protections intact while the sidecar lock
        // closes the save-vs-clear race.
        let Ok(reread) = fs::read_to_string(&path) else {
            return GuardedClearOutcome::Missing;
        };
        let Ok(restate) = parse_inflight_state_content(&reread) else {
            return GuardedClearOutcome::Missing;
        };
        if !restate.matches_finalizer_turn_id(expected_user_msg_id)
            || restate.restart_mode.is_some()
            || restate.rebind_origin
        {
            return GuardedClearOutcome::UserMsgMismatch;
        }
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected_user_msg_id,
                error = %error,
                "inflight guarded-clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

/// #3161 (codex P1): zero-id epilogue/guard cleanup that is STILL identity-safe.
///
/// A zero-id turn (recovery / external-input / cluster-relay synthesized;
/// zero-normalized at [`optional_message_id`]) cannot be authenticated against a
/// non-zero `expected_user_msg_id`, so [`clear_inflight_state_if_matches`]
/// deliberately refuses (`expected_user_msg_id == 0` → `UserMsgMismatch`) to
/// avoid blind-deleting a row it cannot prove ownership of. But a zero-id turn
/// still legitimately owns *its own* row (whose on-disk `user_msg_id` is also 0)
/// and must clean it up — recovery cleanup depends on this.
///
/// This helper closes that gap: it clears ONLY when the on-disk row's
/// `user_msg_id` is itself 0 (a genuine zero-id-owned row). If a NEWER real
/// (non-zero) identity turn has since written its row, the on-disk
/// `user_msg_id != 0` and we return `UserMsgMismatch` — preserving the newer
/// owner so its status panel can still complete. Planned-restart markers and
/// rebind origins are preserved exactly like the non-zero guarded clear.
pub(crate) fn clear_inflight_state_if_matches_zero_owned(
    provider: &ProviderKind,
    channel_id: u64,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_zero_owned_in_root(&root, provider, channel_id)
}

/// Root-explicit variant of [`clear_inflight_state_if_matches_zero_owned`] for
/// unit tests.
pub(super) fn clear_inflight_state_if_matches_zero_owned_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    // The only thing a zero-id turn may clear is a zero-id-owned row. A newer
    // non-zero owner has `user_msg_id != 0` → preserve it.
    if state.user_msg_id != 0 {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight zero-owned guarded-clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

/// #3859: true when the row anchors a finalizable "🔄 처리 중" placeholder — a real
/// placeholder message id (not 0, and not the placeholderless shape where the
/// anchor mirrors the user's own message id), that is still a PURE placeholder
/// (no streamed assistant content) or an explicitly-active long-running card.
/// Mirrors the placeholder sweeper's abandon-eligibility gate so partial-output
/// failure rows keep their delivered text (no "중단됨" clobber) exactly as the
/// pre-#3859 path did.
fn row_has_finalizable_placeholder(state: &InflightTurnState) -> bool {
    if state.current_msg_id == 0
        || (state.user_msg_id != 0 && state.current_msg_id == state.user_msg_id)
    {
        return false;
    }
    state.long_running_placeholder_active
        || (state.full_response.is_empty() && state.response_sent_offset == 0)
}

/// #3859: record a durable abandon-request for `state`'s placeholder so the
/// async `placeholder_sweeper` drain finalizes it to "중단됨" — enqueued UNDER the
/// sidecar lock and BEFORE the caller's unlink, so the request survives even if
/// the process dies right after the delete.
///
/// Returns `true` when it is SAFE for the caller to delete the inflight row:
/// either the row anchors no finalizable placeholder (nothing to strand) OR the
/// abandon-request was DURABLY persisted. Returns `false` ONLY when a finalizable
/// placeholder's record FAILED to persist (#3859 r5 — codex P1); the caller MUST
/// then keep the row so a later sweeper pass retries and the placeholder is never
/// stranded without a record. Invariant: never `(row deleted ∧ record absent)`.
#[must_use]
fn enqueue_abandon_request_for_row(
    provider: &ProviderKind,
    channel_id: u64,
    token_hash: &str,
    state: &InflightTurnState,
) -> bool {
    if !row_has_finalizable_placeholder(state) {
        return true;
    }
    match super::abandon_request_store::enqueue(
        provider,
        token_hash,
        channel_id,
        super::abandon_request_store::AbandonRecord {
            msg_id: state.current_msg_id,
            started_at: state.started_at.clone(),
            current_tool_line: state.current_tool_line.clone(),
        },
    ) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                msg_id = state.current_msg_id,
                error = %error,
                "abandon-request enqueue failed; PRESERVING inflight row so the placeholder is not stranded (sweeper retries next pass)"
            );
            false
        }
    }
}

/// #3859: failure-path cleanup that drives a stranded placeholder to a TERMINAL
/// "중단됨" card WITHOUT keeping the inflight row alive.
///
/// Identical ownership guards to [`clear_inflight_state_if_matches`]
/// (planned-restart / rebind-origin preserved; `UserMsgMismatch` for a newer
/// owner; `expected_user_msg_id == 0` refused) — so a restart/rebind/foreign row
/// is never enqueued or deleted. When the guards pass and the row anchors a
/// finalizable placeholder, a durable abandon-request is enqueued (so the
/// placeholder sweeper finalizes the "🔄 처리 중" card to "중단됨" by message id),
/// then the row is DELETED — freeing the channel immediately like the pre-#3859
/// path. The abandon-request is decoupled from the inflight lifecycle, so a
/// re-adopt (new row + new placeholder) never collides with it (#3859 r4).
pub(crate) fn request_inflight_abandon_if_matches(
    provider: &ProviderKind,
    channel_id: u64,
    expected_user_msg_id: u64,
    token_hash: &str,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    request_inflight_abandon_if_matches_in_root(
        &root,
        provider,
        channel_id,
        expected_user_msg_id,
        token_hash,
    )
}

/// Root-explicit variant for unit tests (the inflight ops use `root`; the
/// abandon-request store is env-rooted via `discord_abandon_requests_root`, so a
/// test must also set `AGENTDESK_ROOT_DIR`).
pub(super) fn request_inflight_abandon_if_matches_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected_user_msg_id: u64,
    token_hash: &str,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = parse_inflight_state_content(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if !state.matches_finalizer_turn_id(expected_user_msg_id) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(pre) = fs::metadata(&path) else {
            return GuardedClearOutcome::Missing;
        };
        let Ok(post) = fs::metadata(&path) else {
            return GuardedClearOutcome::Missing;
        };
        if pre.dev() != post.dev() || pre.ino() != post.ino() {
            return GuardedClearOutcome::UserMsgMismatch;
        }
        let Ok(reread) = fs::read_to_string(&path) else {
            return GuardedClearOutcome::Missing;
        };
        let Ok(restate) = parse_inflight_state_content(&reread) else {
            return GuardedClearOutcome::Missing;
        };
        if !restate.matches_finalizer_turn_id(expected_user_msg_id)
            || restate.restart_mode.is_some()
            || restate.rebind_origin
        {
            return GuardedClearOutcome::UserMsgMismatch;
        }
    }
    // Enqueue BEFORE unlink (durable handoff). #3859 r5 (codex P1): if a
    // FINALIZABLE placeholder's record fails to persist, DO NOT delete the row —
    // return IoError so the sweeper retries (the row stays alive and the
    // placeholder is finalized later). Never delete the row without its record.
    if !enqueue_abandon_request_for_row(provider, channel_id, token_hash, &state) {
        return GuardedClearOutcome::IoError;
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected_user_msg_id,
                error = %error,
                "inflight abandon-request remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

/// #3859 zero-id variant — mirrors [`clear_inflight_state_if_matches_zero_owned`]
/// guards, then enqueues an abandon-request (if the row anchors a finalizable
/// placeholder) and deletes the row.
pub(crate) fn request_inflight_abandon_if_matches_zero_owned(
    provider: &ProviderKind,
    channel_id: u64,
    token_hash: &str,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    request_inflight_abandon_if_matches_zero_owned_in_root(&root, provider, channel_id, token_hash)
}

/// Root-explicit variant of [`request_inflight_abandon_if_matches_zero_owned`].
pub(super) fn request_inflight_abandon_if_matches_zero_owned_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    token_hash: &str,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if state.user_msg_id != 0 {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    // #3859 r5: preserve the row if a finalizable placeholder's record fails to
    // persist (never delete the row without its abandon-request).
    if !enqueue_abandon_request_for_row(provider, channel_id, token_hash, &state) {
        return GuardedClearOutcome::IoError;
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight zero-owned abandon-request remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

fn clear_inflight_state_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&state) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

fn clear_rebind_origin_inflight_state_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if !state.rebind_origin {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    if !expected.matches_state(&state) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "rebind-origin inflight guarded-clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

fn normalize_response_sent_offset(full_response: &str, response_sent_offset: usize) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn clear_inflight_state_if_matches_identity_after_delivery_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    full_response: &str,
    response_sent_offset: usize,
    last_offset: u64,
) -> (GuardedClearOutcome, bool) {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return (GuardedClearOutcome::IoError, false);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return (GuardedClearOutcome::Missing, false);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return (GuardedClearOutcome::Missing, false);
    };
    if state.restart_mode.is_some() {
        return (GuardedClearOutcome::PlannedRestartSkipped, false);
    }
    if state.rebind_origin {
        return (GuardedClearOutcome::RebindOriginSkipped, false);
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&state) {
        return (GuardedClearOutcome::UserMsgMismatch, false);
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if state.turn_start_offset != Some(expected_offset) {
            return (GuardedClearOutcome::UserMsgMismatch, false);
        }
    }

    let mut delivered_state = state;
    delivered_state.full_response = full_response.to_string();
    delivered_state.response_sent_offset =
        normalize_response_sent_offset(full_response, response_sent_offset);
    delivered_state.last_offset = last_offset;
    delivered_state.ensure_finalizer_turn_id();
    delivered_state.updated_at = now_string();

    let mirrored_delivery = match serde_json::to_string_pretty(&delivered_state)
        .map_err(|error| error.to_string())
        .and_then(|json| atomic_write(&path, &json))
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight delivery mirror failed before identity-guarded clear"
            );
            false
        }
    };

    let outcome = match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded delivery clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    };
    (outcome, mirrored_delivery)
}

// #3034: root-parameterized variant exercised directly by the unit tests
// (the prod wrapper was removed; tests drive a tempdir root). Test-only seam.
#[allow(dead_code)]
fn clear_inflight_state_if_matches_tmux_response_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    response: &str,
) -> GuardedClearOutcome {
    let tmux_session_name = tmux_session_name.trim();
    let response = response.trim();
    if tmux_session_name.is_empty() || response.is_empty() {
        return GuardedClearOutcome::UserMsgMismatch;
    }

    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if state.tmux_session_name.as_deref().map(str::trim) != Some(tmux_session_name) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    if state.full_response.trim() != response {
        return GuardedClearOutcome::UserMsgMismatch;
    }

    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                tmux_session_name,
                error = %error,
                "inflight tmux-response guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn refresh_inflight_last_offset_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    output_path: &str,
    expected_current_msg_id: Option<u64>,
    last_offset: u64,
    caller_owner: RelayOwnerKind,
) -> bool {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(mut state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return false;
    };
    if state.restart_mode.is_some() || state.rebind_origin {
        return false;
    }
    if state.output_path.as_deref() != Some(output_path) {
        return false;
    }
    if let Some(expected_msg_id) = expected_current_msg_id {
        if state.current_msg_id != expected_msg_id {
            return false;
        }
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&state) {
        return false;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if state.turn_start_offset != Some(expected_offset) {
            return false;
        }
    }

    // I6 (last_offset_owner_gated): the persisted watermark is advanced only
    // by the current relay owner. A non-owner caller (standby/idle) follows
    // the authoritative offset read-only and must yield to a live owner. The
    // identity guards above already proved this is the SAME turn, so a live
    // owner that differs from the caller is an authority conflict, not a
    // fresh-turn reset.
    let persisted_owner = state.effective_relay_owner_kind();
    let owner_is_live = !matches!(persisted_owner, RelayOwnerKind::None);
    if owner_is_live && persisted_owner != caller_owner {
        record_inflight_invariant(
            false,
            &state,
            "last_offset_owner_gated",
            "src/services/discord/inflight.rs:refresh_inflight_last_offset_if_matches_identity_in_root",
            "inflight last_offset must only be advanced by the current relay owner",
            serde_json::json!({
                "persisted_owner": persisted_owner.as_str(),
                "caller_owner": caller_owner.as_str(),
                "previous": state.last_offset,
                "next": last_offset,
                "path": path.display().to_string(),
            }),
        );
        return false;
    }

    // I6 (last_offset_monotonic): same identity, so a backward watermark write
    // would clobber the authoritative offset and replay a stale transcript
    // tail. Reject and record. A fresh-turn reset is already excluded by the
    // identity guards above.
    if last_offset < state.last_offset {
        record_inflight_invariant(
            false,
            &state,
            "last_offset_monotonic",
            "src/services/discord/inflight.rs:refresh_inflight_last_offset_if_matches_identity_in_root",
            "inflight last_offset must not move backwards for the same turn identity",
            serde_json::json!({
                "previous": state.last_offset,
                "next": last_offset,
                "path": path.display().to_string(),
            }),
        );
        return false;
    }

    state.last_offset = last_offset;
    state.ensure_finalizer_turn_id();
    state.updated_at = now_string();
    serde_json::to_string_pretty(&state)
        .map_err(|error| error.to_string())
        .and_then(|json| atomic_write(&path, &json))
        .is_ok()
}

fn inflight_state_allows_idle_tmux_repair_state(state: &InflightTurnState) -> bool {
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
    match serde_json::to_string_pretty(&state) {
        Ok(json) => atomic_write(path, &json).is_ok(),
        Err(_) => false,
    }
}

/// #2437 (#2427 C wire) boot-time bulk invalidate. Removes inflight
/// state files whose `restart_generation` does not match
/// `current_generation` AND that are NOT planned-restart rows. The
/// planned-restart gate in `stale_removal_reason` (this file, the
/// `state.restart_mode.is_some()` branch) already handles its own
/// generation-mismatch eviction with `DRAIN_RESTART_MAX_AGE_SECS` /
/// `HOT_SWAP_HANDOFF_MAX_AGE_SECS` retention — do not double-evict
/// those here or recovery will lose handoff rows from the prior
/// generation.
///
/// Skips:
///   * `state.restart_mode.is_some()` — planned restart / hot-swap.
///   * `state.rebind_origin` — rebind API owns these, not generation.
///   * `state.restart_generation == Some(current_generation)` — this
///     generation's own rows.
///
/// Returns the number of state files removed. Intended to be called
/// **once per provider** at dcserver boot, BEFORE
/// `restore_inflight_turns`, so recovery does not revive a row from a
/// generation whose tmux session no longer exists.
pub(crate) fn invalidate_stale_generation(
    provider: &ProviderKind,
    current_generation: u64,
) -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    let removed = invalidate_stale_generation_in_root(&root, provider, current_generation);
    removed.len()
}

/// Test-friendly variant. Returns the list of evicted `(channel_id,
/// row_generation)` tuples so unit tests can pin both the count and
/// the row identities without re-loading the directory.
fn invalidate_stale_generation_in_root(
    root: &Path,
    provider: &ProviderKind,
    current_generation: u64,
) -> Vec<(u64, Option<u64>)> {
    let states = load_inflight_states_from_root(root, provider);
    let mut removed = Vec::new();
    for state in states {
        if state.restart_mode.is_some() {
            continue;
        }
        if state.rebind_origin {
            // #3581: a rebind-origin row is normally owned by the rebind API
            // and skipped here. The one exception is an abandoned, never-
            // progressed orphan from a STALL-WATCHDOG respawn: reap it at boot
            // if it is past its deadline OR was born in a prior generation.
            // The reap predicate's strict conjunction guarantees a live /
            // adopted rebind is never touched.
            //
            // #3581 (codex TOCTOU fix): gate the unlocked-snapshot pre-check
            // with the same locked re-validate-then-unlink helper the periodic
            // sweeper now uses, so boot and sweeper stay consistent and a row
            // replaced between the snapshot and the lock is never wiped.
            let path = inflight_state_path(root, provider, state.channel_id);
            let age_secs = rebind_origin_age_secs(&path, &state);
            if should_reap_abandoned_rebind_origin(&state, age_secs, current_generation)
                && reap_abandoned_rebind_origin_locked_in_root(
                    root,
                    provider,
                    &state,
                    current_generation,
                ) == RebindReapOutcome::Reaped
            {
                emit_reap_abandoned_rebind_origin(
                    provider,
                    &state,
                    age_secs,
                    current_generation,
                    "invalidate_stale_generation_boot",
                );
                removed.push((state.channel_id, state.rebind_origin_birth_generation));
            }
            continue;
        }
        // Codex review HIGH on PR #2460: normal rows are constructed with
        // `restart_generation: None` (see `InflightTurnState::new`). The
        // previous `Some(current_generation)` guard alone would evict every
        // healthy current-generation row at boot. Preserve unstamped rows
        // too so only rows explicitly stamped from a PRIOR generation are
        // evicted. (Stale unstamped rows are still bounded by the
        // intake-time staleness threshold path; this function is the
        // boot-time hammer, not the long-lived cleaner.)
        match state.restart_generation {
            None => continue,
            Some(row_generation) if row_generation == current_generation => continue,
            Some(_) => {}
        }
        let path = inflight_state_path(root, provider, state.channel_id);
        let Ok(_lock) = lock_inflight_state_path(&path) else {
            continue;
        };
        let Some(state) = read_inflight_state_content(&path) else {
            continue;
        };
        if state.provider_kind().as_ref() != Some(provider) {
            continue;
        }
        if state.restart_mode.is_some() {
            continue;
        }
        if state.rebind_origin {
            continue;
        }
        match state.restart_generation {
            None => continue,
            Some(row_generation) if row_generation == current_generation => continue,
            Some(_) => {}
        }
        if fs::remove_file(&path).is_ok() {
            // Only emit observability when called via the env wrapper —
            // raw `_in_root` calls are unit tests and we want to keep
            // them deterministic.
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                state.channel_id,
                state.dispatch_id.as_deref(),
                None,
                None,
                "evict_stale_generation",
                serde_json::json!({
                    "reason": "generation_mismatch_boot",
                    "row_generation": state.restart_generation,
                    "current_generation": current_generation,
                    "user_msg_id": state.user_msg_id,
                }),
            );
            removed.push((state.channel_id, state.restart_generation));
        }
    }
    removed
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
pub(super) fn load_inflight_state_read_only(
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

fn planned_restart_retention_secs(restart_mode: InflightRestartMode) -> u64 {
    match restart_mode {
        InflightRestartMode::DrainRestart => DRAIN_RESTART_MAX_AGE_SECS,
        InflightRestartMode::HotSwapHandoff => HOT_SWAP_HANDOFF_MAX_AGE_SECS,
    }
}

/// Thread-local test seam for `tmux_pane_alive_for_stale_check`. Production
/// always calls `tmux_diagnostics::tmux_session_has_live_pane`; tests inject a
/// known-alive name set via `set_test_tmux_alive_override` so the override
/// behaviour can be exercised without spawning real tmux.
#[cfg(test)]
static TEST_TMUX_ALIVE_OVERRIDE: std::sync::OnceLock<
    std::sync::Mutex<Option<std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn set_test_tmux_alive_override(names: Option<&[&str]>) {
    let lock = TEST_TMUX_ALIVE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = lock.lock().expect("tmux alive override lock poisoned");
    *guard = names.map(|slice| slice.iter().map(|s| (*s).to_string()).collect());
}

fn tmux_pane_alive_for_stale_check(name: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(lock) = TEST_TMUX_ALIVE_OVERRIDE.get()
            && let Ok(guard) = lock.lock()
            && let Some(set) = guard.as_ref()
        {
            return set.contains(name);
        }
    }
    crate::services::tmux_diagnostics::tmux_session_has_live_pane(name)
}

fn stale_removal_reason(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> Option<String> {
    match state.restart_mode {
        Some(restart_mode) => {
            if state.restart_generation != Some(current_generation) {
                return Some(format!(
                    "removing {} inflight state from old generation {:?} (current generation {})",
                    restart_mode.label(),
                    state.restart_generation,
                    current_generation
                ));
            }
            let max_age = planned_restart_retention_secs(restart_mode);
            if age_secs > max_age {
                // Defense-in-depth: when DrainRestart inflight ages past the
                // 30-min retention window, refuse to wipe if the inflight's
                // tmux pane is still alive. Wiping the row strands the live
                // CLI's eventual response — see the 2026-05-26 incident where
                // repeated quick-exits left a codex turn pane alive but its
                // inflight anchor was removed at the 10th boot. Only one
                // probe per stale row, gated by all the cheaper checks above.
                if matches!(restart_mode, InflightRestartMode::DrainRestart)
                    && let Some(name) = state.tmux_session_name.as_deref()
                    && tmux_pane_alive_for_stale_check(name)
                {
                    tracing::warn!(
                        "  ⚠ inflight stale-age ({age_secs}s > {max_age}s) overridden — tmux pane '{name}' still alive (channel {})",
                        state.channel_id
                    );
                    return None;
                }
                return Some(format!(
                    "removing stale {} inflight state file ({age_secs}s old > {max_age}s)",
                    restart_mode.label()
                ));
            }
            None
        }
        None => {
            if age_secs > INFLIGHT_MAX_AGE_SECS {
                if let Some(name) = state.tmux_session_name.as_deref()
                    && tmux_pane_alive_for_stale_check(name)
                {
                    tracing::warn!(
                        "  ⚠ inflight stale-age ({age_secs}s > {INFLIGHT_MAX_AGE_SECS}s) overridden — tmux pane '{name}' still alive (channel {})",
                        state.channel_id
                    );
                    return None;
                }
                Some(format!(
                    "removing stale inflight state file ({age_secs}s old > {INFLIGHT_MAX_AGE_SECS}s)"
                ))
            } else {
                None
            }
        }
    }
}

fn stale_removal_reason_for_path(
    path: &Path,
    state: &InflightTurnState,
    current_generation: u64,
) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = modified.elapsed().ok()?;
    stale_removal_reason(state, age.as_secs(), current_generation)
}

fn load_inflight_states_from_root(root: &Path, provider: &ProviderKind) -> Vec<InflightTurnState> {
    let dir = inflight_provider_dir(root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut states = Vec::new();
    let mut tmux_owners: HashMap<String, u64> = HashMap::new();
    let current_generation = super::runtime_store::load_generation();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to read inflight state file: {}",
                path.display()
            );
            continue;
        };
        let (mut state, mut finalizer_backfilled) =
            match parse_inflight_state_content_with_finalizer_backfill(&content) {
                Ok(parsed) => parsed,
                Err(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⚠ removing malformed inflight state file: {}",
                        path.display()
                    );
                    let Ok(_lock) = lock_inflight_state_path(&path) else {
                        continue;
                    };
                    match read_inflight_state_content(&path) {
                        Some(locked_state) => (locked_state, false),
                        None => {
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                    }
                }
            };
        if state.provider_kind().as_ref() != Some(provider) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ removing inflight state with provider mismatch: {}",
                path.display()
            );
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                continue;
            };
            let Some(locked_state) = read_inflight_state_content(&path) else {
                let _ = fs::remove_file(&path);
                continue;
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                let _ = fs::remove_file(&path);
                continue;
            }
            finalizer_backfilled = false;
            state = locked_state;
        }
        if stale_removal_reason_for_path(&path, &state, current_generation).is_some() {
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                continue;
            };
            let Some(locked_state) = read_inflight_state_content(&path) else {
                let _ = fs::remove_file(&path);
                continue;
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                let _ = fs::remove_file(&path);
                continue;
            }
            if let Some(reason) =
                stale_removal_reason_for_path(&path, &locked_state, current_generation)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ⚠ {}: {}", reason, path.display());
                let _ = fs::remove_file(&path);
                continue;
            }
            finalizer_backfilled = false;
            state = locked_state;
        }
        if finalizer_backfilled
            && let Some(locked_state) = backfill_finalizer_turn_id_under_lock(root, &path, provider)
        {
            state = locked_state;
        }
        if let Some(tmux_session_name) = state
            .tmux_session_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            if let Some(previous_channel_id) =
                tmux_owners.insert(tmux_session_name.to_string(), state.channel_id)
            {
                record_inflight_invariant(
                    false,
                    &state,
                    "inflight_tmux_one_to_one",
                    "src/services/discord/inflight.rs:load_inflight_states_from_root",
                    "one tmux session must not be owned by multiple inflight channel files",
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "previous_channel_id": previous_channel_id,
                        "current_channel_id": state.channel_id,
                        "path": path.display().to_string(),
                    }),
                );
            }
        }
        states.push(state);
    }
    states
}

/// #2448: explicit completion signal published from the turn_bridge
/// CompletionGuard so downstream listeners (currently the standby JSONL
/// relay) can exit promptly instead of polling against a wall-clock
/// timeout. Variants are intentionally narrow; add cases as new
/// listeners need them.
#[derive(Debug, Clone)]
pub(in crate::services::discord) enum InflightSignal {
    /// The turn_bridge task for `channel_id` reached its terminal drop —
    /// any per-turn relay tasks bound to this channel may now exit.
    Completed { channel_id: u64 },
}

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

        // A JSON row that omits the new fields entirely (legacy v8 / pre-field
        // shape) must still deserialize, defaulting the follow-up context.
        let mut value = serde_json::to_value(&state).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("followup_reply_context");
        obj.remove("followup_has_reply_boundary");
        obj.remove("followup_merge_consecutive");
        obj.remove("followup_pending_uploads");
        obj.remove("followup_voice_announcement");
        let legacy: InflightTurnState =
            serde_json::from_value(value).expect("legacy row must deserialize");
        assert_eq!(legacy.followup_reply_context, None);
        assert!(!legacy.followup_has_reply_boundary);
        assert!(!legacy.followup_merge_consecutive);
        assert!(legacy.followup_pending_uploads.is_empty());
        assert_eq!(legacy.followup_voice_announcement, None);
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

        assert_eq!(outcome, StatusPanelBindOutcome::Bound);
        assert_eq!(loaded_status_message_id(temp.path(), 7001), Some(5555));
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

        assert_eq!(outcome, StatusPanelBindOutcome::Bound);
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
        assert_eq!(
            bind_status_panel_in_root(
                temp.path(),
                &ProviderKind::Claude,
                7200,
                5555,
                &StatusPanelBindGuard::default(),
            ),
            StatusPanelBindOutcome::Bound
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

    struct EnvReset(Option<std::ffi::OsString>);
    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn set_agentdesk_root_for_test(path: &Path) -> EnvReset {
        let reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        reset
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
    fn synthetic_carry_forward_keeps_reclaim_monotonic_3358() {
        let _serialized = monotonic_3358_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // FIX: birth carried up to the committed frontier → re-claim is forward/
        // equal, ZERO invariant violations, offsets end at the frontier.
        let temp = TempDir::new().unwrap();
        let relay_last_offset: u64 = 2_821_677;
        let committed_frontier: u64 = 2_838_484;
        // Drive the ACTUAL production carry-forward helper (not an inline copy) so
        // this green test honestly tracks the production wiring — if the helper
        // regressed, `carried` would no longer reach the frontier and the
        // monotonicity assertions below would fail. The frontier is `Some(..)`
        // because the watcher advanced it WITHIN this same wrapper generation (the
        // claim choke-point validates that before clamping — #3358 round 2).
        let carried =
            crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
                relay_last_offset,
                Some(committed_frontier),
            );
        assert_eq!(
            carried, committed_frontier,
            "carry-forward must lift birth to the frontier"
        );

        let mut on_disk = build_synth_3358(carried);
        on_disk.full_response = "X".repeat(20_000);
        on_disk.response_sent_offset = 18_000;
        on_disk.last_offset = committed_frontier;
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        // Carried-birth re-claim: turn_start_offset == last_offset == frontier,
        // rso == 0. The rso 0 is NOT a regression because the identity key matches
        // (same turn) and `response_sent_offset_monotonic` only flags within-turn
        // backward moves AFTER bytes were sent — here the re-claim's last_offset
        // equals the on-disk frontier (forward/equal) and rso reset is the
        // documented fresh-claim seed. Assert last_offset never regresses below
        // the committed frontier.
        let carried_reseed = build_synth_3358(carried);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Drive the enforcing watermark path: a carried-birth refresh writes
            // last_offset == committed_frontier — forward/equal, accepted.
            refresh_inflight_last_offset_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Claude,
                321,
                &InflightTurnIdentity::from_state(&carried_reseed),
                carried_reseed.turn_start_offset,
                "/tmp/out.jsonl",
                Some(carried_reseed.current_msg_id),
                committed_frontier,
                RelayOwnerKind::Watcher,
            )
        }));
        assert!(res.is_ok(), "carried-birth refresh must not panic");
        assert!(
            res.unwrap(),
            "carried-birth watermark write is forward/equal — accepted"
        );
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].last_offset, committed_frontier,
            "offsets end at the committed frontier, never regressed"
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
    fn authority_guard_would_suppress_same_turn_frontier_reset_3860() {
        // #3860 SAFETY rationale (why the compiled monotonic-guard default stays
        // OFF and is NOT flipped to enforce-ON in this PR): when ON,
        // `authority_blocks_backward_inflight_write` blocks ANY same-turn backward
        // `response_sent_offset` write — including the LEGITIMATE Gemini/Qwen
        // RetryBoundary reset (turn_bridge/retry_state.rs clears `full_response`
        // and rewinds rso→0 for the SAME identity to re-stream the turn). Blocking
        // it drops the re-streamed body (a real, observed danger — the live
        // AGENTDESK_DELIVERY_RECORD_AUTHORITY=1 config enforce-skips that reset).
        // The root-cause fix (the per-row RMW restart-mode marker) avoids the
        // frontier regression WITHOUT this collateral suppression, so the risky
        // default flip is deferred, not taken here.
        use crate::services::discord::outbound::delivery_record as dr;
        // authority ON + same-turn backward rso (monotonic == false) → blocked
        // (this is the legitimate retry reset that would be wrongly suppressed).
        assert!(dr::authority_blocks_backward_inflight_write(
            true, false, true
        ));
        assert!(dr::authority_blocks_backward_inflight_write(
            true, true, false
        ));
        // authority OFF (compiled default) → never blocks → reset persists.
        assert!(!dr::authority_blocks_backward_inflight_write(
            false, false, true
        ));
        // authority ON but fully monotonic → not blocked (forward writes pass).
        assert!(!dr::authority_blocks_backward_inflight_write(
            true, true, true
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

    /// Skip → the on-disk row became a planned-restart marker. The guarded save
    /// must not clobber it (`IdentityMismatch`); restart recovery owns it.
    #[test]
    fn skip_save_does_not_clobber_planned_restart_marker() {
        let temp = TempDir::new().unwrap();
        let mut marker = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        marker.set_restart_mode(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &marker).unwrap();

        let preserved = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        let expected = InflightTurnIdentity::from_state(&preserved);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &preserved,
            &expected,
            preserved.turn_start_offset,
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].restart_mode.is_some(),
            "the planned-restart marker must be preserved for recovery"
        );
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
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

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
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

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
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

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

    /// RAII guard that points `AGENTDESK_ROOT_DIR` at an isolated tempdir under
    /// the shared env lock and restores the previous value on drop.
    struct IsolatedRootEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl Drop for IsolatedRootEnv {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
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
    fn isolated_root_env(temp: &TempDir) -> IsolatedRootEnv {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var("AGENTDESK_ROOT_DIR").ok();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        IsolatedRootEnv {
            _lock: lock,
            previous,
        }
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
        let previous_root = std::env::var("AGENTDESK_ROOT_DIR").ok();
        let temp = TempDir::new().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        struct EnvRestore(Option<String>);
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _restore = EnvRestore(previous_root);

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
