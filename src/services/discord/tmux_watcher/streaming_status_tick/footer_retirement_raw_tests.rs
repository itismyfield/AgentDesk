use super::*;
use crate::services::discord::footer_retirement;
use crate::services::discord::placeholder_live_events::footer_activity_tests::{
    START, six_tool_fixture,
};

const ANCHOR: &str = "⠋ 계속 처리 중";

fn isolated(test: &str) -> bool {
    const CHILD: &str = "AGENTDESK_FOOTER_RETIRED_RAW_WATCHER_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let name = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("AGENTDESK_SINGLE_MESSAGE_PANEL", "1")
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "raw watcher child: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    false
}

fn assert_history_is_body_or_anchor(contents: &[String]) {
    assert!(
        !contents.is_empty(),
        "the actual REST seam must be exercised"
    );
    for content in contents {
        assert!(
            !content.contains("fixture"),
            "rich tool footer leaked: {content}"
        );
        assert!(
            !content.contains("진행 중 —"),
            "rich header leaked: {content}"
        );
        assert!(
            !content.contains("<t:"),
            "rich elapsed timestamp leaked: {content}"
        );
        assert_ne!(content.trim(), "…");
        assert_ne!(content.trim(), "...");
        if content.contains("계속 처리 중") {
            assert_eq!(
                content, ANCHOR,
                "only the minimal pre-answer anchor is allowed"
            );
        }
    }
}

#[test]
fn retired_watcher_rollover_raw_history_keeps_rich_budget_and_visible_progress() {
    if !isolated("retired_watcher_rollover_raw_history_keeps_rich_budget_and_visible_progress") {
        return;
    }
    let (_lock, guard) = isolate_root();
    let fx = seed_row(guard.root.path(), 63_271, false, false);
    capture_warns(async {
        let shared = six_tool_fixture(crate::config::ToolActivityDisplay::Summary, fx.channel);
        let rec = recorder(fx.channel, true).await;
        let mut locals = tick_locals(&fx, Some(PLACEHOLDER_MSG));
        let body = "x".repeat(3700);
        let budget = build_watcher_single_message_panel_status_block(
            &shared,
            fx.channel,
            &fx.provider,
            START,
            "⠋",
            None,
            None,
            &body,
            None,
        );
        assert!(
            budget.contains("마지막 도구"),
            "this must be a rich budget fixture"
        );
        assert!(budget.len() > ANCHOR.len());
        let mut expected = Vec::new();
        let mut sent = 0;
        {
            let _legacy = footer_retirement::legacy_for_test();
            while let Some((plan, split)) = plan_watcher_streaming_rollover_with_session_banner(
                &shared,
                fx.channel,
                &fx.provider,
                fx.identity.user_msg_id,
                Some(&fx.identity.started_at),
                fx.identity.turn_start_offset,
                sent,
                &body[sent..],
                &budget,
            ) {
                expected.push(plan.frozen_chunk);
                sent += split;
            }
        }
        assert!(!expected.is_empty(), "fixture must traverse rollover POST");
        let spinner_plan = plan_watcher_streaming_rollover_with_session_banner(
            &shared,
            fx.channel,
            &fx.provider,
            fx.identity.user_msg_id,
            Some(&fx.identity.started_at),
            fx.identity.turn_start_offset,
            0,
            &body,
            ANCHOR,
        )
        .unwrap();
        assert_ne!(
            expected[0], spinner_plan.0.frozen_chunk,
            "spinner budget must be a detectable mutant"
        );
        assert_eq!(
            run_tick_with_tools(
                &mut locals,
                &rec,
                &shared,
                &fx,
                &body,
                &WatcherToolState::new()
            )
            .await,
            FALLTHROUGH,
        );
        let contents = rec.bodies.lock().unwrap().clone();
        assert_history_is_body_or_anchor(&contents);
        let mut expected_history = Vec::new();
        for frozen in &expected {
            expected_history.push(frozen.clone());
            expected_history.push(ANCHOR.to_string());
        }
        let tail = crate::services::discord::formatting::format_for_discord_with_status_panel(
            &body[sent..],
            &fx.provider,
        );
        expected_history.push(tail.clone());
        assert_eq!(
            contents, expected_history,
            "all raw POST/PATCH bodies must match"
        );
        assert_eq!(rec.seen("POST").len(), expected.len());
        assert_eq!(locals.sent, sent);
        assert_eq!(
            locals.edit, tail,
            "last-edit cache records the visible bytes"
        );
        assert_eq!(locals.frozen.len(), expected.len());
        assert_eq!(locals.frozen[0], serenity::MessageId::new(PLACEHOLDER_MSG));
        assert_eq!(locals.placeholder, msg(SERVER_MSG));
        let row = load_inflight_state(&fx.provider, fx.channel.get()).unwrap();
        assert_eq!(row.response_sent_offset, sent);
        assert_eq!(row.current_msg_id, SERVER_MSG);
        assert_eq!(row.full_response, body);
    });
}

#[test]
fn retired_watcher_tool_only_raw_post_is_minimal_before_the_body_patch() {
    if !isolated("retired_watcher_tool_only_raw_post_is_minimal_before_the_body_patch") {
        return;
    }
    let (_lock, guard) = isolate_root();
    capture_warns(async {
        for (case, v2) in [(63_272, false), (63_273, true)] {
            let fx = seed_row(guard.root.path(), case, false, false);
            let mut shared =
                six_tool_fixture(crate::config::ToolActivityDisplay::Summary, fx.channel);
            Arc::get_mut(&mut shared)
                .unwrap()
                .ui
                .status_panel_v2_enabled = v2;
            let rec = recorder(fx.channel, true).await;
            let mut locals = tick_locals(&fx, None);
            let mut tools = WatcherToolState::new();
            tools.current_tool_line = Some("⚙ Bash: cargo test secret-tool".to_string());
            tools.any_tool_used = true;
            run_tick_with_tools(&mut locals, &rec, &shared, &fx, "", &tools).await;
            assert_eq!(rec.seen("POST").len(), 1);
            assert_eq!(*rec.bodies.lock().unwrap(), vec![ANCHOR.to_string()]);
            assert_eq!(locals.edit, ANCHOR);
            assert_eq!(locals.sent, 0);
            assert_eq!(locals.placeholder, msg(SERVER_MSG));
            let body = "도구 뒤에 도착한 실제 답변";
            run_tick_with_tools(&mut locals, &rec, &shared, &fx, body, &tools).await;
            let contents = rec.bodies.lock().unwrap().clone();
            assert_history_is_body_or_anchor(&contents);
            assert_eq!(contents, vec![ANCHOR.to_string(), body.to_string()]);
            assert_eq!(
                rec.seen("POST").len(),
                1,
                "the same answer anchor is edited"
            );
            assert_eq!(locals.edit, body);
        }
    });
}

#[test]
fn retired_watcher_empty_existing_anchor_raw_patch_is_minimal() {
    if !isolated("retired_watcher_empty_existing_anchor_raw_patch_is_minimal") {
        return;
    }
    let (_lock, guard) = isolate_root();
    capture_warns(async {
        for (case, v2) in [(63_274, false), (63_275, true)] {
            let fx = seed_row(guard.root.path(), case, false, false);
            let mut shared =
                six_tool_fixture(crate::config::ToolActivityDisplay::Summary, fx.channel);
            Arc::get_mut(&mut shared)
                .unwrap()
                .ui
                .status_panel_v2_enabled = v2;
            let rec = recorder(fx.channel, true).await;
            let mut locals = tick_locals(&fx, Some(PLACEHOLDER_MSG));
            run_tick_with_tools(
                &mut locals,
                &rec,
                &shared,
                &fx,
                "",
                &WatcherToolState::new(),
            )
            .await;
            assert!(rec.seen("POST").is_empty());
            assert_eq!(rec.seen("PATCH").len(), 1);
            assert_eq!(*rec.bodies.lock().unwrap(), vec![ANCHOR.to_string()]);
            assert_eq!(locals.edit, ANCHOR);
            assert_eq!(locals.sent, 0);
            assert_eq!(locals.placeholder, msg(PLACEHOLDER_MSG));
        }
    });
}

#[test]
fn retired_watcher_failed_tail_patch_keeps_the_visible_rollover_anchor_in_cache() {
    if !isolated("retired_watcher_failed_tail_patch_keeps_the_visible_rollover_anchor_in_cache") {
        return;
    }
    let (_lock, guard) = isolate_root();
    let fx = seed_row(guard.root.path(), 63_276, false, false);
    capture_warns(async {
        let shared = six_tool_fixture(crate::config::ToolActivityDisplay::Summary, fx.channel);
        let rec = recorder_with_failed_tail(fx.channel).await;
        let mut locals = tick_locals(&fx, Some(PLACEHOLDER_MSG));
        let body = "x".repeat(2100);
        let budget = build_watcher_single_message_panel_status_block(
            &shared,
            fx.channel,
            &fx.provider,
            START,
            "⠋",
            None,
            None,
            &body,
            None,
        );
        let (plan, split) = {
            let _legacy = footer_retirement::legacy_for_test();
            plan_watcher_streaming_rollover_with_session_banner(
                &shared,
                fx.channel,
                &fx.provider,
                fx.identity.user_msg_id,
                Some(&fx.identity.started_at),
                fx.identity.turn_start_offset,
                0,
                &body,
                &budget,
            )
            .unwrap()
        };
        assert!(
            body.len() - split < split,
            "fixture must have only one rollover"
        );
        run_tick_with_tools(
            &mut locals,
            &rec,
            &shared,
            &fx,
            &body,
            &WatcherToolState::new(),
        )
        .await;
        let contents = rec.bodies.lock().unwrap().clone();
        assert_history_is_body_or_anchor(&contents);
        assert_eq!(
            contents,
            vec![
                plan.frozen_chunk,
                ANCHOR.to_string(),
                body[split..].to_string()
            ]
        );
        assert_eq!(
            locals.edit, ANCHOR,
            "failed PATCH must not cache a rich budget that was never posted"
        );
        assert_eq!(
            locals.sent, split,
            "only the successfully frozen raw prefix advances"
        );
        assert_eq!(locals.placeholder, msg(SERVER_MSG));
        assert_eq!(
            rec.visible
                .lock()
                .unwrap()
                .get(&SERVER_MSG)
                .map(String::as_str),
            Some(ANCHOR)
        );
        let row = load_inflight_state(&fx.provider, fx.channel.get()).unwrap();
        assert_eq!(row.response_sent_offset, split);
        assert_eq!(row.current_msg_id, SERVER_MSG);
    });
}

async fn recorder_with_failed_tail(channel: ChannelId) -> Recorder {
    use axum::body::Bytes;
    use axum::http::{Method, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::{Json, Router, routing::any};
    let calls = Arc::new(Mutex::new(Vec::new()));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let visible = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let (captured_calls, captured_bodies, captured_visible) =
        (calls.clone(), bodies.clone(), visible.clone());
    let patches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = Router::new().fallback(any(move |method: Method, uri: Uri, body: Bytes| {
        let (calls, bodies, visible, patches) = (
            captured_calls.clone(),
            captured_bodies.clone(),
            captured_visible.clone(),
            patches.clone(),
        );
        async move {
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let content = payload["content"].as_str().unwrap_or_default().to_string();
            calls
                .lock()
                .unwrap()
                .push((method.to_string(), uri.to_string()));
            bodies.lock().unwrap().push(content.clone());
            if method == Method::PATCH && patches.fetch_add(1, Ordering::AcqRel) == 1 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "code": 50035, "message": "fixture rejects only the tail PATCH"
                    })),
                )
                    .into_response();
            }
            let id = if method == Method::PATCH {
                uri.path()
                    .rsplit('/')
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap()
            } else {
                SERVER_MSG
            };
            visible.lock().unwrap().insert(id, content.clone());
            Json(serde_json::json!({
                "id": id.to_string(), "channel_id": channel.get().to_string(), "content": content,
                "author": {"id":"1","username":"t","discriminator":"0001","avatar":null},
                "timestamp":"2026-10-19T00:00:00+00:00", "edited_timestamp":null,
                "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                "attachments":[], "embeds":[], "pinned":false, "type":0
            }))
            .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Recorder {
        calls,
        bodies,
        visible,
        http,
        server: server.abort_handle(),
        terminal_gate: Arc::new(tokio::sync::Notify::new()),
        check: Arc::new(std::sync::OnceLock::new()),
    }
}

async fn run_tick_with_tools(
    locals: &mut TickLocals,
    rec: &Recorder,
    shared: &Arc<SharedData>,
    fx: &Fixture,
    body: &str,
    tools: &WatcherToolState,
) -> StreamingStatusTickOutcome {
    locals.last = tokio::time::Instant::now()
        - crate::services::discord::status_update_interval()
        - Duration::from_millis(1);
    let delivered_flag = Arc::new(AtomicBool::new(false));
    let full = body.to_string();
    let ctx = StreamingStatusTickContext {
        http: &rec.http,
        shared,
        channel_id: fx.channel,
        watcher_provider: &fx.provider,
        tmux_session_name: &fx.tmux,
        output_path: &fx.output_path,
        turn_delivered: &delivered_flag,
        host: &HostSnapshot::new(WatchHost::Legacy),
    };
    let turn = StreamingStatusTickTurn {
        data_start_offset: 0,
        current_offset: full.len() as u64,
        full_response: &full,
        tool_state: tools,
        task_notification_kind: None,
        status_panel_started_at: START,
        single_message_panel_footer_mode: false,
        restored_injected_prompt_message_id: None,
    };
    let mut render = StreamingRenderState {
        last_status_update: &mut locals.last,
        spin_idx: &mut locals.spin,
        placeholder_msg_id: &mut locals.placeholder,
        placeholder_from_restored_inflight: &mut locals.restored,
        last_edit_text: &mut locals.edit,
        response_sent_offset: &mut locals.sent,
        watcher_streaming_rollover_frozen_msg_ids: &mut locals.frozen,
    };
    let mut panel = StatusPanelState {
        status_panel_msg_id: &mut locals.panel_id,
        last_status_panel_text: &mut locals.panel_text,
    };
    let mut suppress = StreamingSuppressState {
        turn_is_external_input_for_session: &mut locals.external,
        turn_identity_for_panel: &mut locals.pin,
        streaming_suppressed_by_recent_stop: &mut locals.recent_stop,
        streaming_suppressed_by_missing_inflight: &mut locals.missing,
        active_stream_inflight_reacquire_logged: &mut locals.reacquire,
    };
    let mut generation = PanelGenerationState {
        this_turn_status_panel_generation: &mut locals.generation,
    };
    update_streaming_status_tick(
        &ctx,
        turn,
        &mut render,
        &mut panel,
        &mut suppress,
        &mut generation,
    )
    .await
}
