//! Runtime-root adapters for inflight rebind adoption CAS operations.

use super::*;

/// Identity-path adoption that hands back the committed row, so a later rollback
/// can pin exactly that commit.
pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_committed(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: Option<u64>,
) -> Result<InflightTurnState, GuardedSaveOutcome> {
    let Some(root) = inflight_runtime_root() else {
        return Err(GuardedSaveOutcome::IoError);
    };
    identity_gate::lock_and_save_existing_inflight_rebind_adoption_impl_in_root(
        &root,
        state,
        expected,
        None,
        expected_turn_start_offset,
        expected_last_offset,
        None,
    )
    .map(|(_lock, committed)| committed)
}

/// Single rollback entry for both adoption paths: restores `state` only while the
/// durable row is still `committed` (plus its own readoption marker).
pub(in crate::services::discord) fn restore_inflight_rebind_adoption_if_pinned(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_episode: Option<&InflightEpisodePin>,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset_for_rebase: Option<u64>,
    committed: &InflightTurnState,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    identity_gate::save_existing_inflight_rebind_adoption_impl_in_root(
        &root,
        state,
        expected,
        expected_episode,
        expected_turn_start_offset,
        expected_last_offset_for_rebase,
        Some(committed),
    )
}

/// A rollback may only undo the adoption it committed; the one write the same
/// rebind makes in between is its readoption marker (one generation step).
pub(super) fn rollback_pin_holds(
    committed: &InflightTurnState,
    on_disk: &InflightTurnState,
) -> bool {
    if on_disk.save_generation == committed.save_generation {
        return true;
    }
    if on_disk.save_generation != committed.save_generation.saturating_add(1)
        || (committed.readopted_from_inflight && committed.restart_mode.is_none())
    {
        return false;
    }
    let mut marked = committed.clone();
    marked.readopted_from_inflight = true;
    marked.clear_restart_mode();
    marked.updated_at.clone_from(&on_disk.updated_at);
    marked.save_generation = on_disk.save_generation;
    // A row that cannot be serialized was not shown equal: refuse.
    match (serde_json::to_value(&marked), serde_json::to_value(on_disk)) {
        (Ok(expected), Ok(actual)) => expected == actual,
        _ => false,
    }
}

#[cfg(test)]
#[path = "rollback_pin_tests.rs"]
mod rollback_pin_tests;
