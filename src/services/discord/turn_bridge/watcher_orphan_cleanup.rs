//! Watcher-orphan spinner-card cleanup decision + retry-spawn helpers for the
//! turn bridge.
//!
//! #3479 Phase-1 rank-4 extraction: byte-identical helpers the bridge consults
//! when a reused tmux watcher leaves the bridge-created response placeholder
//! orphaned — the delete-eligibility predicate, the lifecycle-failure retry
//! predicate, the cleanup-outcome recorder, and the bounded retry-spawn. The
//! retry-spawn takes every dependency by value (no ambient `shared`/`http`
//! capture) and routes through `super::task_supervisor` /
//! `super::placeholder_cleanup`, so it moves as a free fn. Moved verbatim from
//! `turn_bridge/mod.rs` and re-exported there so call sites stay identical.

use super::*;

pub(in crate::services::discord) fn should_delete_bridge_created_watcher_orphan_response(
    status_panel_v2_enabled: bool,
    watcher_handoff_claim_outcome: WatcherHandoffClaimOutcome,
    bridge_created_response_placeholder_msg_id: Option<MessageId>,
    current_msg_id: MessageId,
) -> bool {
    status_panel_v2_enabled
        && watcher_handoff_claim_outcome == WatcherHandoffClaimOutcome::ReusedExisting
        && bridge_created_response_placeholder_msg_id == Some(current_msg_id)
}

pub(in crate::services::discord) fn should_retry_watcher_orphan_spinner_cleanup(
    outcome: &super::placeholder_cleanup::PlaceholderCleanupOutcome,
) -> bool {
    matches!(
        outcome,
        super::placeholder_cleanup::PlaceholderCleanupOutcome::Failed {
            class: super::placeholder_cleanup::PlaceholderCleanupFailureClass::LifecycleFailure,
            ..
        }
    )
}

pub(in crate::services::discord) fn record_watcher_orphan_spinner_cleanup(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    outcome: super::placeholder_cleanup::PlaceholderCleanupOutcome,
    source: &'static str,
) {
    use super::placeholder_cleanup::PlaceholderCleanupOutcome;
    let (emit_outcome, emit_detail) = match &outcome {
        PlaceholderCleanupOutcome::Succeeded => ("committed", None),
        PlaceholderCleanupOutcome::AlreadyGone => ("already_gone", None),
        PlaceholderCleanupOutcome::Failed { class, detail } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher orphan spinner cleanup failed ({}) for channel {} msg {}: {}",
                class.as_str(),
                channel_id.get(),
                message_id.get(),
                detail
            );
            ("failed", Some(detail.clone()))
        }
    };
    // #3607: durable delete observability for the orphan-spinner cleanup. The
    // skip path is emitted by `orphan_spinner_cleanup_guarded_skip` before the
    // delete; here we record the actual delete result.
    crate::services::observability::emit_relay_delete(
        provider.as_str(),
        channel_id.get(),
        message_id.get(),
        None,
        None,
        source,
        super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteNonterminal.as_str(),
        emit_outcome,
        emit_detail.as_deref(),
    );
    shared
        .ui
        .placeholder_cleanup
        .record(super::placeholder_cleanup::PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            operation: super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteNonterminal,
            outcome,
            source,
        });
}

/// #3607: full orphan-spinner cleanup, lifted out of the `turn_bridge/mod.rs`
/// hot file. When a reused watcher leaves the bridge-created response
/// placeholder orphaned the bridge calls this once.
///
/// First the terminal-anchor guard: if `current_msg_id` is a committed terminal
/// anchor (the accident #3607 fixes — a finished turn's retired message that a
/// generic janitor is about to wrongly delete) the delete is SKIPPED and a
/// `skipped_committed_terminal` delete event is emitted. Otherwise this is a
/// genuine non-terminal orphan: delete it, record the cleanup outcome (which
/// also emits the durable `committed | already_gone | failed` delete event via
/// [`record_watcher_orphan_spinner_cleanup`]), and on a retryable lifecycle
/// failure spawn the bounded retry.
///
/// Behaviour for the non-guarded path is byte-identical to the prior inline
/// block; only the guard + observability are new.
pub(in crate::services::discord) async fn cleanup_or_preserve_watcher_orphan_spinner(
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    gateway: Arc<dyn TurnGateway>,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    inflight_state: &InflightTurnState,
) {
    if super::placeholder_cleanup::committed_terminal_anchor_protects_delete(
        &shared.ui.placeholder_cleanup,
        provider,
        channel_id,
        current_msg_id,
        Some(inflight_state),
    ) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🛡 #3607 bridge preserved orphan watcher-handoff spinner anchor — committed terminal cleanup owns msg {} (channel {})",
            current_msg_id.get(),
            channel_id.get()
        );
        crate::services::observability::emit_relay_delete(
            provider.as_str(),
            channel_id.get(),
            current_msg_id.get(),
            None,
            None,
            "turn_bridge_watcher_orphan_spinner_cleanup",
            super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteNonterminal.as_str(),
            "skipped_committed_terminal",
            None,
        );
        return;
    }

    let cleanup_outcome = match gateway.delete_message(channel_id, current_msg_id).await {
        Ok(()) => super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
        Err(error) => super::placeholder_cleanup::classify_delete_error(&error),
    };
    let cleanup_committed = cleanup_outcome.is_committed();
    let cleanup_should_retry = should_retry_watcher_orphan_spinner_cleanup(&cleanup_outcome);
    let cleanup_already_gone = matches!(
        cleanup_outcome,
        super::placeholder_cleanup::PlaceholderCleanupOutcome::AlreadyGone
    );
    record_watcher_orphan_spinner_cleanup(
        shared.as_ref(),
        provider,
        channel_id,
        current_msg_id,
        inflight_state.tmux_session_name.as_deref(),
        cleanup_outcome,
        "turn_bridge_watcher_orphan_spinner_cleanup",
    );
    let ts = chrono::Local::now().format("%H:%M:%S");
    if cleanup_committed {
        if cleanup_already_gone {
            tracing::info!(
                "  [{ts}] 🧹 bridge orphan watcher-handoff spinner card already gone (channel {}, msg {})",
                channel_id,
                current_msg_id
            );
        } else {
            tracing::info!(
                "  [{ts}] 🧹 bridge removed orphan watcher-handoff spinner card (channel {}, msg {})",
                channel_id,
                current_msg_id
            );
        }
    } else if cleanup_should_retry {
        spawn_watcher_orphan_spinner_cleanup_retry(
            shared.clone(),
            provider.clone(),
            gateway.clone(),
            channel_id,
            current_msg_id,
            inflight_state.tmux_session_name.clone(),
        );
    }
}

pub(in crate::services::discord) fn spawn_watcher_orphan_spinner_cleanup_retry(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    gateway: Arc<dyn TurnGateway>,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<String>,
) {
    const RETRY_DELAYS: &[std::time::Duration] = &[
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(15),
    ];

    super::task_supervisor::spawn_observed("watcher_orphan_spinner_cleanup_retry", async move {
        for (attempt, delay) in RETRY_DELAYS.iter().enumerate() {
            tokio::time::sleep(*delay).await;
            let outcome = match gateway.delete_message(channel_id, message_id).await {
                Ok(()) => super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
                Err(error) => super::placeholder_cleanup::classify_delete_error(&error),
            };
            let committed = outcome.is_committed();
            let should_retry = should_retry_watcher_orphan_spinner_cleanup(&outcome);
            record_watcher_orphan_spinner_cleanup(
                shared.as_ref(),
                &provider,
                channel_id,
                message_id,
                tmux_session_name.as_deref(),
                outcome,
                "turn_bridge_watcher_orphan_spinner_cleanup_retry",
            );
            if committed || !should_retry {
                return;
            }
            if attempt + 1 == RETRY_DELAYS.len() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ watcher orphan spinner cleanup exhausted retries for channel {} msg {}",
                    channel_id.get(),
                    message_id.get()
                );
            }
        }
    });
}

#[cfg(test)]
mod watcher_orphan_cleanup_tests {
    use super::*;

    #[test]
    fn watcher_reuse_deletes_only_bridge_created_response_placeholder() {
        let bridge_placeholder = MessageId::new(20);

        assert!(should_delete_bridge_created_watcher_orphan_response(
            true,
            WatcherHandoffClaimOutcome::ReusedExisting,
            Some(bridge_placeholder),
            bridge_placeholder,
        ));
        assert!(!should_delete_bridge_created_watcher_orphan_response(
            true,
            WatcherHandoffClaimOutcome::Spawned,
            Some(bridge_placeholder),
            bridge_placeholder,
        ));
        assert!(!should_delete_bridge_created_watcher_orphan_response(
            true,
            WatcherHandoffClaimOutcome::ReusedExisting,
            Some(bridge_placeholder),
            MessageId::new(10),
        ));
        assert!(!should_delete_bridge_created_watcher_orphan_response(
            false,
            WatcherHandoffClaimOutcome::ReusedExisting,
            Some(bridge_placeholder),
            bridge_placeholder,
        ));
        assert!(!should_delete_bridge_created_watcher_orphan_response(
            true,
            WatcherHandoffClaimOutcome::None,
            Some(bridge_placeholder),
            bridge_placeholder,
        ));
    }

    #[test]
    fn watcher_orphan_spinner_cleanup_retries_only_lifecycle_failures() {
        use super::super::placeholder_cleanup::{
            PlaceholderCleanupFailureClass, PlaceholderCleanupOutcome,
        };

        assert!(should_retry_watcher_orphan_spinner_cleanup(
            &PlaceholderCleanupOutcome::Failed {
                class: PlaceholderCleanupFailureClass::LifecycleFailure,
                detail: "HTTP 500".to_string(),
            }
        ));
        assert!(!should_retry_watcher_orphan_spinner_cleanup(
            &PlaceholderCleanupOutcome::Failed {
                class: PlaceholderCleanupFailureClass::PermissionOrRoutingDiagnostic,
                detail: "HTTP 403".to_string(),
            }
        ));
        assert!(!should_retry_watcher_orphan_spinner_cleanup(
            &PlaceholderCleanupOutcome::Succeeded
        ));
        assert!(!should_retry_watcher_orphan_spinner_cleanup(
            &PlaceholderCleanupOutcome::AlreadyGone
        ));
    }

    use super::super::gateway::GatewayFuture;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use std::sync::Mutex;

    #[derive(Default)]
    struct DeleteTrackingGateway {
        deleted: Arc<Mutex<Vec<MessageId>>>,
    }

    impl TurnGateway for DeleteTrackingGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Err("unused".to_string()) })
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
            let deleted = self.deleted.clone();
            Box::pin(async move {
                deleted.lock().expect("deleted lock").push(message_id);
                Ok(())
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Err("unused".to_string()) })
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
            _intervention: &'a super::super::Intervention,
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
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Codex)
        }
    }

    fn orphan_inflight(current_msg_id: u64, terminal_committed: bool) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            777,
            None,
            1,
            2,
            current_msg_id,
            "test".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.terminal_delivery_committed = terminal_committed;
        state
    }

    #[tokio::test]
    async fn committed_terminal_anchor_skips_orphan_spinner_delete() {
        // #3607: when the orphaned spinner card's message is a committed terminal
        // anchor (a finished turn's retired message), the bridge must NOT delete
        // it. The accident this fixes.
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(777);
        let anchor = MessageId::new(50_000);
        // The terminal cleanup already committed a DeleteTerminal tombstone, and
        // the inflight is no longer the live owner (terminal not committed on
        // this orphan row) — only the registry signal protects it.
        shared.ui.placeholder_cleanup.record(
            super::placeholder_cleanup::PlaceholderCleanupRecord {
                provider: provider.clone(),
                channel_id,
                message_id: anchor,
                tmux_session_name: None,
                operation: super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteTerminal,
                outcome: super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
                source: "test",
            },
        );
        let gateway = Arc::new(DeleteTrackingGateway::default());
        let inflight = orphan_inflight(anchor.get(), false);

        cleanup_or_preserve_watcher_orphan_spinner(
            shared.clone(),
            &provider,
            gateway.clone() as Arc<dyn TurnGateway>,
            channel_id,
            anchor,
            &inflight,
        )
        .await;

        assert!(
            gateway.deleted.lock().expect("deleted lock").is_empty(),
            "committed terminal anchor must be preserved (no delete)"
        );
    }

    #[tokio::test]
    async fn genuine_non_terminal_orphan_is_deleted() {
        // #3607: a real orphan spinner (no terminal-anchor tombstone, inflight
        // never committed terminal delivery) is still deleted as before.
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(777);
        let orphan = MessageId::new(60_000);
        let gateway = Arc::new(DeleteTrackingGateway::default());
        let inflight = orphan_inflight(orphan.get(), false);

        cleanup_or_preserve_watcher_orphan_spinner(
            shared.clone(),
            &provider,
            gateway.clone() as Arc<dyn TurnGateway>,
            channel_id,
            orphan,
            &inflight,
        )
        .await;

        assert_eq!(
            gateway.deleted.lock().expect("deleted lock").as_slice(),
            &[orphan],
            "genuine non-terminal orphan must be deleted"
        );
    }
}
