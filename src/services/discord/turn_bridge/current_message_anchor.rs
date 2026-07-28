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
/// 0 -> real bind. Any authority or current-message epoch advance during the
/// send aborts this bridge lease; every unbound candidate is deleted or queued
/// for orphan cleanup.
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
    let expected_current_message = (
        inflight_state.current_msg_id,
        inflight_state.current_msg_len,
    );
    if expected_current_message.0 != 0 {
        return false;
    }
    let expected_relay_authority =
        crate::services::discord::inflight::StreamRelayAuthority::from_state(inflight_state);
    if !expected_relay_authority.bridge_owns_relay() {
        return false;
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
        expected_current_message.0,
        Some(expected_current_message.1),
        candidate.get(),
        anchor_text.len(),
        Some(expected_relay_authority),
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
            Some(expected_current_message),
            Some(expected_relay_authority),
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
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::GatewayFuture;
    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    enum AuthorityMarker {
        Restart,
        Rebind,
        Watcher,
        Standby,
        CompetingBridgeAnchor,
    }

    struct MarkerDuringSendGateway {
        provider: ProviderKind,
        channel_id: u64,
        marker: AuthorityMarker,
        candidate: MessageId,
        deleted: Mutex<Vec<u64>>,
    }

    impl TurnGateway for MarkerDuringSendGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async move {
                let mut durable = crate::services::discord::inflight::load_inflight_state(
                    &self.provider,
                    self.channel_id,
                )
                .expect("durable row during send");
                match self.marker {
                    AuthorityMarker::Restart => durable.set_restart_mode(
                        crate::services::discord::InflightRestartMode::DrainRestart,
                    ),
                    AuthorityMarker::Rebind => durable.rebind_origin = true,
                    AuthorityMarker::Watcher => {
                        durable.set_watcher_owner_channel_id(self.channel_id + 100);
                        durable.watcher_owns_live_relay = true;
                        durable.set_relay_owner_kind(
                            crate::services::discord::inflight::RelayOwnerKind::Watcher,
                        );
                    }
                    AuthorityMarker::Standby => {
                        durable.set_watcher_owner_channel_id(self.channel_id + 100);
                        durable.watcher_owns_live_relay = false;
                        durable.set_relay_owner_kind(
                            crate::services::discord::inflight::RelayOwnerKind::StandbyRelay,
                        );
                    }
                    AuthorityMarker::CompetingBridgeAnchor => {
                        durable.current_msg_id = self.candidate.get() + 1_000;
                        durable.current_msg_len = 17;
                    }
                }
                crate::services::discord::inflight::save_inflight_state(&durable)
                    .expect("install authority marker during send");
                Ok(self.candidate)
            })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            message_id: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            self.deleted
                .lock()
                .expect("deleted lock")
                .push(message_id.get());
            Box::pin(async { Ok(()) })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            false
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(self.provider.clone())
        }
    }

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

    #[tokio::test]
    async fn authority_or_message_epoch_advance_during_send_aborts_and_deletes_candidate() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );

        for (index, marker) in [
            AuthorityMarker::Restart,
            AuthorityMarker::Rebind,
            AuthorityMarker::Watcher,
            AuthorityMarker::Standby,
            AuthorityMarker::CompetingBridgeAnchor,
        ]
        .into_iter()
        .enumerate()
        {
            let provider = ProviderKind::Codex;
            let channel_id = 42_592_710 + index as u64;
            let mut local = InflightTurnState::new(
                provider.clone(),
                channel_id,
                Some("adk-anchor-r7".to_string()),
                343_742_347_365_974_026,
                77_010 + index as u64,
                18,
                "anchor race".to_string(),
                Some("session".to_string()),
                Some(format!("AgentDesk-codex-anchor-{index}")),
                Some(format!("/runtime/anchor-{index}.jsonl")),
                None,
                512,
            );
            local.current_msg_id = 0;
            local.current_msg_len = 0;
            crate::services::discord::inflight::save_inflight_state(&local)
                .expect("seed absent anchor");
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&local);
            let candidate = MessageId::new(990_000 + index as u64);
            let gateway = MarkerDuringSendGateway {
                provider: provider.clone(),
                channel_id,
                marker,
                candidate,
                deleted: Mutex::new(Vec::new()),
            };
            let mut detached = detached_current_msg_id_from_durable(0);
            let mut bridge_candidate = None;

            assert!(
                !ensure_bridge_current_message_anchor(
                    &gateway,
                    &provider,
                    "anchor-r7",
                    ChannelId::new(channel_id),
                    &expected,
                    &mut detached,
                    &mut bridge_candidate,
                    &mut local,
                    "⏳",
                )
                .await
            );
            assert_eq!(
                gateway.deleted.lock().expect("deleted lock").as_slice(),
                &[candidate.get()]
            );
            assert_eq!(durable_current_msg_id_from_detached(detached), 0);
            assert_eq!(bridge_candidate, None);
            let durable =
                crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
                    .expect("preserved marked row");
            let expected_durable_message =
                if matches!(marker, AuthorityMarker::CompetingBridgeAnchor) {
                    candidate.get() + 1_000
                } else {
                    0
                };
            assert_eq!(durable.current_msg_id, expected_durable_message);
            match marker {
                AuthorityMarker::Restart => assert!(durable.restart_mode.is_some()),
                AuthorityMarker::Rebind => assert!(durable.rebind_origin),
                AuthorityMarker::Watcher => assert_eq!(
                    durable.effective_relay_owner_kind(),
                    crate::services::discord::inflight::RelayOwnerKind::Watcher,
                ),
                AuthorityMarker::Standby => assert_eq!(
                    durable.effective_relay_owner_kind(),
                    crate::services::discord::inflight::RelayOwnerKind::StandbyRelay,
                ),
                AuthorityMarker::CompetingBridgeAnchor => {
                    assert_eq!(durable.current_msg_len, 17)
                }
            }
        }
    }
}
