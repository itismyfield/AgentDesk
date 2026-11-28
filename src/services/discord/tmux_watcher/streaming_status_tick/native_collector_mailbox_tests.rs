/// The inflight-gate debug event below the mailbox gate witnesses that a call
/// got past it: idle mailboxes must reach it, busy/unreachable ones must not.
#[test]
fn watcher_direct_idle_commit_passes_mailbox_gate_only_when_idle() {
    use crate::services::provider::CancelToken;
    use poise::serenity_prelude::{MessageId, UserId};
    use tracing_subscriber::layer::SubscriberExt;
    struct Liveness(std::sync::Arc<std::sync::Mutex<usize>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Liveness {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event
                .metadata()
                .target()
                .ends_with("tmux_watcher::liveness")
            {
                *self.0.lock().unwrap() += 1;
            }
        }
    }

    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = crate::services::provider::ProviderKind::Claude;
    let (idle, busy, dead) = (
        poise::serenity_prelude::ChannelId::new(6_046_101),
        poise::serenity_prelude::ChannelId::new(6_046_102),
        poise::serenity_prelude::ChannelId::new(6_046_103),
    );
    for channel in [idle, busy, dead] {
        let state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            Some("adk-cc".to_string()),
            7,
            1001,
            1002,
            "prompt".to_string(),
            None,
            Some("s".to_string()),
            None,
            None,
            0,
        );
        crate::services::discord::inflight::save_inflight_state(&state).expect("save inflight");
    }

    let events = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let subscriber = tracing_subscriber::registry().with(Liveness(std::sync::Arc::clone(&events)));
    crate::logging::test_capture::pin_callsite_interest();
    let _guard = tracing::subscriber::set_default(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        assert!(
            shared
                .mailbox(busy)
                .try_start_turn(
                    std::sync::Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(77)
                )
                .await
        );
        shared.mailboxes.insert_unreachable_for_test(dead);
        let commit = |channel| {
            crate::services::discord::tmux::tmux_watcher::liveness::commit_watcher_direct_terminal_session_idle(
                &shared, &provider, channel, "s", None, 0, 0,
            )
        };

        assert!(!commit(idle).await);
        assert_eq!(
            *events.lock().unwrap(),
            1,
            "idle must reach the inflight gate"
        );
        assert!(!commit(busy).await);
        assert!(!commit(dead).await);
        assert_eq!(
            *events.lock().unwrap(),
            1,
            "busy/unreachable must stop at the mailbox gate"
        );
    });
}
