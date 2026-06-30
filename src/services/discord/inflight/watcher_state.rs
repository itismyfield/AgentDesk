use std::fs;
use std::path::Path;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::provider::ProviderKind;

use super::{
    InflightTurnIdentity, inflight_runtime_root, inflight_state_path, load_inflight_state_unlocked,
    lock_inflight_state_path, normalize_response_sent_offset, persist_under_lock,
};

/// #3558: the watcher-owned streaming fields a single-flock RMW patches onto the
/// persisted row. Plain value struct (moved into the helper). `last_offset` is
/// deliberately ABSENT — the streaming caller does not own the relay watermark
/// and the helper preserves whatever the in-lock disk reload carries (this is
/// the core of the TOCTOU fix: the old unlocked load→save re-wrote a stale
/// `last_offset`, racing a concurrent owner-gated `refresh_inflight_last_offset_*`
/// advance and emitting a spurious `last_offset_monotonic` violation).
#[derive(Debug, Clone)]
pub(in crate::services::discord) struct WatcherStreamProgressPatch {
    pub current_msg_id: Option<u64>,
    pub full_response: String,
    pub response_sent_offset: usize,
    pub current_tool_line: Option<String>,
    pub prev_tool_status: Option<String>,
    pub task_notification_kind: Option<TaskNotificationKind>,
    pub any_tool_used: bool,
    pub has_post_tool_text: bool,
    /// #3871: the frozen streamed rollover-prefix Discord message ids accumulated
    /// so far this turn. Union-merged into the reloaded row so the set only ever
    /// grows (a terminal full-body fallback in a later iteration / after restart
    /// can delete every accumulated prefix).
    pub streaming_rollover_frozen_msg_ids: Vec<u64>,
}

/// #3558: outcome of [`persist_watcher_stream_progress_locked_in_root`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherProgressOutcome {
    /// The watcher-owned fields were patched and persisted.
    Saved,
    /// Either no row exists, or the in-lock reload no longer matches the
    /// expected identity / tmux session (a fresh turn replaced it, or a
    /// restart/rebind marker is now pinned). The write was skipped.
    Skipped,
    /// Filesystem or lock acquisition failure.
    IoError,
}

/// #3558: single-flock read-modify-write for the tmux streaming-progress
/// caller. Acquires the sidecar flock ONCE, reloads the on-disk row, re-checks
/// the caller's identity/session guards against the freshly reloaded row, then
/// patches ONLY the watcher-owned streaming fields and persists via
/// [`persist_under_lock`] — never re-entering [`super::save_inflight_state`] (which
/// would re-acquire the same non-reentrant flock and self-deadlock).
///
/// `last_offset` is preserved verbatim from the in-lock reload, so a concurrent
/// owner-gated `refresh_inflight_last_offset_*` advance can no longer be
/// clobbered backward by a stale unlocked snapshot.
pub(in crate::services::discord) fn persist_watcher_stream_progress_locked(
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: Option<&InflightTurnIdentity>,
    require_tmux_session_name: &str,
    patch: WatcherStreamProgressPatch,
) -> WatcherProgressOutcome {
    let Some(root) = inflight_runtime_root() else {
        return WatcherProgressOutcome::IoError;
    };
    persist_watcher_stream_progress_locked_in_root(
        &root,
        provider,
        channel_id,
        require_identity,
        require_tmux_session_name,
        patch,
    )
}

/// Root-explicit variant of [`persist_watcher_stream_progress_locked`] for unit
/// tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(super) fn persist_watcher_stream_progress_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: Option<&InflightTurnIdentity>,
    require_tmux_session_name: &str,
    patch: WatcherStreamProgressPatch,
) -> WatcherProgressOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return WatcherProgressOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return WatcherProgressOutcome::IoError;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return WatcherProgressOutcome::Skipped;
    };
    // A pinned restart/rebind marker means a different lifecycle owns the row;
    // the streaming caller must not touch it (mirrors the refresh-path guard).
    if state.restart_mode.is_some() || state.rebind_origin {
        return WatcherProgressOutcome::Skipped;
    }
    if state.tmux_session_name.as_deref() != Some(require_tmux_session_name) {
        return WatcherProgressOutcome::Skipped;
    }
    // #3558: when the caller has captured a per-turn identity, reject a write
    // onto a fresh row B (different user_msg_id / started_at / turn_start_offset)
    // — exactly the late-frame race the old tmux-session-only guard let through.
    // Before identity is captured (early frames) the caller passes `None` and we
    // fall back to the historical tmux-session-only guard above.
    if let Some(identity) = require_identity
        && !identity.matches_state(&state)
    {
        return WatcherProgressOutcome::Skipped;
    }

    if let Some(msg_id) = patch.current_msg_id {
        state.current_msg_id = msg_id;
    }
    state.full_response = patch.full_response;
    // Recompute the boundary clamp against the freshly reloaded full_response so
    // the persisted offset stays in-bounds even if the disk row's body differs
    // from the caller's last unlocked snapshot.
    state.response_sent_offset =
        normalize_response_sent_offset(&state.full_response, patch.response_sent_offset);
    state.current_tool_line = patch.current_tool_line;
    state.prev_tool_status = patch.prev_tool_status;
    state.any_tool_used = patch.any_tool_used;
    state.has_post_tool_text = patch.has_post_tool_text;
    if patch.task_notification_kind.is_some() {
        state.task_notification_kind = patch.task_notification_kind;
    }
    // #3871: union-merge the frozen rollover-prefix ids into the reloaded row so
    // the set is monotonic (never drops an id another iteration already froze).
    for frozen_id in patch.streaming_rollover_frozen_msg_ids {
        if !state.streaming_rollover_frozen_msg_ids.contains(&frozen_id) {
            state.streaming_rollover_frozen_msg_ids.push(frozen_id);
        }
    }
    // `last_offset` intentionally untouched — preserved from the in-lock reload.

    match persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:persist_watcher_stream_progress_locked_in_root",
    ) {
        Ok(()) => WatcherProgressOutcome::Saved,
        Err(_) => WatcherProgressOutcome::IoError,
    }
}

/// #3558: the watcher-owned fields the terminal-commit RMW writes. Unlike the
/// streaming patch, the commit caller IS the authoritative owner of the
/// turn-end watermark, so it deliberately writes `last_offset` /
/// `response_sent_offset` — but max-serializes them against the in-lock reload
/// so a late commit observing a newer disk watermark never moves it backward.
pub(in crate::services::discord) struct WatcherTerminalCommitPatch {
    pub full_response: String,
    pub last_offset: u64,
    pub last_watcher_relayed_offset: Option<u64>,
    pub last_watcher_relayed_generation_mtime_ns: Option<i64>,
}

/// #3558: outcome of [`commit_watcher_terminal_delivery_locked_in_root`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherTerminalCommitOutcome {
    Committed,
    Skipped,
    IoError,
}

/// #3558: single-flock read-modify-write for the watcher terminal-commit caller
/// (`commit_decisions::mark_watcher_terminal_delivery_committed`). Replaces the
/// old unlocked `load_inflight_state` → mutate → `save_inflight_state` (which
/// re-wrote a stale `last_offset`/`response_sent_offset`, racing a concurrent
/// owner advance). Holds the flock across reload → identity guard → patch →
/// `persist_under_lock`. The commit owns the watermark, so it writes
/// `last_offset`/`response_sent_offset` but `max`-serializes both against the
/// in-lock reload (forward writes are unchanged; only a backward commit is
/// clamped up to the disk value).
pub(in crate::services::discord) fn commit_watcher_terminal_delivery_locked(
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
    patch: WatcherTerminalCommitPatch,
) -> WatcherTerminalCommitOutcome {
    let Some(root) = inflight_runtime_root() else {
        return WatcherTerminalCommitOutcome::IoError;
    };
    commit_watcher_terminal_delivery_locked_in_root(
        &root,
        provider,
        channel_id,
        require_identity,
        require_tmux_session_name,
        patch,
    )
}

/// Root-explicit variant of [`commit_watcher_terminal_delivery_locked`] for unit
/// tests.
pub(super) fn commit_watcher_terminal_delivery_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
    patch: WatcherTerminalCommitPatch,
) -> WatcherTerminalCommitOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return WatcherTerminalCommitOutcome::IoError;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return WatcherTerminalCommitOutcome::Skipped;
    };
    if state.restart_mode.is_some() || state.rebind_origin {
        return WatcherTerminalCommitOutcome::Skipped;
    }
    // Preserve the existing strong identity guard (user_msg_id + started_at +
    // tmux_session + turn_start_offset) exactly — `matches_state` already
    // compares all four, and we additionally pin the caller-supplied session.
    if !require_identity.matches_state(&state)
        || state.tmux_session_name.as_deref() != Some(require_tmux_session_name)
    {
        return WatcherTerminalCommitOutcome::Skipped;
    }

    state.terminal_delivery_committed = true;
    // Max-serialize against the in-lock reload so a late commit never moves the
    // watermark backward (the TOCTOU the old unlocked load→save introduced):
    //  - `full_response`: keep whichever body is LONGER. A concurrent stream may
    //    have persisted a longer body than this (possibly stale) commit carries;
    //    adopting the longer one avoids truncating already-relayed content AND
    //    keeps `response_sent_offset` in-bounds.
    //  - `response_sent_offset`: the committed body length, never below disk.
    //  - `last_offset`: the larger of the commit arg and the disk watermark.
    if patch.full_response.len() >= state.full_response.len() {
        state.full_response = patch.full_response;
    }
    let committed_response_offset = state.full_response.len().max(state.response_sent_offset);
    state.response_sent_offset =
        normalize_response_sent_offset(&state.full_response, committed_response_offset);
    state.last_offset = patch.last_offset.max(state.last_offset);
    state.last_watcher_relayed_offset = patch.last_watcher_relayed_offset;
    state.last_watcher_relayed_generation_mtime_ns = patch.last_watcher_relayed_generation_mtime_ns;

    match persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:commit_watcher_terminal_delivery_locked_in_root",
    ) {
        Ok(()) => WatcherTerminalCommitOutcome::Committed,
        Err(_) => WatcherTerminalCommitOutcome::IoError,
    }
}

/// #3558 (codex review follow-up): the watcher-owned relay-success watermark a
/// single-flock RMW patches onto the persisted row. Unlike the terminal-commit
/// patch this does NOT carry `last_offset` / `response_sent_offset` /
/// `full_response` and does NOT set `terminal_delivery_committed` — those are
/// preserved verbatim from the in-lock disk reload. The two
/// session-bound-relay-success sites in `tmux_watcher.rs` only mean to advance
/// the relay watermark; the old unlocked `load_inflight_state` → mutate →
/// `save_inflight_state(&inflight)` re-wrote the whole stale row (including a
/// possibly-backward `last_offset`/`response_sent_offset`), reintroducing the
/// exact backward-write TOCTOU the #3558 fix closed elsewhere.
#[derive(Debug, Clone, Copy)]
pub(in crate::services::discord) struct WatcherRelayWatermarkPatch {
    pub last_watcher_relayed_offset: Option<u64>,
    pub last_watcher_relayed_generation_mtime_ns: Option<i64>,
}

/// #3558: outcome of [`persist_watcher_relay_watermark_locked_in_root`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherRelayWatermarkOutcome {
    Saved,
    Skipped,
    IoError,
}

/// #3558 (codex review follow-up): single-flock read-modify-write for the
/// watcher's session-bound-relay-success watermark. Replaces the old unlocked
/// `load_inflight_state` → mutate → `save_inflight_state` at
/// `tmux_watcher.rs` (the two terminal-relay-success sites). Holds the sidecar
/// flock across reload → identity guard → patch → [`persist_under_lock`], never
/// re-entering [`super::save_inflight_state`] (which would re-acquire the same
/// non-reentrant flock and self-deadlock). ONLY `last_watcher_relayed_*` is
/// patched; `last_offset` / `response_sent_offset` / `full_response` are
/// preserved verbatim from the in-lock reload so a concurrent owner-gated
/// `refresh_inflight_last_offset_*` advance can no longer be clobbered backward
/// by the stale unlocked snapshot these sites used to write back.
pub(in crate::services::discord) fn persist_watcher_relay_watermark_locked(
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
    patch: WatcherRelayWatermarkPatch,
) -> WatcherRelayWatermarkOutcome {
    let Some(root) = inflight_runtime_root() else {
        return WatcherRelayWatermarkOutcome::IoError;
    };
    persist_watcher_relay_watermark_locked_in_root(
        &root,
        provider,
        channel_id,
        require_identity,
        require_tmux_session_name,
        patch,
    )
}

/// Root-explicit variant of [`persist_watcher_relay_watermark_locked`] for unit
/// tests.
pub(super) fn persist_watcher_relay_watermark_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
    patch: WatcherRelayWatermarkPatch,
) -> WatcherRelayWatermarkOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return WatcherRelayWatermarkOutcome::IoError;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return WatcherRelayWatermarkOutcome::Skipped;
    };
    if state.restart_mode.is_some() || state.rebind_origin {
        return WatcherRelayWatermarkOutcome::Skipped;
    }
    // Same strong identity guard as the terminal-commit helper (user_msg_id +
    // started_at + tmux_session + turn_start_offset, plus the caller-supplied
    // session). Rejects a write onto a fresh row B that replaced the row this
    // relay was for — the late-frame race the old tmux-session-only load→save
    // let through.
    if !require_identity.matches_state(&state)
        || state.tmux_session_name.as_deref() != Some(require_tmux_session_name)
    {
        return WatcherRelayWatermarkOutcome::Skipped;
    }

    state.last_watcher_relayed_offset = patch.last_watcher_relayed_offset;
    state.last_watcher_relayed_generation_mtime_ns = patch.last_watcher_relayed_generation_mtime_ns;
    // `last_offset` / `response_sent_offset` / `full_response` /
    // `terminal_delivery_committed` intentionally untouched — preserved from the
    // in-lock reload.

    match persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:persist_watcher_relay_watermark_locked_in_root",
    ) {
        Ok(()) => WatcherRelayWatermarkOutcome::Saved,
        Err(_) => WatcherRelayWatermarkOutcome::IoError,
    }
}
