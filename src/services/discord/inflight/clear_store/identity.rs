use super::*;

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    clear_inflight_state_if_matches_identity_turn_nonce_in_root(
        root, provider, channel_id, expected, None,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_returning_row_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return (GuardedClearOutcome::IoError, None);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return (GuardedClearOutcome::Missing, None);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return (GuardedClearOutcome::Missing, None);
    };
    let outcome = guarded_identity_clear_outcome(&state, expected, None);
    if outcome != GuardedClearOutcome::Cleared {
        return (outcome, None);
    }
    remove_identity_matched_state(&path, provider, channel_id, expected, state)
}

fn guarded_identity_clear_outcome(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if !expected.matches_state(state) || !turn_nonce_matches(expected_turn_nonce, state) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    GuardedClearOutcome::Cleared
}

fn remove_identity_matched_state(
    path: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    state: InflightTurnState,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    log_inflight_remove(
        provider,
        channel_id,
        state.user_msg_id,
        "clear_inflight_state_if_matches_identity",
        path,
    );
    match fs::remove_file(path) {
        Ok(()) => (GuardedClearOutcome::Cleared, Some(state)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (GuardedClearOutcome::Missing, None)
        }
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            (GuardedClearOutcome::IoError, None)
        }
    }
}

pub(in crate::services::discord) fn turn_nonce_matches(
    expected_turn_nonce: Option<&str>,
    state: &InflightTurnState,
) -> bool {
    match (
        expected_turn_nonce.filter(|value| !value.is_empty()),
        state
            .turn_nonce
            .as_deref()
            .filter(|value| !value.is_empty()),
    ) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => true,
    }
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_turn_nonce_in_root(
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
    let outcome = guarded_identity_clear_outcome(&state, expected, expected_turn_nonce);
    if outcome != GuardedClearOutcome::Cleared {
        return outcome;
    }
    remove_identity_matched_state(&path, provider, channel_id, expected, state).0
}
