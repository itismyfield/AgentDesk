use std::fs;
use std::path::Path;

use crate::services::provider::ProviderKind;

use super::{
    InflightTurnIdentity, inflight_runtime_root, inflight_state_path, load_inflight_state_unlocked,
    lock_inflight_state_path, persist_under_lock,
};

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

pub(super) fn bind_status_panel_in_root(
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

pub(super) fn clear_status_panel_if_current_in_root(
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

pub(super) fn clear_current_msg_if_matches_in_root(
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
        Some(value) => !super::super::is_synthetic_headless_message_id_raw(value),
        None => false,
    }
}
