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
    remove_identity_matched_state(
        &path,
        provider,
        channel_id,
        expected,
        state,
        "clear_inflight_state_if_matches_identity",
    )
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
    reason: &'static str,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    log_inflight_remove(provider, channel_id, state.user_msg_id, reason, path);
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

fn clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    reconcile_current_generation: Option<u64>,
) -> super::reconcile_gate::ReconcileClearOutcome {
    use super::reconcile_gate::ReconcileClearOutcome;

    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::IoError);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    if reconcile_current_generation
        .is_some_and(|current| super::reconcile_gate::row_is_current_generation(&state, current))
    {
        return ReconcileClearOutcome::LiveGenerationSkipped;
    }
    let outcome = guarded_identity_clear_outcome(&state, expected, expected_turn_nonce);
    if outcome != GuardedClearOutcome::Cleared {
        return ReconcileClearOutcome::Delegated(outcome);
    }
    ReconcileClearOutcome::Delegated(
        remove_identity_matched_state(
            &path,
            provider,
            channel_id,
            expected,
            state,
            "clear_inflight_state_if_matches_identity",
        )
        .0,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_turn_nonce_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    match clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        None,
    ) {
        super::reconcile_gate::ReconcileClearOutcome::Delegated(outcome) => outcome,
        super::reconcile_gate::ReconcileClearOutcome::LiveGenerationSkipped => {
            unreachable!("the ordinary identity clear never enables the reconcile gate")
        }
    }
}

pub(super) fn clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    current_generation: u64,
) -> super::reconcile_gate::ReconcileClearOutcome {
    clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        Some(current_generation),
    )
}

fn clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    reconcile_current_generation: Option<u64>,
) -> super::reconcile_gate::ReconcileClearOutcome {
    use super::reconcile_gate::ReconcileClearOutcome;

    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::IoError);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    if reconcile_current_generation
        .is_some_and(|current| super::reconcile_gate::row_is_current_generation(&state, current))
    {
        return ReconcileClearOutcome::LiveGenerationSkipped;
    }
    let outcome = if state.restart_mode.is_some() {
        GuardedClearOutcome::PlannedRestartSkipped
    } else if !state.rebind_origin
        || !expected.matches_state(&state)
        || !turn_nonce_matches(expected_turn_nonce, &state)
    {
        GuardedClearOutcome::UserMsgMismatch
    } else {
        GuardedClearOutcome::Cleared
    };
    if outcome != GuardedClearOutcome::Cleared {
        return ReconcileClearOutcome::Delegated(outcome);
    }
    ReconcileClearOutcome::Delegated(
        remove_identity_matched_state(
            &path,
            provider,
            channel_id,
            expected,
            state,
            "clear_rebind_origin_inflight_state_if_matches_identity",
        )
        .0,
    )
}

pub(in crate::services::discord) fn clear_rebind_origin_inflight_state_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    match clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        None,
    ) {
        super::reconcile_gate::ReconcileClearOutcome::Delegated(outcome) => outcome,
        super::reconcile_gate::ReconcileClearOutcome::LiveGenerationSkipped => {
            unreachable!("the ordinary rebind clear never enables the reconcile gate")
        }
    }
}

pub(super) fn clear_rebind_origin_inflight_state_if_matches_identity_for_reconcile_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    current_generation: u64,
) -> super::reconcile_gate::ReconcileClearOutcome {
    clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        Some(current_generation),
    )
}
