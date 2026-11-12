//! Settle a persisted successful quiet result before recovery can offer its raw body.
use super::*;
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::discord::routine_presentation::TerminalPresentation;
use crate::services::discord::turn_bridge::NoVisibleResultLease;

pub(in crate::services::discord::recovery_engine) async fn settle_routine_no_visible(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    saved: &inflight::InflightTurnState,
) -> bool {
    if *provider != ProviderKind::Claude
        || saved.terminal_receipt.is_some()
        || saved.tui_terminal_kind == Some(NativeTerminalKind::Aborted)
        || saved.routine_presentation.terminal_presentation()
            != TerminalPresentation::NoVisibleResult
    {
        return false;
    }
    let Some(channel) = inflight::opt_channel_id(saved.channel_id) else {
        return true;
    };
    let owner = super::super::mailbox_snapshot(shared, channel).await;
    if crate::services::provider::cancel_requested(owner.cancel_token.as_deref()) {
        return false;
    }
    // Live actors and unknown formats retain their existing authority.
    if owner.cancel_token.is_some()
        || saved.rebind_origin
        || saved.runtime_kind_unknown_on_disk
        || saved.version > inflight::inflight_state_version()
    {
        return true;
    }
    let mut row = saved.clone();
    if row.restart_mode.is_some() {
        let Some(adopted) = shared
            .mailbox(channel)
            .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
                shared: shared.clone(),
                state: row,
                actor: None,
                quiet_restart_generation: Some(shared.restart.current_generation),
                delivery: RecoveryRelayOutcome::Delivered.into(),
            })
            .await
        else {
            return true;
        };
        row = adopted.state;
    }
    if !super::replay_hold::hydrate_replay_hold(
        shared.pg_pool.as_ref(),
        &mut row,
        shared.restart.replay_restart.get(),
    )
    .await
    {
        return true;
    }
    let current = inflight::load_inflight_state_read_only(provider, row.channel_id);
    if current.as_ref().is_none_or(|current| {
        !inflight::InflightEpisodePin::from_state(&row).matches_state(current)
            || current.save_generation != row.save_generation
    }) {
        return true;
    }
    let Some(lease_owner) = inflight::opt_channel_id(row.delivery_record_owner_channel_id()) else {
        return true;
    };
    let Some(lease) =
        NoVisibleResultLease::acquire(shared, lease_owner, &row, Some(row.last_offset))
    else {
        return true;
    };
    let Some(committed) = shared
        .mailbox(channel)
        .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
            shared: shared.clone(),
            state: row,
            actor: None,
            quiet_restart_generation: None,
            delivery: RecoveryRelayOutcome::Delivered.into(),
        })
        .await
    else {
        return true;
    };
    row = committed.state;
    if !lease.commit_and_advance(shared, lease_owner, row.tmux_session_name.as_deref()) {
        return true;
    }
    // A replay hold consumes only the body debt; its request episode is not ours to release.
    if row.replay_rerun_blocked()
        || !crate::services::discord::receipt_settle::record_or_stamp(
            shared.pg_pool.as_ref(),
            &row,
            crate::services::discord::receipt_settle::body_outcome(&row.full_response),
        )
        .await
    {
        return true;
    }
    let snapshot = crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(&row);
    super::super::idle_captured_response::retire_captured_ready_response(
        shared,
        provider,
        &row,
        snapshot,
        |snapshot| {
            super::super::idle_captured_response::finish_recovered_turn_mailbox_for_captured_state(
                shared, provider, &row, snapshot,
            )
        },
    )
    .await;
    true
}

/// Called by the serialized mailbox actor; the row lock rechecks the saved success and epoch.
pub(in crate::services::discord::recovery_engine) fn adopt_quiet_drain_restart(
    expected: &inflight::InflightTurnState,
    generation: u64,
    actor: Option<&Arc<CancelToken>>,
) -> Option<inflight::InflightTurnState> {
    let provider = expected.provider_kind()?;
    let mut locked = inflight::lock_inflight_episode(
        &provider,
        expected.channel_id,
        &inflight::InflightEpisodePin::from_state(expected),
    )
    .ok()?;
    let current = locked.state();
    let marker_generation = current.restart_generation?;
    if actor.is_some()
        || provider != ProviderKind::Claude
        || current.save_generation != expected.save_generation
        || current.restart_mode != Some(crate::services::discord::InflightRestartMode::DrainRestart)
        || current.restart_mode != expected.restart_mode
        || current.restart_generation != expected.restart_generation
        || marker_generation == 0
        || marker_generation >= generation
        || current.born_generation == 0
        || current.born_generation >= generation
        || current.user_msg_id == 0
        || current.request_owner_user_id == 0
        || current.rebind_origin
        || current.runtime_kind_unknown_on_disk
        || current.version > inflight::inflight_state_version()
        || current.terminal_receipt.is_some()
        || current.tui_terminal_kind == Some(NativeTerminalKind::Aborted)
        || current.routine_presentation.terminal_presentation()
            != TerminalPresentation::NoVisibleResult
    {
        return None;
    }
    let mut adopted = current.clone();
    adopted.clear_restart_mode();
    matches!(
        locked.persist_verified_under_guard(&adopted, "routine_quiet_drain_adoption"),
        inflight::GuardedSaveOutcome::Saved
    )
    .then(|| locked.state().clone())
}
