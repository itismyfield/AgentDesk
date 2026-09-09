use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn adopt_coordinates(
    existing: &mut super::inflight::InflightTurnState,
    tmux_session_name: &str,
    output_path: &str,
    input_fifo_for_state: &Option<String>,
    existing_offset_rebase_to_output: Option<u64>,
    runtime_kind_for_state: Option<RuntimeHandoffKind>,
    session_id_for_state: &Option<String>,
    expected_episode: Option<&super::inflight::InflightEpisodePin>,
    locked_episode_from_adoption: &mut Option<super::inflight::LockedInflightEpisode>,
) -> (
    super::inflight::GuardedSaveOutcome,
    super::inflight::InflightTurnIdentity,
    Option<u64>,
    Option<u64>,
) {
    let expected = super::inflight::InflightTurnIdentity::from_state(existing);
    let expected_turn_start_offset = existing.turn_start_offset;
    let expected_last_offset_for_rebase = existing.last_offset;
    existing.tmux_session_name = Some(tmux_session_name.to_owned());
    existing.output_path = Some(output_path.to_owned());
    existing.input_fifo_path = input_fifo_for_state.clone();
    if let Some(rebased_last_offset) = existing_offset_rebase_to_output {
        existing.last_offset = rebased_last_offset;
        existing.turn_start_offset = Some(rebased_last_offset);
        existing.last_watcher_relayed_offset = None;
        existing.last_watcher_relayed_generation_mtime_ns = None;
    }
    if let Some(runtime_kind) = runtime_kind_for_state {
        existing.runtime_kind = Some(runtime_kind);
    }
    if session_id_for_state.is_some() {
        existing.session_id = session_id_for_state.clone();
    }
    existing.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
    let rollback_expected = super::inflight::InflightTurnIdentity::from_state(existing);
    let rollback_expected_turn_start_offset = existing.turn_start_offset;
    let rollback_expected_last_offset_for_rebase =
        existing_offset_rebase_to_output.map(|_| existing.last_offset);
    let save_outcome = if let Some(expected_episode) = expected_episode {
        let adoption_state = existing.clone();
        let expected_identity = expected.clone();
        let expected_episode = expected_episode.clone();
        let expected_last_offset =
            existing_offset_rebase_to_output.map(|_| expected_last_offset_for_rebase);
        let adoption = tokio::task::spawn_blocking(move || {
            super::inflight::adopt_and_lock_inflight_episode(
                &adoption_state,
                &expected_identity,
                &expected_episode,
                expected_turn_start_offset,
                expected_last_offset,
            )
        })
        .await
        .unwrap_or(Err(super::inflight::GuardedSaveOutcome::IoError));
        match adoption {
            Ok(guard) => {
                *existing = guard.state().clone();
                *locked_episode_from_adoption = Some(guard);
                super::inflight::GuardedSaveOutcome::Saved
            }
            Err(outcome) => outcome,
        }
    } else if existing_offset_rebase_to_output.is_some() {
        super::inflight::save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity(
            existing,
            &expected,
            expected_turn_start_offset,
            expected_last_offset_for_rebase,
        )
    } else {
        super::inflight::save_existing_inflight_rebind_adoption_if_matches_identity(
            existing,
            &expected,
            expected_turn_start_offset,
        )
    };
    (
        save_outcome,
        rollback_expected,
        rollback_expected_turn_start_offset,
        rollback_expected_last_offset_for_rebase,
    )
}
