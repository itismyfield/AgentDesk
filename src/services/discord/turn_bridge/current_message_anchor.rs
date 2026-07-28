//! Explicit absent/current response-anchor state and guarded materialization.

use super::*;

/// Detached bridge loops require a non-zero serenity id even when the durable
/// owner explicitly cleared its placeholder. This synthetic id is an in-memory
/// `None`; it must never cross a durable or observability boundary.
pub(super) fn detached_current_msg_id_from_durable(durable_id: u64) -> MessageId {
    crate::services::discord::inflight::optional_message_id(durable_id).unwrap_or_else(|| {
        MessageId::new(headless_delivery::SYNTHETIC_HEADLESS_RECOVERY_PLACEHOLDER_ID)
    })
}

pub(super) fn durable_current_msg_id_from_detached(detached_id: MessageId) -> u64 {
    if detached_id.get() == headless_delivery::SYNTHETIC_HEADLESS_RECOVERY_PLACEHOLDER_ID {
        0
    } else {
        detached_id.get()
    }
}

pub(super) fn optional_durable_current_msg_id_from_detached(detached_id: MessageId) -> Option<u64> {
    let durable_id = durable_current_msg_id_from_detached(detached_id);
    (durable_id != 0).then_some(durable_id)
}

pub(super) fn unbound_current_message_candidate(
    current_msg_id: MessageId,
    expected_durable_id: u64,
) -> Option<MessageId> {
    (!is_synthetic_headless_message_id(current_msg_id)
        && current_msg_id.get() != expected_durable_id)
        .then_some(current_msg_id)
}

pub(super) async fn cleanup_unbound_bridge_anchor<G: TurnGateway + ?Sized>(
    gateway: &G,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    message_id: MessageId,
) {
    if gateway
        .delete_message(channel_id, message_id)
        .await
        .is_err()
    {
        crate::services::discord::status_panel_orphan_store::enqueue(
            provider,
            token_hash,
            channel_id.get(),
            message_id.get(),
        );
    }
}

/// Sends an absent response anchor, then adopts it only after a guarded durable
/// 0 -> real bind. A competing same-turn bind is re-read and adopted; every
/// unbound candidate is deleted or queued for orphan cleanup.
pub(super) async fn ensure_bridge_current_message_anchor<G: TurnGateway + ?Sized>(
    gateway: &G,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    expected_identity: &crate::services::discord::inflight::InflightTurnIdentity,
    current_msg_id: &mut MessageId,
    bridge_created_response_placeholder_msg_id: &mut Option<MessageId>,
    inflight_state: &mut InflightTurnState,
    anchor_text: &str,
) -> bool {
    if durable_current_msg_id_from_detached(*current_msg_id) != 0 {
        return true;
    }

    if let Some(stale_candidate) = bridge_created_response_placeholder_msg_id.take() {
        cleanup_unbound_bridge_anchor(gateway, provider, token_hash, channel_id, stale_candidate)
            .await;
    }
    let candidate = match TurnGateway::send_message(gateway, channel_id, anchor_text).await {
        Ok(candidate) => candidate,
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                error = %error,
                "bridge could not recreate a cleared response placeholder"
            );
            return false;
        }
    };
    let expected_turn_start_offset = inflight_state.turn_start_offset;
    let bind = crate::services::discord::inflight::bind_recovery_anchor_if_matches_identity(
        provider,
        channel_id.get(),
        expected_identity,
        expected_turn_start_offset,
        0,
        candidate.get(),
        anchor_text.len(),
        Some(inflight_state),
    );
    if bind == crate::services::discord::inflight::GuardedSaveOutcome::Saved {
        *current_msg_id = candidate;
        *bridge_created_response_placeholder_msg_id = Some(candidate);
        inflight_state.current_msg_id = candidate.get();
        inflight_state.current_msg_len = anchor_text.len();
        return true;
    }

    cleanup_unbound_bridge_anchor(gateway, provider, token_hash, channel_id, candidate).await;
    let Some((bound_id, bound_len)) =
        crate::services::discord::inflight::recovery_anchor_message_if_matches_identity(
            provider,
            channel_id.get(),
            expected_identity,
            expected_turn_start_offset,
            Some(inflight_state),
        )
    else {
        return false;
    };
    *current_msg_id = MessageId::new(bound_id);
    inflight_state.current_msg_id = bound_id;
    inflight_state.current_msg_len = bound_len;
    true
}

pub(super) async fn edit_bound_current_message<G: TurnGateway + ?Sized>(
    gateway: &G,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    inflight_state: &mut InflightTurnState,
    content: &str,
) -> bool {
    let durable_id = durable_current_msg_id_from_detached(current_msg_id);
    if durable_id == 0 {
        return false;
    }
    let _ = TurnGateway::edit_message(gateway, channel_id, current_msg_id, content).await;
    inflight_state.current_msg_id = durable_id;
    inflight_state.current_msg_len = content.len();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleared_durable_current_message_stays_absent_until_real_anchor_exists() {
        let detached = detached_current_msg_id_from_durable(0);
        assert_eq!(
            detached.get(),
            headless_delivery::SYNTHETIC_HEADLESS_RECOVERY_PLACEHOLDER_ID
        );
        assert_eq!(durable_current_msg_id_from_detached(detached), 0);
        assert_eq!(
            optional_durable_current_msg_id_from_detached(detached),
            None
        );

        let real = MessageId::new(900_002);
        assert_eq!(durable_current_msg_id_from_detached(real), real.get());
        assert_eq!(
            optional_durable_current_msg_id_from_detached(real),
            Some(real.get())
        );
        assert_eq!(unbound_current_message_candidate(detached, 0), None);
        assert_eq!(unbound_current_message_candidate(real, real.get()), None);
        assert_eq!(unbound_current_message_candidate(real, 900_001), Some(real));
    }
}
