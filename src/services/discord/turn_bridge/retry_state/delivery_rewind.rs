//! Delivery rewind persistence and local frontier reconciliation.

use super::*;
use crate::services::discord::inflight::StreamRelayAuthority;

fn normalized_response_sent_offset(full_response: &str, response_sent_offset: usize) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

pub(in crate::services::discord::turn_bridge) fn sync_response_delivery_state(
    full_response: &str,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    *response_sent_offset = normalized_response_sent_offset(full_response, *response_sent_offset);
    inflight_state.full_response = full_response.to_string();
    inflight_state.response_sent_offset = *response_sent_offset;
}

pub(in crate::services::discord::turn_bridge) fn sync_terminal_error_delivery_state(
    full_response: &str,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    *response_sent_offset = 0;
    sync_response_delivery_state(full_response, response_sent_offset, inflight_state);
}

pub(in crate::services::discord::turn_bridge) fn bridge_confirmed_response_sent_offset_seed(
    owner: crate::services::discord::inflight::RelayOwnerKind,
    response_sent_offset: usize,
) -> usize {
    match owner {
        crate::services::discord::inflight::RelayOwnerKind::None => response_sent_offset,
        crate::services::discord::inflight::RelayOwnerKind::Watcher
        | crate::services::discord::inflight::RelayOwnerKind::StandbyRelay
        | crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay
        | crate::services::discord::inflight::RelayOwnerKind::Unknown => 0,
    }
}

pub(in crate::services::discord::turn_bridge) fn bridge_should_reclaim_relay_from_missing_watcher(
    watcher_owns_assistant_relay: bool,
    standby_relay_owns_output: bool,
    live_watcher_registered: bool,
) -> bool {
    watcher_owns_assistant_relay && !standby_relay_owns_output && !live_watcher_registered
}

#[cfg(test)]
pub(super) fn refresh_delivery_rewind_state(inflight_state: &mut InflightTurnState) -> bool {
    let captured_nonce = inflight_state.turn_nonce.clone();
    refresh_owned_delivery_rewind_state(inflight_state, captured_nonce.as_deref())
}

/// Adopts the reloaded row only while it is still the episode `captured_nonce` names, so a
/// successor's row never replaces the local or receives this turn's settlement authority.
fn refresh_owned_delivery_rewind_state(
    inflight_state: &mut InflightTurnState,
    captured_nonce: Option<&str>,
) -> bool {
    let Some(provider) = inflight_state.provider_kind() else {
        return false;
    };
    let expected_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(inflight_state);
    let Some(mut reloaded) = crate::services::discord::inflight::load_inflight_state(
        &provider,
        inflight_state.channel_id,
    ) else {
        return false;
    };
    if !expected_identity.matches_state(&reloaded) || !reloaded.owned_by_turn_nonce(captured_nonce)
    {
        return false;
    }
    reloaded.preserve_intake_outbox_identity_from(inflight_state);
    *inflight_state = reloaded;
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryRewindPersistOutcome {
    Saved,
    Rejected,
    PersistError,
}

impl DeliveryRewindPersistOutcome {
    fn saved(self) -> bool {
        matches!(self, Self::Saved)
    }
}

fn persist_delivery_rewind(
    inflight_state: &mut InflightTurnState,
    captured_nonce: Option<&str>,
    expected_owner: StreamRelayAuthority,
    reason: crate::services::discord::inflight::InflightDeliveryRewindReason,
    channel_id: ChannelId,
) -> DeliveryRewindPersistOutcome {
    match crate::services::discord::inflight::save_inflight_delivery_rewind_if_matches_identity(
        inflight_state,
        captured_nonce,
        expected_owner,
        reason,
    ) {
        Ok(saved) => {
            if saved {
                refresh_owned_delivery_rewind_state(inflight_state, captured_nonce);
                DeliveryRewindPersistOutcome::Saved
            } else {
                DeliveryRewindPersistOutcome::Rejected
            }
        }
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                reason = reason.as_str(),
                error = %error,
                "turn_bridge failed to persist legitimate delivery rewind; preserving local rewind state"
            );
            DeliveryRewindPersistOutcome::PersistError
        }
    }
}

#[cfg(test)]
pub(super) fn persist_terminal_error_delivery_rewind(
    inflight_state: &mut InflightTurnState,
    expected_owner: StreamRelayAuthority,
    channel_id: ChannelId,
) -> DeliveryRewindPersistOutcome {
    let captured_nonce = inflight_state.turn_nonce.clone();
    persist_delivery_rewind(
        inflight_state,
        captured_nonce.as_deref(),
        expected_owner,
        crate::services::discord::inflight::InflightDeliveryRewindReason::TerminalErrorReset,
        channel_id,
    )
}

#[cfg(test)]
pub(super) fn persist_reclaim_delivery_rewind(
    inflight_state: &mut InflightTurnState,
    expected_owner: StreamRelayAuthority,
    channel_id: ChannelId,
) -> DeliveryRewindPersistOutcome {
    let captured_nonce = inflight_state.turn_nonce.clone();
    persist_delivery_rewind(
        inflight_state,
        captured_nonce.as_deref(),
        expected_owner,
        crate::services::discord::inflight::InflightDeliveryRewindReason::MissingWatcherReclaim,
        channel_id,
    )
}

pub(in crate::services::discord::turn_bridge) fn sync_terminal_error_delivery_state_for_bridge_owner(
    full_response: &str,
    response_sent_offset: &mut usize,
    bridge_confirmed_response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
    watcher_relay_owns_output: bool,
) -> bool {
    let expected_owner = StreamRelayAuthority::from_state(inflight_state);
    // One nonce, read before the local is touched, gates the write and both reloads.
    let captured_nonce = inflight_state.turn_nonce.clone();
    if watcher_relay_owns_output {
        inflight_state
            .set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::None);
    }
    sync_terminal_error_delivery_state(full_response, response_sent_offset, inflight_state);
    let persisted = persist_delivery_rewind(
        inflight_state,
        captured_nonce.as_deref(),
        expected_owner,
        crate::services::discord::inflight::InflightDeliveryRewindReason::TerminalErrorReset,
        channel_id,
    );
    if matches!(persisted, DeliveryRewindPersistOutcome::Rejected) {
        refresh_owned_delivery_rewind_state(inflight_state, captured_nonce.as_deref());
    }
    *response_sent_offset = inflight_state.response_sent_offset;
    *bridge_confirmed_response_sent_offset = *response_sent_offset;
    persisted.saved()
}

pub(in crate::services::discord::turn_bridge) fn rewind_delivery_on_reclaim(
    full_response: &str,
    bridge_confirmed_response_sent_offset: usize,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
) -> bool {
    if *response_sent_offset <= bridge_confirmed_response_sent_offset {
        return false;
    }
    *response_sent_offset = bridge_confirmed_response_sent_offset;
    sync_response_delivery_state(full_response, response_sent_offset, inflight_state);
    tracing::warn!(
        channel_id = channel_id.get(),
        response_sent_offset,
        "turn_bridge rewound response_sent_offset after reclaiming missing watcher"
    );
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) enum ReclaimDeliveryOutcome {
    NoRewindNeeded,
    Saved,
    Rejected,
    PersistError,
}

pub(in crate::services::discord::turn_bridge) fn rewind_and_persist_delivery_on_reclaim(
    full_response: &str,
    bridge_confirmed_response_sent_offset: usize,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
) -> ReclaimDeliveryOutcome {
    let pre_reclaim_state = inflight_state.clone();
    let pre_reclaim_response_sent_offset = *response_sent_offset;
    let expected_owner = StreamRelayAuthority::from_state(&pre_reclaim_state);
    let captured_nonce = pre_reclaim_state.turn_nonce.as_deref();
    inflight_state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::None);
    if !rewind_delivery_on_reclaim(
        full_response,
        bridge_confirmed_response_sent_offset,
        response_sent_offset,
        inflight_state,
        channel_id,
    ) {
        return ReclaimDeliveryOutcome::NoRewindNeeded;
    }
    match persist_delivery_rewind(
        inflight_state,
        captured_nonce,
        expected_owner,
        crate::services::discord::inflight::InflightDeliveryRewindReason::MissingWatcherReclaim,
        channel_id,
    ) {
        DeliveryRewindPersistOutcome::Saved => {
            *response_sent_offset = inflight_state.response_sent_offset;
            ReclaimDeliveryOutcome::Saved
        }
        DeliveryRewindPersistOutcome::Rejected => {
            if refresh_owned_delivery_rewind_state(inflight_state, captured_nonce) {
                *response_sent_offset = inflight_state.response_sent_offset;
            } else {
                *inflight_state = pre_reclaim_state;
                *response_sent_offset = pre_reclaim_response_sent_offset;
            }
            ReclaimDeliveryOutcome::Rejected
        }
        DeliveryRewindPersistOutcome::PersistError => ReclaimDeliveryOutcome::PersistError,
    }
}
