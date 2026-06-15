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
    if let super::placeholder_cleanup::PlaceholderCleanupOutcome::Failed { class, detail } =
        &outcome
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher orphan spinner cleanup failed ({}) for channel {} msg {}: {}",
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
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
}
