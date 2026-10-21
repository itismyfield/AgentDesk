//! Episode-identity predicates for writers that read or rewrite another persisted row.

use super::*;

impl InflightTurnState {
    /// Exact `Option` equality with the nonce a turn captured when it started; `None` matches
    /// only `None`, so a legacy actor never adopts a successor's nonced row or the reverse.
    pub(in crate::services::discord) fn owned_by_turn_nonce(&self, captured: Option<&str>) -> bool {
        self.turn_nonce.as_deref() == captured
    }

    /// The one writer that may move a persisted episode to a newly admitted actor's nonce. `self`
    /// is the row read before the claim; the durable row must still equal it in pin and version.
    pub(in crate::services::discord) fn save_admitted_nonce_transition(
        &self,
        refreshed: &InflightTurnState,
        caller: &'static str,
    ) -> GuardedSaveOutcome {
        use crate::services::discord::inflight::InflightEpisodePin;
        let (Some(root), Some(provider)) = (inflight_runtime_root(), self.provider_kind()) else {
            return GuardedSaveOutcome::IoError;
        };
        let mut birth = refreshed.clone();
        birth.turn_nonce.clone_from(&self.turn_nonce);
        let pin = InflightEpisodePin::from_state(self);
        if refreshed.turn_nonce.is_none()
            || refreshed.turn_nonce == self.turn_nonce
            || !pin.is_same_episode_as(&InflightEpisodePin::from_state(&birth))
        {
            return GuardedSaveOutcome::AuthorityPinned;
        }
        let path = inflight_state_path(&root, &provider, self.channel_id);
        let Ok(_lock) = lock_inflight_state_path(&path) else {
            return GuardedSaveOutcome::IoError;
        };
        let expected = InflightTurnIdentity::from_state(self);
        let on_disk = match read_inflight_state_for_guarded_write(
            &path,
            &provider,
            self.channel_id,
            &expected,
            caller,
        ) {
            Ok(on_disk) => on_disk,
            Err(outcome) => return outcome,
        };
        if !pin.matches_state(&on_disk) {
            return GuardedSaveOutcome::SuccessorOwned;
        }
        if on_disk.restart_mode.is_some() || on_disk.rebind_origin {
            return GuardedSaveOutcome::from_durable_authority(&on_disk);
        }
        if on_disk.save_generation != self.save_generation
            || on_disk.updated_at != self.updated_at
            || !hosted_binding_write_admitted(&on_disk, refreshed)
        {
            return GuardedSaveOutcome::AuthorityPinned;
        }
        match crate::services::discord::inflight::store::persist_under_lock_with_snapshot(
            &root, &path, refreshed, caller,
        ) {
            Ok(Some(_)) => GuardedSaveOutcome::Saved,
            Ok(None) => GuardedSaveOutcome::AuthorityPinned,
            Err(_) => GuardedSaveOutcome::IoError,
        }
    }
}

/// Reloads under the caller's lock and refuses a successor episode's row before any field of it
/// is merged, copied or written back.
pub(super) fn read_owned_row_for_guarded_write(
    path: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    captured_nonce: Option<&str>,
    caller: &'static str,
) -> Result<InflightTurnState, GuardedSaveOutcome> {
    let row = read_inflight_state_for_guarded_write(path, provider, channel_id, expected, caller)?;
    if !row.owned_by_turn_nonce(captured_nonce) {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id,
            caller,
            snapshot_identity = ?expected,
            "guarded inflight write skipped because another episode's nonce owns the durable row"
        );
        return Err(GuardedSaveOutcome::SuccessorOwned);
    }
    Ok(row)
}

pub(super) fn identity_matches_with_offset_guard(
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    captured_nonce: Option<&str>,
    state: &InflightTurnState,
) -> bool {
    if !expected.matches_state(state) || !state.owned_by_turn_nonce(captured_nonce) {
        return false;
    }
    // Anchor bind/reuse reads or rewrites another persisted row. For synthetic
    // id-0 rows, fail closed unless BOTH sides carry the birth offset and it
    // matches. This is stricter than the delivery-lease id-0 degenerate fallback,
    // which is transport-level dedup only and never authorizes row mutation.
    if expected.user_msg_id == 0 {
        return matches!(
            (expected_turn_start_offset, state.turn_start_offset),
            (Some(expected_offset), Some(actual_offset)) if expected_offset == actual_offset
        );
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        state.turn_start_offset == Some(expected_offset)
    } else {
        true
    }
}
