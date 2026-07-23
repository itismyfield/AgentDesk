//! Restart recovery for the rollout-gated two-message status panel (#4488).
//!
//! A persisted panel handle can point above the latest answer chunk after a crash
//! between rollover and re-anchor, or at a panel that was deleted while dcserver
//! was down. Recovery publishes a replacement below the persisted answer and
//! atomically rebinds it to the same durable turn identity. Discord ordering and
//! message shape are only evidence that repair is needed; they never authorize
//! ownership mutation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedPanelState {
    Missing,
    Live,
    ProbeFailed,
}

type RecoveryPanelFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

trait RecoveryPanelGateway {
    fn probe_panel<'a>(
        &'a self,
        channel_id: ChannelId,
        panel_id: MessageId,
    ) -> RecoveryPanelFuture<'a, PersistedPanelState>;

    fn after_bind<'a>(&'a self) -> RecoveryPanelFuture<'a, ()> {
        Box::pin(async {})
    }

    fn after_ownership_reload<'a>(&'a self) -> RecoveryPanelFuture<'a, ()> {
        Box::pin(async {})
    }

    fn send_panel<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> RecoveryPanelFuture<'a, Result<MessageId, String>>;

    fn delete_panel<'a>(
        &'a self,
        channel_id: ChannelId,
        panel_id: MessageId,
    ) -> RecoveryPanelFuture<'a, Result<(), String>>;
}

struct SerenityRecoveryPanelGateway<'a> {
    http: &'a Arc<serenity::Http>,
}

fn probe_error_is_missing(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(http_error)
        if http_error.status_code().is_some_and(|status| status.as_u16() == 404))
}

impl RecoveryPanelGateway for SerenityRecoveryPanelGateway<'_> {
    fn probe_panel<'a>(
        &'a self,
        channel_id: ChannelId,
        panel_id: MessageId,
    ) -> RecoveryPanelFuture<'a, PersistedPanelState> {
        Box::pin(async move {
            match self.http.get_message(channel_id, panel_id).await {
                Ok(_) => PersistedPanelState::Live,
                Err(error) if probe_error_is_missing(&error) => PersistedPanelState::Missing,
                Err(error) => {
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        panel_message_id = panel_id.get(),
                        error = %error,
                        "two-message restart recovery could not probe the persisted panel; preserving it"
                    );
                    PersistedPanelState::ProbeFailed
                }
            }
        })
    }

    fn send_panel<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> RecoveryPanelFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            crate::services::discord::http::send_channel_message(self.http, channel_id, content)
                .await
                .map(|message| message.id)
                .map_err(|error| error.to_string())
        })
    }

    fn delete_panel<'a>(
        &'a self,
        channel_id: ChannelId,
        panel_id: MessageId,
    ) -> RecoveryPanelFuture<'a, Result<(), String>> {
        Box::pin(async move {
            crate::services::discord::http::delete_channel_message(self.http, channel_id, panel_id)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

fn recovery_reanchor_needed(
    answer_message_id: MessageId,
    panel_message_id: Option<MessageId>,
    panel_state: PersistedPanelState,
) -> bool {
    match (panel_message_id, panel_state) {
        (_, PersistedPanelState::ProbeFailed) => false,
        (None, _) | (Some(_), PersistedPanelState::Missing) => true,
        (Some(panel), PersistedPanelState::Live) => panel.get() <= answer_message_id.get(),
    }
}

async fn recover_two_message_panel_with_gateway<G: RecoveryPanelGateway + ?Sized>(
    gateway: &G,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut inflight::InflightTurnState,
) -> bool {
    if !shared.ui.two_message_panel_enabled || !shared.ui.status_panel_v2_enabled {
        return false;
    }
    let Some(channel_id) = inflight::opt_channel_id(state.channel_id) else {
        return false;
    };
    let Some(answer_message_id) = inflight::opt_message_id(state.current_msg_id) else {
        return false;
    };
    if crate::services::discord::is_synthetic_headless_message_id_raw(answer_message_id.get()) {
        return false;
    }

    let identity = inflight::InflightTurnIdentity::from_state(state);
    let panel_message_id = state.status_message_id.and_then(inflight::opt_message_id);
    let panel_state = match panel_message_id {
        Some(panel_id) => gateway.probe_panel(channel_id, panel_id).await,
        None => PersistedPanelState::Missing,
    };
    if !recovery_reanchor_needed(answer_message_id, panel_message_id, panel_state) {
        return false;
    }

    let started_at_unix = inflight::parse_started_at_unix(&state.started_at)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let panel_text = shared.ui.placeholder_live_events.render_status_panel(
        channel_id,
        provider,
        started_at_unix,
    );
    let new_panel_id = match gateway.send_panel(channel_id, &panel_text).await {
        Ok(message_id) => message_id,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                error = %error,
                "two-message restart recovery failed to publish a replacement panel"
            );
            return false;
        }
    };

    status_panel_orphan_store::enqueue_pending_bind(
        provider,
        &shared.token_hash,
        channel_id.get(),
        new_panel_id.get(),
        Some(identity.clone()),
    );
    let bind_outcome = inflight::bind_status_panel(
        provider,
        channel_id.get(),
        new_panel_id.get(),
        &inflight::StatusPanelBindGuard {
            require_identity: Some(identity.clone()),
            skip_if_panel_already_set: panel_message_id.is_none(),
            require_current_status_message_id: panel_message_id.map(MessageId::get),
            bump_status_panel_generation: true,
            ..Default::default()
        },
    );
    if !bind_outcome.is_bound() {
        if gateway
            .delete_panel(channel_id, new_panel_id)
            .await
            .is_err()
        {
            status_panel_orphan_store::enqueue(
                provider,
                &shared.token_hash,
                channel_id.get(),
                new_panel_id.get(),
            );
        } else {
            status_panel_orphan_store::remove(
                provider,
                &shared.token_hash,
                channel_id.get(),
                new_panel_id.get(),
            );
        }
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            outcome = ?bind_outcome,
            "two-message restart recovery lost durable episode ownership; discarded replacement panel"
        );
        return false;
    }

    gateway.after_bind().await;
    if let Some(old_panel_id) = panel_message_id
        && panel_state == PersistedPanelState::Live
        && gateway
            .delete_panel(channel_id, old_panel_id)
            .await
            .is_err()
    {
        status_panel_orphan_store::enqueue(
            provider,
            &shared.token_hash,
            channel_id.get(),
            old_panel_id.get(),
        );
    }

    let reloaded = inflight::load_inflight_state(provider, channel_id.get());
    let still_owned = reloaded.as_ref().is_some_and(|reloaded| {
        inflight::InflightTurnIdentity::from_state(state).matches_state(reloaded)
            && reloaded.status_message_id == Some(new_panel_id.get())
    });
    if !still_owned {
        if gateway
            .delete_panel(channel_id, new_panel_id)
            .await
            .is_err()
        {
            status_panel_orphan_store::enqueue(
                provider,
                &shared.token_hash,
                channel_id.get(),
                new_panel_id.get(),
            );
        } else {
            status_panel_orphan_store::remove(
                provider,
                &shared.token_hash,
                channel_id.get(),
                new_panel_id.get(),
            );
        }
        return false;
    }

    gateway.after_ownership_reload().await;
    if status_panel_orphan_store::remove_pending_bind_if_owned(
        provider,
        &shared.token_hash,
        channel_id.get(),
        new_panel_id.get(),
        &identity,
    ) == status_panel_orphan_store::PendingBindOwnedRemovalOutcome::OwnershipMismatch
    {
        return false;
    }
    *state = reloaded.expect("still_owned requires a loaded inflight row");
    true
}

pub(super) async fn recover_two_message_panel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut inflight::InflightTurnState,
) -> bool {
    recover_two_message_panel_with_gateway(
        &SerenityRecoveryPanelGateway { http },
        shared,
        provider,
        state,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct MockGateway {
        probe: PersistedPanelState,
        next_id: MessageId,
        sent: Arc<Mutex<Vec<MessageId>>>,
        deleted: Arc<Mutex<Vec<MessageId>>>,
        replacement_on_send: Option<inflight::InflightTurnState>,
        replacement_after_bind: Option<inflight::InflightTurnState>,
        replacement_after_ownership_reload: Option<inflight::InflightTurnState>,
    }

    impl RecoveryPanelGateway for MockGateway {
        fn probe_panel<'a>(
            &'a self,
            _channel_id: ChannelId,
            _panel_id: MessageId,
        ) -> RecoveryPanelFuture<'a, PersistedPanelState> {
            Box::pin(async move { self.probe })
        }

        fn after_bind<'a>(&'a self) -> RecoveryPanelFuture<'a, ()> {
            Box::pin(async move {
                if let Some(replacement) = self.replacement_after_bind.as_ref() {
                    inflight::save_inflight_state(replacement).expect("persist post-bind owner");
                }
            })
        }

        fn after_ownership_reload<'a>(&'a self) -> RecoveryPanelFuture<'a, ()> {
            Box::pin(async move {
                if let Some(replacement) = self.replacement_after_ownership_reload.as_ref() {
                    inflight::save_inflight_state(replacement)
                        .expect("persist owner after ownership reload");
                }
            })
        }

        fn send_panel<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> RecoveryPanelFuture<'a, Result<MessageId, String>> {
            Box::pin(async move {
                self.sent.lock().unwrap().push(self.next_id);
                if let Some(replacement) = self.replacement_on_send.as_ref() {
                    inflight::save_inflight_state(replacement).expect("persist replacement owner");
                }
                Ok(self.next_id)
            })
        }

        fn delete_panel<'a>(
            &'a self,
            _channel_id: ChannelId,
            panel_id: MessageId,
        ) -> RecoveryPanelFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.deleted.lock().unwrap().push(panel_id);
                Ok(())
            })
        }
    }

    fn shared_with_two_message_enabled() -> Arc<SharedData> {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        let shared_mut = Arc::get_mut(&mut shared).expect("fresh shared state");
        shared_mut.ui.status_panel_v2_enabled = true;
        shared_mut.ui.two_message_panel_enabled = true;
        shared
    }

    fn live_state(channel_id: u64, user_msg_id: u64) -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-test".to_string()),
            1,
            user_msg_id,
            300,
            "restart recovery".to_string(),
            None,
            Some("AgentDesk-claude-adk-test".to_string()),
            Some("/tmp/issue-4488.jsonl".to_string()),
            None,
            10,
        );
        state.status_message_id = Some(200);
        state.status_panel_generation = 3;
        state
    }

    fn isolate_runtime_root() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
        let root = tempfile::tempdir().expect("runtime root");
        let guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        (root, guard)
    }

    #[tokio::test]
    async fn restart_reanchors_stranded_panel_and_persists_new_epoch() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _guard) = isolate_runtime_root();
        let shared = shared_with_two_message_enabled();
        let provider = ProviderKind::Claude;
        let mut state = live_state(44_880, 7_001);
        inflight::save_inflight_state(&state).expect("seed inflight");
        let gateway = MockGateway {
            probe: PersistedPanelState::Live,
            next_id: MessageId::new(400),
            sent: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            replacement_on_send: None,
            replacement_after_bind: None,
            replacement_after_ownership_reload: None,
        };

        assert!(
            recover_two_message_panel_with_gateway(&gateway, &shared, &provider, &mut state).await
        );
        assert_eq!(state.current_msg_id, 300);
        assert_eq!(state.status_message_id, Some(400));
        assert_eq!(state.status_panel_generation, 4);
        assert_eq!(*gateway.deleted.lock().unwrap(), vec![MessageId::new(200)]);
        assert!(status_panel_orphan_store::load_pending(&provider, &shared.token_hash).is_empty());

        assert!(
            !recover_two_message_panel_with_gateway(&gateway, &shared, &provider, &mut state).await,
            "a second recovery pass must reuse the already-correct panel"
        );
        assert_eq!(gateway.sent.lock().unwrap().len(), 1);
        assert_eq!(gateway.deleted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restart_recreates_missing_panel_without_duplicate_old_delete() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _guard) = isolate_runtime_root();
        let shared = shared_with_two_message_enabled();
        let provider = ProviderKind::Claude;
        let mut state = live_state(44_881, 7_002);
        state.status_message_id = Some(500);
        state.current_msg_id = 300;
        inflight::save_inflight_state(&state).expect("seed inflight");
        let gateway = MockGateway {
            probe: PersistedPanelState::Missing,
            next_id: MessageId::new(600),
            sent: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            replacement_on_send: None,
            replacement_after_bind: None,
            replacement_after_ownership_reload: None,
        };

        assert!(
            recover_two_message_panel_with_gateway(&gateway, &shared, &provider, &mut state).await
        );
        assert_eq!(state.status_message_id, Some(600));
        assert!(gateway.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn replacement_owner_survives_recovery_send_race() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _guard) = isolate_runtime_root();
        let shared = shared_with_two_message_enabled();
        let provider = ProviderKind::Claude;
        let mut stale = live_state(44_882, 7_003);
        inflight::save_inflight_state(&stale).expect("seed stale owner");
        let mut replacement = live_state(44_882, 8_003);
        replacement.started_at = "2099-01-01 00:00:00".to_string();
        replacement.current_msg_id = 900;
        replacement.status_message_id = Some(901);
        replacement.status_panel_generation = 11;
        let gateway = MockGateway {
            probe: PersistedPanelState::Live,
            next_id: MessageId::new(400),
            sent: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            replacement_on_send: Some(replacement.clone()),
            replacement_after_bind: None,
            replacement_after_ownership_reload: None,
        };

        assert!(
            !recover_two_message_panel_with_gateway(&gateway, &shared, &provider, &mut stale).await
        );
        let durable = inflight::load_inflight_state(&provider, replacement.channel_id)
            .expect("replacement owner remains");
        assert_eq!(durable.user_msg_id, replacement.user_msg_id);
        assert_eq!(durable.current_msg_id, 900);
        assert_eq!(durable.status_message_id, Some(901));
        assert_eq!(durable.status_panel_generation, 11);
        assert_eq!(*gateway.deleted.lock().unwrap(), vec![MessageId::new(400)]);
    }

    #[tokio::test]
    async fn replacement_owner_after_bind_cannot_leak_recovery_panel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _guard) = isolate_runtime_root();
        let shared = shared_with_two_message_enabled();
        let provider = ProviderKind::Claude;
        let mut stale = live_state(44_883, 7_004);
        inflight::save_inflight_state(&stale).expect("seed stale owner");
        let mut replacement = live_state(44_883, 8_004);
        replacement.started_at = "2099-01-01 00:00:01".to_string();
        replacement.current_msg_id = 900;
        replacement.status_message_id = Some(901);
        replacement.status_panel_generation = 11;
        let gateway = MockGateway {
            probe: PersistedPanelState::Live,
            next_id: MessageId::new(400),
            sent: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            replacement_on_send: None,
            replacement_after_bind: Some(replacement.clone()),
            replacement_after_ownership_reload: None,
        };

        assert!(
            !recover_two_message_panel_with_gateway(&gateway, &shared, &provider, &mut stale).await
        );
        let durable = inflight::load_inflight_state(&provider, replacement.channel_id)
            .expect("replacement owner remains");
        assert_eq!(durable.user_msg_id, replacement.user_msg_id);
        assert_eq!(durable.status_message_id, Some(901));
        assert_eq!(
            *gateway.deleted.lock().unwrap(),
            vec![MessageId::new(200), MessageId::new(400)]
        );
        assert!(status_panel_orphan_store::load_pending(&provider, &shared.token_hash).is_empty());
    }

    #[tokio::test]
    async fn replacement_owner_after_final_reload_keeps_recovery_panel_protected() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_root, _guard) = isolate_runtime_root();
        let shared = shared_with_two_message_enabled();
        let provider = ProviderKind::Claude;
        let mut stale = live_state(44_884, 7_005);
        inflight::save_inflight_state(&stale).expect("seed stale owner");
        let mut replacement = live_state(44_884, 8_005);
        replacement.started_at = "2099-01-01 00:00:02".to_string();
        replacement.current_msg_id = 900;
        replacement.status_message_id = Some(901);
        replacement.status_panel_generation = 11;
        let gateway = MockGateway {
            probe: PersistedPanelState::Live,
            next_id: MessageId::new(400),
            sent: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            replacement_on_send: None,
            replacement_after_bind: None,
            replacement_after_ownership_reload: Some(replacement.clone()),
        };

        assert!(
            !recover_two_message_panel_with_gateway(&gateway, &shared, &provider, &mut stale).await
        );
        let durable = inflight::load_inflight_state(&provider, replacement.channel_id)
            .expect("replacement owner remains");
        assert_eq!(durable.user_msg_id, replacement.user_msg_id);
        assert_eq!(durable.status_message_id, Some(901));
        assert_eq!(*gateway.deleted.lock().unwrap(), vec![MessageId::new(200)]);
        assert_eq!(
            status_panel_orphan_store::load_pending(&provider, &shared.token_hash),
            vec![(replacement.channel_id, 400)],
            "the failed exact-owner removal must leave the panel for the sweeper"
        );
    }

    #[test]
    fn ordering_is_only_repair_evidence() {
        assert!(recovery_reanchor_needed(
            MessageId::new(300),
            Some(MessageId::new(200)),
            PersistedPanelState::Live,
        ));
        assert!(!recovery_reanchor_needed(
            MessageId::new(300),
            Some(MessageId::new(400)),
            PersistedPanelState::Live,
        ));
        assert!(recovery_reanchor_needed(
            MessageId::new(300),
            Some(MessageId::new(400)),
            PersistedPanelState::Missing,
        ));
        assert!(!recovery_reanchor_needed(
            MessageId::new(300),
            Some(MessageId::new(200)),
            PersistedPanelState::ProbeFailed,
        ));
    }
}
