//! Settle a persisted successful quiet result before recovery can offer its raw body.
use super::*;
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::discord::routine_presentation::TerminalPresentation;
use crate::services::discord::turn_bridge::{
    BridgeLeaseAcquire, bridge_delivery_lease_for_inflight,
};

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
    // A live actor, an unknown format, or a planned restart retains its existing authority.
    if owner.cancel_token.is_some()
        || saved.rebind_origin
        || saved.restart_mode.is_some()
        || saved.runtime_kind_unknown_on_disk
        || saved.version > inflight::inflight_state_version()
    {
        return true;
    }
    let mut row = saved.clone();
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
    let lease = match bridge_delivery_lease_for_inflight(
        shared,
        lease_owner,
        shared.restart.current_generation,
        &row,
        Some(row.last_offset),
    ) {
        BridgeLeaseAcquire::Held(lease) => Some(lease),
        BridgeLeaseAcquire::NoRange => None,
        BridgeLeaseAcquire::Skip => return true,
    };
    let Some(committed) = shared
        .mailbox(channel)
        .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
            shared: shared.clone(),
            state: row,
            actor: None,
            delivery: RecoveryRelayOutcome::Delivered.into(),
        })
        .await
    else {
        return true;
    };
    row = committed.state;
    if let Some(lease) = lease
        && !lease.commit_and_advance(
            shared,
            lease_owner,
            row.tmux_session_name.as_deref(),
            crate::services::discord::LeaseOutcome::Delivered,
        )
    {
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
