//! Cancel only the process episode named by a routine's durable Discord turn id.

use poise::serenity_prelude::ChannelId;

use super::HealthRegistry;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight;
use crate::services::provider::ProviderKind;
use crate::services::provider::cancel_token_cleanup::executor::{
    CleanupRequest, ExpectedBinding, TmuxCleanupIntent,
};

/// Leaves settlement to the existing bridge, which owns the captured episode.
/// A routine's old tmux label is never resolved or used as a cancellation target.
pub(crate) async fn cancel_routine_process_turn(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
    turn_id: &str,
    reason: &str,
) -> bool {
    if reason.trim().is_empty() {
        return false;
    }
    let Some(shared) = registry
        .shared_for_provider_on_channel(provider, channel)
        .await
    else {
        return false;
    };
    let Some(mailbox) = shared.mailbox_peek(channel) else {
        return false;
    };
    let snapshot = mailbox.snapshot().await;
    let Some(token) = snapshot.cancel_token else {
        return false;
    };
    let Some(nonce) = token.turn_nonce().filter(|nonce| !nonce.is_empty()) else {
        return false;
    };
    let Some(row) = inflight::load_inflight_state_read_only(provider, channel.get()) else {
        return false;
    };
    // ProcessReady stores a logical process-session name in tmux_session_name.
    // Only an explicit ProcessBackend kind makes that name safe to disregard.
    let process_row = match row.runtime_kind {
        Some(RuntimeHandoffKind::ProcessBackend) => true,
        None => row.tmux_session_name.is_none(),
        _ => false,
    };
    if !process_row
        || row.runtime_kind_unknown_on_disk
        || row.input_fifo_path.is_some()
        || row.rebind_origin
        || row.restart_mode.is_some()
        || token.restart_mode().is_some()
        || token.is_completion_cleanup()
        || token.tmux_session_name().is_some()
        || row.user_msg_id == 0
        || row.provider != provider.as_str()
        || row.channel_id != channel.get()
        || turn_id != format!("discord:{}:{}", channel.get(), row.user_msg_id)
        || snapshot.active_user_message_id.map(|id| id.get()) != Some(row.user_msg_id)
        || snapshot.active_turn_nonce.as_deref() != Some(nonce)
        || row.turn_nonce.as_deref() != Some(nonce)
    {
        return false;
    }
    // The mailbox compares the captured token, so a successor admitted while
    // reading the durable row cannot receive this cancellation.
    let cancelled = mailbox
        .cancel_active_turn_if_current_with_reason(token.clone(), reason.to_string())
        .await;
    if cancelled.token.is_none() {
        return false;
    }
    let cleanup = token.request_cleanup_expecting(
        CleanupRequest {
            cancel_source: reason.to_string(),
            intent: TmuxCleanupIntent::PidOnly,
            termination_reason: None,
            hard_stop_target: None,
        },
        ExpectedBinding::Decided(None),
    );
    tracing::info!(
        channel_id = channel.get(),
        provider = provider.as_str(),
        turn_id,
        reason,
        ?cleanup,
        "routine requested cancellation of its exact process turn"
    );
    !cleanup.host_refused
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
    use crate::services::discord::{SharedData, make_shared_data_for_tests_with_storage};
    use crate::services::provider::CancelToken;
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    use poise::serenity_prelude::{MessageId, UserId};
    use std::sync::{Arc, atomic::Ordering};

    const CHANNEL: u64 = 61_018_201;
    const USER_MESSAGE: u64 = 61_018_202;

    async fn fixture() -> (
        HealthRegistry,
        Arc<SharedData>,
        Arc<CancelToken>,
        inflight::InflightTurnState,
    ) {
        let registry = HealthRegistry::new();
        let shared = make_shared_data_for_tests_with_storage(None);
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(ChannelId::new(CHANNEL))
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(USER_MESSAGE))
            .await;
        registry.register("codex".into(), shared.clone()).await;
        let mut row = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            CHANNEL,
            None,
            7,
            USER_MESSAGE,
            USER_MESSAGE + 1,
            "routine".into(),
            None,
            None,
            None,
            None,
            0,
        );
        row.turn_nonce = token.turn_nonce().map(str::to_string);
        inflight::save_inflight_state(&row).unwrap();
        (registry, shared, token, row)
    }

    async fn cancel(registry: &HealthRegistry) -> bool {
        cancel_routine_process_turn(
            registry,
            &ProviderKind::Codex,
            ChannelId::new(CHANNEL),
            &format!("discord:{CHANNEL}:{USER_MESSAGE}"),
            "routine_timeout",
        )
        .await
    }

    #[tokio::test]
    async fn exact_initial_process_turn_is_cancelled_without_clearing_its_lease() {
        with_isolated_runtime_root(|| async {
            let (registry, shared, token, _) = fixture().await;
            let mailbox = shared.mailbox(ChannelId::new(CHANNEL));
            for id in [USER_MESSAGE + 2, USER_MESSAGE + 3] {
                let queued = Intervention {
                    author_id: UserId::new(7),
                    author_is_bot: false,
                    message_id: MessageId::new(id),
                    queued_generation: shared.restart.current_generation,
                    source_message_ids: vec![MessageId::new(id)],
                    source_message_queued_generations: vec![],
                    source_text_segments: vec![],
                    text: format!("queued {id}"),
                    mode: InterventionMode::Soft,
                    created_at: std::time::Instant::now(),
                    reply_context: None,
                    has_reply_boundary: false,
                    merge_consecutive: false,
                    pending_uploads: vec![],
                    voice_announcement: None,
                };
                assert!(
                    mailbox
                        .enqueue(
                            queued,
                            crate::services::discord::queue_persistence_context(
                                &shared,
                                &ProviderKind::Codex,
                                ChannelId::new(CHANNEL),
                            ),
                        )
                        .await
                        .enqueued
                );
            }
            assert!(cancel(&registry).await);
            assert!(token.cancelled.load(Ordering::Acquire));
            assert_eq!(token.cancel_source().as_deref(), Some("routine_timeout"));
            let snapshot = mailbox.snapshot().await;
            assert!(Arc::ptr_eq(&snapshot.cancel_token.unwrap(), &token,));
            assert_eq!(
                snapshot
                    .intervention_queue
                    .iter()
                    .map(|queued| (queued.message_id.get(), queued.text.clone()))
                    .collect::<Vec<_>>(),
                vec![
                    (USER_MESSAGE + 2, format!("queued {}", USER_MESSAGE + 2)),
                    (USER_MESSAGE + 3, format!("queued {}", USER_MESSAGE + 3)),
                ],
                "cancelling one episode preserves the queued successor messages",
            );
            assert!(
                inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL).is_some()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn exact_process_handoff_accepts_its_logical_session_name() {
        with_isolated_runtime_root(|| async {
            let (registry, _, token, mut row) = fixture().await;
            row.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
            row.tmux_session_name = Some("routine-logical-process-session".into());
            inflight::save_inflight_state(&row).unwrap();
            assert!(cancel(&registry).await);
            assert!(token.cancelled.load(Ordering::Acquire));
        })
        .await;
    }

    #[tokio::test]
    async fn stale_turn_id_or_nonce_never_cancels_the_current_process() {
        with_isolated_runtime_root(|| async {
            let (registry, shared, token, mut row) = fixture().await;
            assert!(
                !cancel_routine_process_turn(
                    &registry,
                    &ProviderKind::Codex,
                    ChannelId::new(CHANNEL),
                    &format!("discord:{CHANNEL}:{}", USER_MESSAGE - 1),
                    "routine_timeout",
                )
                .await
            );
            row.turn_nonce = Some("older-routine-episode".into());
            inflight::save_inflight_state(&row).unwrap();
            assert!(!cancel(&registry).await);
            assert!(!token.cancelled.load(Ordering::Acquire));
            let newer = Arc::new(CancelToken::new());
            shared
                .mailbox(ChannelId::new(CHANNEL))
                .restore_active_turn(
                    newer.clone(),
                    UserId::new(7),
                    MessageId::new(USER_MESSAGE + 9),
                )
                .await;
            assert!(!cancel(&registry).await);
            assert!(!newer.cancelled.load(Ordering::Acquire));
        })
        .await;
    }

    #[tokio::test]
    async fn tmux_rows_and_actual_token_bindings_are_never_cancelled() {
        with_isolated_runtime_root(|| async {
            let (registry, _, token, mut row) = fixture().await;
            row.tmux_session_name = Some("old-routine-tmux".into());
            inflight::save_inflight_state(&row).unwrap();
            assert!(!cancel(&registry).await);
            for kind in [
                RuntimeHandoffKind::CodexTui,
                RuntimeHandoffKind::ClaudeTui,
                RuntimeHandoffKind::LegacyTmuxWrapper,
            ] {
                row.runtime_kind = Some(kind);
                inflight::save_inflight_state(&row).unwrap();
                assert!(!cancel(&registry).await);
            }
            row.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
            inflight::save_inflight_state(&row).unwrap();
            token.bind_unmanaged_session_name("old-routine-tmux");
            assert!(!cancel(&registry).await);
            assert!(!token.cancelled.load(Ordering::Acquire));
        })
        .await;
    }

    #[cfg(unix)]
    #[test]
    fn exact_routine_cancellation_terminates_its_captured_child_process() {
        use crate::services::provider::cancel_token_cleanup::executor::with_real_pid_kill;
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        with_real_pid_kill(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(with_isolated_runtime_root(|| async {
                    let (registry, _, token, _) = fixture().await;
                    let mut child = ChildGuard(
                        Command::new("/bin/sleep")
                            .arg("30")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .process_group(0)
                            .spawn()
                            .unwrap(),
                    );
                    token.store_child_pid(child.0.id());
                    assert!(token.captured_child_process().unwrap().identity.is_some());
                    assert!(cancel(&registry).await);
                    tokio::time::timeout(std::time::Duration::from_secs(3), async {
                        while child.0.try_wait().unwrap().is_none() {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .expect("the exact captured child must exit after cancellation");
                }));
        });
    }
}
