use super::*;
use crate::services::discord::inflight::store::persist_under_lock_with_snapshot;

/// #3041 P1-2 (codex P1-2 R3): identity-guarded re-save for the bridge's
/// delivery-lease `Skip` epilogue. On a Skip the live HOLDER (watcher) owns the
/// turn and CLEARS the row on success, so the bridge epilogue must NOT blindly
/// `save_inflight_state`: if the holder's clear won the race, a blind re-save
/// would resurrect a STALE row for an already-delivered turn (recovery then sees
/// it delivered, never clears, leaks the row). This closes the window the same
/// way `clear_inflight_state_if_matches` (#2427 D-wire) does: under the lock,
/// write only when the row is STILL present AND its `(user_msg_id, started_at,
/// tmux_session_name)` identity (+ `turn_start_offset` when known) matches. Gone
/// (`Missing`) or replaced by a newer turn / restart-rebind marker
/// (`IdentityMismatch`) → no-op; holder FAILED + didn't clear → still present &
/// matching → refresh (`Saved`). Same flock + atomic_write primitives as the
/// rest of the module (Windows-safe).
pub(in crate::services::discord) fn save_inflight_state_if_matches_identity<
    T: GuardedStampTarget,
>(
    state: T,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

/// Root-explicit inner form of [`save_inflight_state_if_matches_identity`] for
/// unit tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(in crate::services::discord::inflight) fn save_inflight_state_if_matches_identity_in_root<
    T: GuardedStampTarget,
>(
    root: &Path,
    target: T,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let state = InflightTurnState::clone(target.local_state());
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    // Hold the sidecar flock across the read AND the write so a concurrent
    // holder `clear_inflight_state` (which takes the same lock) cannot land its
    // remove between our identity check and our write.
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    // Holder already cleared the row on its success path → do NOT resurrect.
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        // Malformed row: treat like a mismatch and do not clobber — the loader
        // eviction path GCs malformed payloads on the next read.
        return GuardedSaveOutcome::IdentityMismatch;
    };
    // A newer turn (different identity) or a planned-restart / rebind-origin
    // marker now owns the row — never overwrite it with this preserved turn.
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    // Completion preservation is a durable-first merge, never a stale row
    // replay. A generation match allows a deliberate non-prefix response
    // rewrite (for example API_FRICTION stripping). Once another same-turn
    // writer advances the generation, only prefix-compatible forward progress
    // is merged; anchor/runtime/session/owner/tool evidence stays durable.
    let generation_matches = on_disk.save_generation == state.save_generation;
    let mut updated = on_disk;
    if generation_matches {
        updated.full_response.clone_from(&state.full_response);
        updated.response_sent_offset =
            normalize_response_sent_offset(&updated.full_response, state.response_sent_offset);
    } else if let Some((full_response, response_sent_offset)) = merge_forward_response_progress(
        (&updated.full_response, updated.response_sent_offset),
        (&state.full_response, state.response_sent_offset),
    ) {
        updated.full_response = full_response;
        updated.response_sent_offset = response_sent_offset;
    } else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            snapshot_save_generation = state.save_generation,
            durable_save_generation = updated.save_generation,
            "completion inflight preserve kept a concurrently advanced divergent durable response"
        );
    }
    updated.last_offset = updated.last_offset.max(state.last_offset);
    updated.terminal_delivery_committed |= state.terminal_delivery_committed;
    for frozen_id in &state.streaming_rollover_frozen_msg_ids {
        if !updated
            .streaming_rollover_frozen_msg_ids
            .contains(frozen_id)
        {
            updated.streaming_rollover_frozen_msg_ids.push(*frozen_id);
        }
    }
    match persist_under_lock_with_snapshot(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_matches_identity_in_root",
    ) {
        Ok(Some(persisted)) => {
            target.adopt_persisted(persisted);
            GuardedSaveOutcome::Saved
        }
        Ok(None) => GuardedSaveOutcome::IdentityMismatch,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}
