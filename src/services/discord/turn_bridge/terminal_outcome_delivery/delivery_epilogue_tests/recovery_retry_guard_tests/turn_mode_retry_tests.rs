//! A turn-mode turn without an anchor whose recovery queues a retry shows one continue reply to
//! the user's message: no failure notice, and nothing aimed at the missing message.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;

/// The retry counter, with every edit sent through the live gateway's real transport.
struct LiveEdits(
    RetryCounter,
    crate::services::discord::gateway::DiscordGateway,
);

impl TurnGateway for LiveEdits {
    fn send_message<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.0.send_message(channel_id, content)
    }

    fn send_notice_once<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.0.send_notice_once(channel_id, content)
    }

    fn edit_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        TurnGateway::edit_message(&self.1, channel_id, message_id, content)
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        self.0
            .replace_message_with_outcome(channel_id, message_id, content)
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        self.0
            .schedule_retry_with_history(channel_id, user_message_id, user_text)
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        channel_id: ChannelId,
        intervention: &'a Intervention,
        request_owner_name: &'a str,
        has_more_queued_turns: bool,
        dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.0.dispatch_queued_turn(
            channel_id,
            intervention,
            request_owner_name,
            has_more_queued_turns,
            dispatch_lease,
        )
    }

    fn validate_live_routing<'a>(
        &'a self,
        channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.0.validate_live_routing(channel_id)
    }

    fn requester_mention(&self) -> Option<String> {
        self.0.requester_mention()
    }

    fn can_chain_locally(&self) -> bool {
        self.0.can_chain_locally()
    }

    fn can_deliver_directly(&self) -> bool {
        self.0.can_deliver_directly()
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        self.0.bot_owner_provider()
    }
}

#[test]
fn an_anchorless_recovery_retry_shows_one_continue_reply_pg() {
    const CHILD: &str = "ADK_I7507_ANCHORLESS_RETRY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // The direct API context is process-global, so the fake API runs in a fresh process.
        let root = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::an_anchorless_recovery_retry_shows_one_continue_reply_pg",
            module_path!().split_once("::").unwrap().1
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[(
        DRIVER_CHANNEL_ID,
        ClaudeTui,
    )]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(DRIVER_CHANNEL_ID);
    runtime.block_on(async {
        let app = axum::Router::new().fallback(|| async { axum::Json(serde_json::json!({})) });
        let loopback = crate::config::loopback();
        let listener = tokio::net::TcpListener::bind((loopback.as_str(), 0));
        let listener = listener.await.unwrap();
        crate::services::discord::internal_api::init(listener.local_addr().unwrap().port(), None);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
        driver.inflight.runtime_kind = Some(ClaudeTui);
        driver.inflight.current_msg_id = 0;
        crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
        let shared = driver.shared.clone();
        let channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
        let mut session = resumable_session();
        session.channel_name = Some("i7507-anchorless-retry".to_string());
        shared.core.lock().await.sessions.insert(channel_id, session);
        let _mailbox = shared.mailbox(channel_id);
        let key = crate::services::discord::adk_session::build_adk_session_key(
            &shared,
            channel_id,
            &ProviderKind::Claude,
            None,
        );
        let key = key.await.expect("channel session key");
        crate::services::discord::inflight::seed_session_row_keyed(
            &pool,
            &key,
            DRIVER_CHANNEL_ID,
            None,
        )
        .await;
        let discord =
            crate::services::discord::recovery_engine::o_cut_recorder::start(DRIVER_CHANNEL_ID)
                .await;
        let _rest =
            crate::services::discord::shared_state::test_rest::install(discord.http.clone());

        let scheduled = Arc::new(AtomicUsize::new(0));
        let edits = Arc::new(Mutex::new(Vec::new()));
        let (mut ctx, mut state) = driver.parts();
        ctx.user_msg_id = Some(MessageId::new(DRIVER_USER_MSG_ID));
        ctx.recovery_retry = true;
        ctx.current_msg_id =
            crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(0);
        let live = crate::services::discord::gateway::DiscordGateway::new(
            discord.http.clone(),
            shared.clone(),
            ProviderKind::Claude,
            None,
        );
        let counter = RetryCounter(scheduled.clone(), edits.clone());
        state.gateway = Arc::new(LiveEdits(counter, live));
        state
            .cancel_token
            .bind_unmanaged_session_name(DRIVER_TMUX_SESSION);
        state.adk_session_key = Some(key);
        state.full_response = "partial".to_string();
        state.inflight_state.current_msg_id = 0;
        state.inflight_state.runtime_kind = Some(ClaudeTui);
        state.new_session_id = Some("sid-turn".to_string());
        state.inflight_state.session_id = Some("sid-turn".to_string());
        state.terminal_full_replay_cleanup_msg_ids.clear();
        let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state));
        output.await.expect("terminal delivery finishes");
        for _ in 0..200 {
            if scheduled.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(scheduled.load(Ordering::SeqCst), 1, "one retry is queued");
        let calls = discord.calls();
        assert_eq!(calls.len(), 1, "one notice and no PATCH or DELETE: {calls:?}");
        assert!(calls[0].route.starts_with("POST "), "{calls:?}");
        assert_eq!(calls[0].reply_to, Some(DRIVER_USER_MSG_ID.to_string()));
        let shown = calls[0].content.clone().unwrap_or_default();
        assert!(shown.contains("자동으로 이어갑니다"), "{shown}");
        assert!(!shown.contains("다시 시도"), "no failure notice: {shown}");
        let edits = edits.lock().unwrap().clone();
        assert!(
            edits.iter().all(|text| text.starts_with("-# ")),
            "nothing edits the missing message: {edits:?}"
        );
        server.abort();
        pool.close().await;
        db.drop().await;
        drop(driver);
    });
}
