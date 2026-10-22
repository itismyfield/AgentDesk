//! Snapshot CAS clear of one exact captured row, reporting why a kept row stayed.

use super::*;

/// Why a snapshot clear removed or kept the row, with the row version it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct SnapshotClearDetail {
    pub(in crate::services::discord) reason: &'static str,
    pub(in crate::services::discord) observed_save_generation: Option<u64>,
}

impl GuardedClearOutcome {
    /// `clear_inflight_state_for_snapshot` with the lock-held reason behind its outcome.
    pub(in crate::services::discord) fn for_snapshot_with_detail(
        provider: &ProviderKind,
        snapshot: &InflightTurnState,
    ) -> (Self, SnapshotClearDetail) {
        let Some(root) = inflight_runtime_root() else {
            return (Self::Missing, detail("no_runtime_root", None));
        };
        clear_if_matches_generation_and_episode_in_root(
            &root,
            provider,
            snapshot.channel_id,
            &InflightTurnIdentity::from_state(snapshot),
            snapshot.effective_finalizer_turn_id(),
            (&snapshot.updated_at, snapshot.save_generation),
            Some(&super::super::InflightEpisodePin::from_state(snapshot)),
        )
    }
}

fn detail(reason: &'static str, observed_save_generation: Option<u64>) -> SnapshotClearDetail {
    SnapshotClearDetail {
        reason,
        observed_save_generation,
    }
}

/// Reads the row; a present but unreadable row is logged and reported as `row_read_failed`.
pub(super) fn read_row(
    provider: &ProviderKind,
    channel_id: u64,
    path: &std::path::Path,
) -> Result<String, &'static str> {
    fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            return "not_found";
        }
        warn_unreadable_row(provider, channel_id, "row_read_failed", &error);
        "row_read_failed"
    })
}

/// Parses the row; a malformed row is logged and reported as `row_parse_failed`.
pub(super) fn parse_row(
    provider: &ProviderKind,
    channel_id: u64,
    data: &str,
) -> Result<InflightTurnState, &'static str> {
    serde_json::from_str(data).map_err(|error| {
        warn_unreadable_row(provider, channel_id, "row_parse_failed", &error);
        "row_parse_failed"
    })
}

/// Guarded clears keep reporting such a row as `Missing`; this names why it was not removed.
fn warn_unreadable_row(
    provider: &ProviderKind,
    channel_id: u64,
    reason: &'static str,
    error: &dyn std::fmt::Display,
) {
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id,
        reason,
        error = %error,
        "inflight guarded clear could not read the row; reporting it as missing"
    );
}

pub(super) fn clear_if_matches_generation_and_episode_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_finalizer_turn_id: u64,
    expected_generation: (&str, u64),
    expected_episode: Option<&super::super::InflightEpisodePin>,
) -> (GuardedClearOutcome, SnapshotClearDetail) {
    let (expected_updated_at, expected_save_generation) = expected_generation;
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return (GuardedClearOutcome::IoError, detail("lock_failed", None));
    };
    let data = match read_row(provider, channel_id, &path) {
        Ok(data) => data,
        Err(reason) => return (GuardedClearOutcome::Missing, detail(reason, None)),
    };
    if super::super::raw_before_start_receipt(&data) && !super::super::receipt_mutant("clear") {
        return (GuardedClearOutcome::IoError, detail("start_receipt", None));
    }
    let state = match parse_row(provider, channel_id, &data) {
        Ok(state) => state,
        Err(reason) => return (GuardedClearOutcome::Missing, detail(reason, None)),
    };
    let observed = Some(state.save_generation);
    if state.restart_mode.is_some() {
        return (
            GuardedClearOutcome::PlannedRestartSkipped,
            detail("planned_restart", observed),
        );
    }
    if state.rebind_origin {
        return (
            GuardedClearOutcome::RebindOriginSkipped,
            detail("rebind_origin", observed),
        );
    }
    let mismatch = if expected_episode.is_some_and(|episode| !episode.matches_state(&state)) {
        Some("episode_pin_changed")
    } else if expected_finalizer_turn_id == 0
        || !state.matches_finalizer_turn_id(expected_finalizer_turn_id)
        || !expected.matches_state(&state)
    {
        Some("identity_changed")
    } else if state.updated_at != expected_updated_at
        || state.save_generation != expected_save_generation
    {
        Some("snapshot_version_changed")
    } else {
        None
    };
    if let Some(reason) = mismatch {
        return (
            GuardedClearOutcome::UserMsgMismatch,
            detail(reason, observed),
        );
    }
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_inflight_state_if_matches_identity_generation",
        &path,
    );
    match super::remove_row(&path) {
        Ok(GuardedClearOutcome::Cleared) => {
            (GuardedClearOutcome::Cleared, detail("cleared", observed))
        }
        Ok(outcome) => (outcome, detail("not_found", observed)),
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                expected_user_msg_id = expected.user_msg_id,
                expected_finalizer_turn_id,
                error = %error,
                "inflight generation-guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            (
                GuardedClearOutcome::IoError,
                detail("remove_failed", observed),
            )
        }
    }
}
