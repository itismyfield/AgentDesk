use super::*;
use crate::services::discord::serenity::UserId;
#[cfg(unix)]
use crate::services::discord::turn_end_summary::LiveSummary;
use crate::services::discord::turn_end_summary::tests::{Gateway, Isolation, row};
use crate::services::turn_summary_line::Outcome;

struct Case {
    outcome: Outcome,
    terminal: bool,
    emit: bool,
    foreign: bool,
    send_body: bool,
    single: bool,
    panel: Option<MessageId>,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            outcome: Outcome::Completed,
            terminal: true,
            emit: true,
            foreign: false,
            send_body: true,
            single: true,
            panel: None,
        }
    }
}

async fn bridge(
    shared: Arc<SharedData>,
    gateway: Arc<dyn TurnGateway>,
    inflight: InflightTurnState,
    case: Case,
) {
    let channel = ChannelId::new(inflight.channel_id);
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = if case.foreign {
        Arc::new(crate::services::provider::CancelToken::new())
    } else {
        token.clone()
    };
    assert!(
        crate::services::discord::mailbox_try_start_turn_kinded(
            &shared,
            channel,
            owner,
            UserId::new(1),
            MessageId::new(inflight.user_msg_id.max(1)),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    if case.send_body {
        let body = crate::services::discord::formatting::build_streaming_placeholder_text(
            "answer",
            "⠸ 계속 처리 중",
        );
        gateway.send_message(channel, &body).await.unwrap();
    }
    let fence = tokio::sync::OnceCell::new();
    crate::db::session_transcripts::observe_channel_clear_fence_once(
        &fence,
        None,
        &channel.to_string(),
    )
    .await;
    let guard = CompletionGuard::for_completion_test(
        shared.clone(),
        channel,
        inflight.effective_finalizer_turn_id(),
    );
    let mut inflight_guard = InflightCleanupGuard::for_completion_test(&inflight, "test".into());
    inflight_guard.defuse();
    let ctx = CompletionPostludeContext {
        shared_owned: shared.clone(),
        gateway: gateway.clone(),
        channel_id: channel,
        provider: ProviderKind::Claude,
        cancel_token: token,
        user_msg_id: None,
        turn_id: format!("summary-{}", inflight.effective_finalizer_turn_id()),
        request_owner_name: String::new(),
        final_session_status: "idle",
        status_panel_started_at: 0,
        has_queued_turns: false,
        defer_watcher_resume: true,
        can_chain_locally: false,
        single_message_panel_footer_mode:
            crate::services::discord::single_message_panel::footer_mode_enabled(
                case.single,
                shared.ui.status_panel_v2_enabled,
            ),
        is_external_input_tui_direct: false,
        context_window_tokens: 1000,
        context_compact_percent: 80,
        clear_fence: fence.into_inner().unwrap(),
        turn_start: std::time::Instant::now(),
    };
    let state = CompletionPostludeState {
        watcher_delivery_pin: None,
        full_response: "answer".into(),
        user_text_owned: "question".into(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        new_session_id: None,
        new_raw_provider_session_id: None,
        status_panel_terminal_committed: case.terminal,
        bridge_should_emit_completion: case.emit,
        current_msg_id: MessageId::new(100),
        status_panel_msg_id: case.panel,
        last_status_panel_text: String::new(),
        completion_footer_terminal_text: Some("answer".into()),
        busy_requeue_outcome: None,
        spin_idx: 0,
        status_panel_generation: 0,
        preserve_inflight_for_cleanup_retry: true,
        tmux_last_offset: None,
        watcher_owner_channel_id: channel,
        bridge_relay_delegated_to_watcher: false,
        is_prompt_too_long: false,
        resume_failure_detected: false,
        auto_retry: Default::default(),
        recovery_retry: false,
        rx_disconnected: false,
        tmux_handed_off: false,
        bridge_output_owner: None,
        terminal_delivery_committed: case.terminal,
        terminal_session_reset_required: false,
        transcript_events: Vec::new(),
        accumulated_input_tokens: 100,
        accumulated_cache_create_tokens: 200,
        accumulated_cache_read_tokens: 150,
        accumulated_output_tokens: 10,
        accumulated_memory_input_tokens: 0,
        accumulated_memory_output_tokens: 0,
        transport_error: false,
        provider_failure: case.outcome == Outcome::Failed,
        api_friction_reports: Vec::new(),
        cancelled: case.outcome == Outcome::Cancelled,
        restart_followup_pending: None,
        bridge_skip_holder_owns_inflight: false,
        completion_guard: guard,
        inflight_guard,
        inflight_state: inflight,
    };
    assert!(!ctx.single_message_panel_footer_mode);
    super::super::run_completion_postlude(ctx, state).await;
}

fn shared(v2: bool) -> Arc<SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .unwrap()
        .ui
        .status_panel_v2_enabled = v2;
    shared
}

#[tokio::test(flavor = "current_thread")]
async fn real_bridge_postlude_posts_one_terminal_summary_for_all_retired_mode_flags_and_outcomes() {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert!(crate::services::discord::footer_retirement::retired());
    let mut channel = 586_101;
    for v2 in [false, true] {
        for single in [false, true] {
            for outcome in [Outcome::Completed, Outcome::Cancelled, Outcome::Failed] {
                let shared = shared(v2);
                assert!(
                    shared
                        .ui
                        .placeholder_live_events
                        .context_panel_snapshot(ChannelId::new(channel))
                        .is_none()
                );
                let gateway = Arc::new(Gateway::default());
                bridge(
                    shared.clone(),
                    gateway.clone(),
                    row(channel, 11, "bridge"),
                    Case {
                        outcome,
                        single,
                        ..Default::default()
                    },
                )
                .await;
                let posts = gateway.posts();
                assert_eq!(
                    posts.len(),
                    2,
                    "v2={v2} single={single} outcome={outcome:?}: {posts:?}"
                );
                assert_eq!(posts[0], "answer");
                assert!(posts[1].starts_with(match outcome {
                    Outcome::Completed => "-# ✅ ",
                    Outcome::Cancelled => "-# ⏹ 중단 · ",
                    Outcome::Failed => "-# ⚠️ 오류 · ",
                }));
                assert!(
                    posts[1].ends_with(" · 📦 45%"),
                    "exact usage backfill: {posts:?}"
                );
                assert!(gateway.edited.lock().unwrap().is_empty());
                channel += 1;
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_summary_keeps_terminal_emit_and_episode_authority_guards_after_footer_retirement() {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for (index, case) in [
        Case {
            terminal: false,
            ..Default::default()
        },
        Case {
            emit: false,
            ..Default::default()
        },
        Case {
            foreign: true,
            ..Default::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let gateway = Arc::new(Gateway::default());
        bridge(
            shared(true),
            gateway.clone(),
            row(586_121 + index as u64, 11, "guard"),
            case,
        )
        .await;
        assert_eq!(gateway.posts(), ["answer"]);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn real_bridge_and_watcher_producers_share_the_pinned_episode_latch_in_either_order() {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for watcher_first in [false, true] {
        let channel = 586_131 + u64::from(watcher_first);
        let shared = shared(false);
        let mut state = row(channel, 0, "same-episode");
        state.tmux_session_name = Some("fixture-summary".into());
        let key =
            crate::services::discord::tmux::tmux_watcher::completion_producer::watcher_completion_key(
                &shared,
                ChannelId::new(channel),
                Some(&state),
                "fixture-summary",
                30,
            )
            .unwrap();
        let snapshot =
            LiveSummary::watcher(&shared, Some(key), Some(&state), false, Outcome::Completed)
                .unwrap();
        state.tmux_session_name = None;
        state.finalizer_turn_id = key.user_msg_id;
        let gateway = Arc::new(Gateway::default());
        gateway
            .send_message(ChannelId::new(channel), "answer")
            .await
            .unwrap();
        let recorder =
            crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
        if watcher_first {
            watcher(&recorder.http, &shared, channel, snapshot).await;
        }
        bridge(
            shared.clone(),
            gateway.clone(),
            state,
            Case {
                send_body: false,
                ..Default::default()
            },
        )
        .await;
        if !watcher_first {
            watcher(&recorder.http, &shared, channel, snapshot).await;
        }
        let bridge_posts = gateway.posts();
        let watcher_posts = recorder.contents();
        if watcher_first {
            assert_eq!(
                bridge_posts,
                ["answer"],
                "actual bridge/watcher producers must have one summary in watcher-first order"
            );
            assert_eq!(watcher_posts, ["-# ✅ 완료"]);
        } else {
            assert!(watcher_posts.is_empty(), "{watcher_posts:?}");
            assert_eq!(bridge_posts.len(), 2, "{bridge_posts:?}");
            assert_eq!(bridge_posts[0], "answer");
            assert!(bridge_posts[1].starts_with("-# ✅ "));
            assert!(bridge_posts[1].ends_with(" · 📦 45%"));
        }
    }
}

#[cfg(unix)]
async fn watcher(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel: u64,
    snapshot: LiveSummary,
) {
    let _mailbox = shared.mailbox(ChannelId::new(channel));
    let mut spin = 0;
    let mut last = String::new();
    crate::services::discord::tmux::tmux_watcher::completion_producer::complete_watcher_terminal_footer_or_status_panel_with_sniffer(
        http,shared,ChannelId::new(channel),&ProviderKind::Claude,0,false,&mut spin,None,None,"answer",None,&mut last,None,None,|_|async{false},None,false,false,false,Some(snapshot),None,
    ).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn actual_producers_keep_equal_offset_turns_with_distinct_sources_and_nonces_separate() {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = 586_141;
    let shared = shared(false);
    let gateway = Arc::new(Gateway::default());
    let recorder = crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
    for (source, nonce) in [("a", "first"), ("b", "first"), ("a", "next")] {
        let mut state = row(channel, 0, nonce);
        state.started_at = "2026-10-19 00:00:00".into();
        state.output_path = Some(format!("/fixture/{source}.jsonl"));
        state.tmux_session_name = Some("fixture-summary".into());
        let key =
            crate::services::discord::tmux::tmux_watcher::completion_producer::watcher_completion_key(
                &shared,
                ChannelId::new(channel),
                Some(&state),
                "fixture-summary",
                30,
            )
            .unwrap();
        let snapshot =
            LiveSummary::watcher(&shared, Some(key), Some(&state), false, Outcome::Completed)
                .unwrap();
        state.tmux_session_name = None;
        state.finalizer_turn_id = key.user_msg_id;
        bridge(shared.clone(), gateway.clone(), state, Default::default()).await;
        watcher(&recorder.http, &shared, channel, snapshot).await;
        crate::services::discord::mailbox_finish_turn(
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(channel),
        )
        .await;
    }
    let posts = gateway.posts();
    assert_eq!(posts.len(), 6, "{posts:?}");
    for pair in posts.chunks_exact(2) {
        assert_eq!(pair[0], "answer");
        assert!(pair[1].starts_with("-# ✅ "));
    }
    assert!(recorder.contents().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn real_bridge_postlude_suppresses_o_owned_and_unresolved_identity_summaries() {
    let _isolation = Isolation::new();
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    let channel = 586_151;
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[
        (channel, ClaudeTui),
        (channel + 1, ClaudeTui),
    ]);
    for unknown in [false, true] {
        let channel = channel + u64::from(unknown);
        let mut state = row(channel, 11, "o-owned");
        if unknown {
            state.runtime_kind = None;
        }
        let gateway = Arc::new(Gateway::default());
        bridge(shared(false), gateway.clone(), state, Default::default()).await;
        assert_eq!(
            gateway.posts(),
            ["answer"],
            "actual O-owned bridge completion must not POST Legacy summary"
        );
    }
}

struct SuspendedPanelGateway {
    inner: Arc<Gateway>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl TurnGateway for SuspendedPanelGateway {
    fn send_message<'a>(
        &'a self,
        channel: ChannelId,
        text: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<MessageId, String>> {
        self.inner.send_message(channel, text)
    }
    fn edit_message<'a>(
        &'a self,
        channel: ChannelId,
        id: MessageId,
        text: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.edit_message(channel, id, text).await
        })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        channel: ChannelId,
        id: MessageId,
        text: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<
        'a,
        Result<crate::services::discord::formatting::ReplaceLongMessageOutcome, String>,
    > {
        self.inner.replace_message_with_outcome(channel, id, text)
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        channel: ChannelId,
        id: MessageId,
        text: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, ()> {
        self.inner.schedule_retry_with_history(channel, id, text)
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        channel: ChannelId,
        intervention: &'a crate::services::discord::Intervention,
        owner: &'a str,
        more: bool,
        lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        self.inner
            .dispatch_queued_turn(channel, intervention, owner, more, lease)
    }
    fn validate_live_routing<'a>(
        &'a self,
        channel: ChannelId,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        self.inner.validate_live_routing(channel)
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn can_deliver_directly(&self) -> bool {
        true
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn suspended_panel_projection_rechecks_summary_authority_without_consuming_the_latch() {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let shared = shared(true);
    let channel = ChannelId::new(586_161);
    let state = row(channel.get(), 11, "suspended-projection");
    let inner = Arc::new(Gateway::default());
    let gateway = Arc::new(SuspendedPanelGateway {
        inner: inner.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let completion = bridge(
        shared.clone(),
        gateway.clone(),
        state.clone(),
        Case {
            panel: Some(MessageId::new(101)),
            ..Default::default()
        },
    );
    let successor = Arc::new(crate::services::provider::CancelToken::new());
    let takeover = async {
        gateway.entered.notified().await;
        crate::services::discord::mailbox_finish_turn(&shared, &ProviderKind::Claude, channel)
            .await;
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                &shared,
                channel,
                successor.clone(),
                UserId::new(1),
                MessageId::new(12),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent,
            )
            .await
        );
        gateway.release.notify_one();
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(completion, takeover);
    })
    .await
    .unwrap();
    assert_eq!(
        inner.posts(),
        ["answer"],
        "an owner admitted before the panel await cannot post after takeover"
    );
    assert_eq!(
        inner.edited.lock().unwrap().len(),
        1,
        "the upgrade panel projection still completes"
    );
    let snapshot = shared.mailbox_peek(channel).unwrap().snapshot().await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &successor))
    );

    crate::services::discord::mailbox_finish_turn(&shared, &ProviderKind::Claude, channel).await;
    bridge(
        shared,
        inner.clone(),
        state,
        Case {
            send_body: false,
            ..Default::default()
        },
    )
    .await;
    let posts = inner.posts();
    assert_eq!(
        posts.len(),
        2,
        "the veto must not consume the captured episode's summary latch"
    );
    assert_eq!(posts[0], "answer");
    assert!(posts[1].starts_with("-# ✅ "));
}
