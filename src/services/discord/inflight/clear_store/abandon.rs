use super::super::{
    InflightTurnState, inflight_runtime_root, inflight_state_path, lock_inflight_state_path,
    log_inflight_remove, parse_inflight_state_content,
};
use super::GuardedClearOutcome;
use crate::services::discord::abandon_request_store;
use crate::services::provider::ProviderKind;
use std::fs;

/// #3859: true when the row anchors a finalizable "🔄 처리 중" placeholder — a real
/// placeholder message id (not 0, and not the placeholderless shape where the
/// anchor mirrors the user's own message id), that is still a PURE placeholder
/// (no streamed assistant content) or an explicitly-active long-running card.
/// Mirrors the placeholder sweeper's abandon-eligibility gate so partial-output
/// failure rows keep their delivered text (no "중단됨" clobber) exactly as the
/// pre-#3859 path did.
pub(in crate::services::discord::inflight) fn row_has_finalizable_placeholder(
    state: &InflightTurnState,
) -> bool {
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
    match abandon_request_store::enqueue(
        provider,
        token_hash,
        channel_id,
        abandon_request_store::AbandonRecord {
            msg_id: state.current_msg_id,
            started_at: state.started_at.clone(),
            current_tool_line: state.current_tool_line.clone(),
            terminal_status: abandon_request_store::TerminalCardStatus::Aborted,
            episode: abandon_request_store::AbandonEpisodeIdentity {
                user_msg_id: state.user_msg_id,
                started_at: state.started_at.clone(),
                status_panel_generation: state.status_panel_generation,
                save_generation: state.save_generation,
            },
        },
    ) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
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
pub(in crate::services::discord::inflight) fn request_inflight_abandon_if_matches_in_root(
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
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "request_inflight_abandon_if_matches",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
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
pub(in crate::services::discord::inflight) fn request_inflight_abandon_if_matches_zero_owned_in_root(
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
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "request_inflight_abandon_if_matches_zero_owned",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                error = %error,
                "inflight zero-owned abandon-request remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}
