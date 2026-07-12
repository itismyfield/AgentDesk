//! Runtime-root adapters for inflight rebind adoption CAS operations.

use super::*;

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    identity_gate::save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_if_matches_episode(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_episode: &InflightEpisodePin,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    identity_gate::save_existing_inflight_rebind_adoption_impl_in_root(
        &root,
        state,
        expected,
        Some(expected_episode),
        expected_turn_start_offset,
        None,
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
    identity_gate::save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
        expected_last_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_episode(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_episode: &InflightEpisodePin,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: u64,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    identity_gate::save_existing_inflight_rebind_adoption_impl_in_root(
        &root,
        state,
        expected,
        Some(expected_episode),
        expected_turn_start_offset,
        Some(expected_last_offset),
    )
}
