//! Inflight store-side CAS "clear" + abandon cluster (#3835 extraction).
//!
//! The compare-and-set clear / abandon half of the inflight sidecar contract:
//! unconditional clears, identity-guarded clears (incl. rebind-origin and
//! after-delivery variants), zero-owned clears, the abandon-request enqueue
//! path, and the `response_sent_offset` refresh + normalization helper. Moved
//! verbatim out of `inflight.rs`; the parent re-exports every public symbol at
//! its original visibility so `inflight::*` flat paths stay byte-identical.
//! `normalize_response_sent_offset` is re-imported (non-test) by the parent
//! because the `watcher_state` sibling consumes it in production. The `_in_root`
//! explicit-root seams keep `pub(super)` for the parent's test re-imports.
//! Behaviour-preserving: no function body is altered.

use super::*;

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

pub(in crate::services::discord) fn clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_updated_at: &str,
    expected_save_generation: u64,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_updated_at,
        expected_save_generation,
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
pub(in crate::services::discord) fn clear_inflight_state_if_matches_in_root(
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
pub(in crate::services::discord) fn clear_inflight_state_if_matches_zero_owned_in_root(
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
pub(super) fn row_has_finalizable_placeholder(state: &InflightTurnState) -> bool {
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

pub(super) fn clear_inflight_state_if_matches_identity_in_root(
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
                "inflight identity-guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

pub(super) fn clear_rebind_origin_inflight_state_if_matches_identity_in_root(
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

pub(super) fn clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_updated_at: &str,
    expected_save_generation: u64,
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
    if state.restart_mode.is_none() && !state.rebind_origin {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    if !expected.matches_state(&state) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    if state.updated_at != expected_updated_at {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    if state.save_generation != expected_save_generation {
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
                "lifecycle inflight guarded-clear after death evidence remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

pub(super) fn normalize_response_sent_offset(
    full_response: &str,
    response_sent_offset: usize,
) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

pub(super) fn clear_inflight_state_if_matches_identity_after_delivery_in_root(
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
    bump_save_generation_for_write(&path, &mut delivered_state);

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
pub(super) fn clear_inflight_state_if_matches_tmux_response_in_root(
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
pub(super) fn refresh_inflight_last_offset_if_matches_identity_in_root(
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
    bump_save_generation_for_write(&path, &mut state);
    serde_json::to_string_pretty(&state)
        .map_err(|error| error.to_string())
        .and_then(|json| atomic_write(&path, &json))
        .is_ok()
}
