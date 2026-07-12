//! Final watcher-reattach mutation adapter.
//!
//! Keeping the exact episode pin beside the registry rebind call makes the
//! reservation-to-mutation authority handoff explicit without growing the
//! frozen `relay_recovery.rs` root.

use super::*;

pub(super) async fn apply_rebind(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    episode: Option<&circuit_breaker::RelayReattachEpisode>,
) -> RelayRecoveryApplyResult {
    match registry
        .rebind_inflight(
            provider,
            decision.channel_id,
            decision.affected.tmux_session.clone(),
            super::super::recovery_engine::ManualRebindOverrides::default(),
            episode.map(circuit_breaker::RelayReattachEpisode::pin),
        )
        .await
    {
        Some(Ok(outcome)) => RelayRecoveryApplyResult {
            status: reattach_apply_status(outcome.watcher_spawned),
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: Some(outcome.watcher_spawned),
            reattach_watcher_replaced: Some(outcome.watcher_replaced),
            reattach_initial_offset: Some(outcome.initial_offset),
            reattach_error: None,
        },
        Some(Err(error)) => RelayRecoveryApplyResult {
            status: if matches!(
                error,
                super::super::recovery_engine::RebindError::InflightEpisodeChanged
            ) {
                "reattach_episode_changed"
            } else {
                "rebind_failed"
            },
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: None,
            reattach_watcher_replaced: None,
            reattach_initial_offset: None,
            reattach_error: Some(error.to_string()),
        },
        None => RelayRecoveryApplyResult {
            status: "provider_unavailable",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: None,
            reattach_watcher_replaced: None,
            reattach_initial_offset: None,
            reattach_error: Some("provider unavailable".to_string()),
        },
    }
}
