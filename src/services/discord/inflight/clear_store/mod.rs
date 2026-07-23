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

mod abandon;
mod identity;

pub(crate) use self::abandon::{
    request_inflight_abandon_if_matches, request_inflight_abandon_if_matches_zero_owned,
};
#[cfg(test)]
pub(super) use self::abandon::{
    request_inflight_abandon_if_matches_in_root,
    request_inflight_abandon_if_matches_zero_owned_in_root, row_has_finalizable_placeholder,
};
pub(super) use self::identity::{
    clear_inflight_state_if_matches_identity_in_root,
    clear_inflight_state_if_matches_identity_returning_row_in_root,
    clear_inflight_state_if_matches_identity_turn_nonce_in_root, turn_nonce_matches,
};

use super::*;

pub(crate) fn clear_inflight_state(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    log_inflight_remove_for_path(provider, channel_id, "clear_inflight_state", &path);
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
    clear_inflight_state_if_matches_identity_returning_row(provider, channel_id, expected).0
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_returning_row(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    let Some(root) = inflight_runtime_root() else {
        return (GuardedClearOutcome::Missing, None);
    };
    clear_inflight_state_if_matches_identity_returning_row_in_root(
        &root, provider, channel_id, expected,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_turn_nonce(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_identity_turn_nonce_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_generation(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_finalizer_turn_id: u64,
    expected_updated_at: &str,
    expected_save_generation: u64,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_identity_generation_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_finalizer_turn_id,
        expected_updated_at,
        expected_save_generation,
    )
}

pub(in crate::services::discord) fn clear_rebind_origin_inflight_state_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_rebind_origin_inflight_state_if_matches_identity_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
    )
}

pub(in crate::services::discord) fn archive_inflight_state_if_matches_identity_generation(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_updated_at: &str,
    expected_save_generation: u64,
    reason: &str,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    archive_inflight_state_if_matches_identity_generation_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_updated_at,
        expected_save_generation,
        reason,
    )
}

pub(super) fn archive_inflight_state_if_matches_identity_generation_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_updated_at: &str,
    expected_save_generation: u64,
    reason: &str,
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
    if !expected.matches_state(&state)
        || state.updated_at != expected_updated_at
        || state.save_generation != expected_save_generation
    {
        return GuardedClearOutcome::UserMsgMismatch;
    }

    let archive_dir = root.join("archive");
    if fs::create_dir_all(&archive_dir).is_err() {
        return GuardedClearOutcome::IoError;
    }
    let safe_reason: String = reason
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S%3f");
    let archive_path = archive_dir.join(format!("{channel_id}.json.{safe_reason}-{timestamp}"));
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "archive_inflight_state_if_matches_identity_generation",
        &path,
    );
    match fs::rename(&path, &archive_path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                archive_path = %archive_path.display(),
                error = %error,
                "inflight identity-guarded archive failed"
            );
            GuardedClearOutcome::IoError
        }
    }
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
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_inflight_state_if_matches",
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
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_inflight_state_if_matches_zero_owned",
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
                "inflight zero-owned guarded-clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

pub(super) fn clear_inflight_state_if_matches_identity_generation_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_finalizer_turn_id: u64,
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
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if expected_finalizer_turn_id == 0
        || !state.matches_finalizer_turn_id(expected_finalizer_turn_id)
        || !expected.matches_state(&state)
        || state.updated_at != expected_updated_at
        || state.save_generation != expected_save_generation
    {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_inflight_state_if_matches_identity_generation",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                expected_user_msg_id = expected.user_msg_id,
                expected_finalizer_turn_id,
                error = %error,
                "inflight generation-guarded clear remove_file failed; treating as IoError so sweeper retries"
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
    expected_turn_nonce: Option<&str>,
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
    if !expected.matches_state(&state) || !turn_nonce_matches(expected_turn_nonce, &state) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_rebind_origin_inflight_state_if_matches_identity",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
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
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
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

#[allow(clippy::too_many_arguments)]
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
                channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight delivery mirror failed before identity-guarded clear"
            );
            false
        }
    };

    log_inflight_remove(
        provider,
        channel_id,
        delivered_state.user_msg_id,
        "clear_inflight_state_if_matches_identity_after_delivery",
        &path,
    );
    let outcome = match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
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

    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_inflight_state_if_matches_tmux_response",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
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
