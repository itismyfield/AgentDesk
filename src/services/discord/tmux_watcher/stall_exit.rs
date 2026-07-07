use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PinnedWatcherExitResult {
    pub(super) clear_outcome: Option<crate::services::discord::inflight::GuardedClearOutcome>,
    pub(super) submitted_terminal: bool,
}

pub(super) async fn finalize_pinned_watcher_exit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    pinned_inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    stop_source: &'static str,
) -> PinnedWatcherExitResult {
    let Some(pinned_inflight) = pinned_inflight else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            stop_source,
            "watcher exit skipped terminal submit because no pinned inflight snapshot was available"
        );
        return PinnedWatcherExitResult {
            clear_outcome: None,
            submitted_terminal: false,
        };
    };
    let finalizer_turn_id = pinned_inflight.effective_finalizer_turn_id();
    if finalizer_turn_id == 0 {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            stop_source,
            "watcher exit skipped terminal submit because pinned inflight had no effective finalizer id"
        );
        return PinnedWatcherExitResult {
            clear_outcome: None,
            submitted_terminal: false,
        };
    }

    let identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(pinned_inflight);
    let clear_outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
            provider,
            channel_id.get(),
            &identity,
        );
    match clear_outcome {
        crate::services::discord::inflight::GuardedClearOutcome::Cleared
        | crate::services::discord::inflight::GuardedClearOutcome::Missing => {}
        crate::services::discord::inflight::GuardedClearOutcome::IoError => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                stop_source,
                clear_outcome = ?clear_outcome,
                pinned_user_msg_id = pinned_inflight.user_msg_id,
                finalizer_turn_id,
                "watcher exit inflight clear failed with IO error; see preceding inflight guarded-clear error detail"
            );
            return PinnedWatcherExitResult {
                clear_outcome: Some(clear_outcome),
                submitted_terminal: false,
            };
        }
        _ => {
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                stop_source,
                clear_outcome = ?clear_outcome,
                pinned_user_msg_id = pinned_inflight.user_msg_id,
                finalizer_turn_id,
                "watcher exit inflight clear skipped because the current row no longer matches the pinned turn"
            );
            return PinnedWatcherExitResult {
                clear_outcome: Some(clear_outcome),
                submitted_terminal: false,
            };
        }
    }

    let _ = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            crate::services::discord::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            crate::services::discord::turn_finalizer::TerminalEvent::Cancel,
            crate::services::discord::turn_finalizer::FinalizeContext::monitor(),
            Some(
                crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(
                    pinned_inflight,
                ),
            ),
            shared.clone(),
        )
        .await;

    PinnedWatcherExitResult {
        clear_outcome: Some(clear_outcome),
        submitted_terminal: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::{self, InflightTurnState};
    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::ActiveTurnKind;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_root(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    #[tokio::test]
    async fn pinned_exit_preserves_newer_inflight_and_mailbox_on_identity_mismatch() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_root(temp.path());

        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_019_200_001);
        let tmux_session_name = "AgentDesk-claude-r2-stall-exit";
        let old_msg = 4_019_200_101;
        let new_msg = 4_019_200_202;

        let mut old = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            1,
            old_msg,
            old_msg,
            "old turn".to_string(),
            Some("session-old".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/r2-old.jsonl".to_string()),
            None,
            10,
        );
        old.turn_start_offset = Some(10);
        let old_snapshot = old.clone();

        let mut new = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            1,
            new_msg,
            new_msg,
            "new turn".to_string(),
            Some("session-new".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/r2-new.jsonl".to_string()),
            None,
            50,
        );
        new.turn_start_offset = Some(50);
        inflight::save_inflight_state(&new).expect("save newer row");
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                shared.as_ref(),
                channel_id,
                Arc::new(crate::services::provider::CancelToken::new()),
                UserId::new(1),
                MessageId::new(new_msg),
                ActiveTurnKind::UserOrAgent,
            )
            .await
        );

        let result = finalize_pinned_watcher_exit(
            &shared,
            &provider,
            channel_id,
            Some(&old_snapshot),
            "test_stall_exit",
        )
        .await;

        assert_eq!(
            result.clear_outcome,
            Some(crate::services::discord::inflight::GuardedClearOutcome::UserMsgMismatch)
        );
        assert!(!result.submitted_terminal);
        let persisted =
            crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("newer row survives");
        assert_eq!(persisted.user_msg_id, new_msg);
        let mailbox = shared.mailbox(channel_id).snapshot().await;
        assert_eq!(
            mailbox.active_user_message_id,
            Some(MessageId::new(new_msg))
        );
        assert!(
            mailbox.cancel_token.is_some(),
            "newer active turn token must survive stale pinned finalizer submit"
        );
    }

    #[tokio::test]
    async fn pinned_exit_releases_matching_turn_through_finalizer() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_root(temp.path());

        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_019_200_002);
        let tmux_session_name = "AgentDesk-claude-r2-stall-exit-owned";
        let msg = 4_019_200_303;
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            1,
            msg,
            msg,
            "owned turn".to_string(),
            Some("session-owned".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/r2-owned.jsonl".to_string()),
            None,
            25,
        );
        state.turn_start_offset = Some(25);
        inflight::save_inflight_state(&state).expect("save owned row");
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                shared.as_ref(),
                channel_id,
                token.clone(),
                UserId::new(1),
                MessageId::new(msg),
                ActiveTurnKind::UserOrAgent,
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let result = finalize_pinned_watcher_exit(
            &shared,
            &provider,
            channel_id,
            Some(&state),
            "test_stall_exit_owned",
        )
        .await;

        assert_eq!(
            result.clear_outcome,
            Some(crate::services::discord::inflight::GuardedClearOutcome::Cleared)
        );
        assert!(result.submitted_terminal);
        assert!(
            inflight::load_inflight_state(&provider, channel_id.get()).is_none(),
            "matching row should be cleared before finalizer submit"
        );
        let mailbox = shared.mailbox(channel_id).snapshot().await;
        assert_eq!(mailbox.active_user_message_id, None);
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    }
}
