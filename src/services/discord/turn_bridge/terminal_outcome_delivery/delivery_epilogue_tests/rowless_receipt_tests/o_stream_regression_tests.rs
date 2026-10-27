use super::*;
use crate::services::discord::turn_bridge::{
    STREAM_TEST_EVENTS, TurnBridgeContext, spawn_turn_bridge,
};
use crate::services::discord::{health, mailbox_snapshot, mailbox_try_start_turn};
use crate::services::tui_o::{
    cutover::test_override,
    writer::progress::{ChannelProgress, Publisher},
};
use std::time::Duration;

const CHILD: &str = "ADK_7453_CHILD";

fn isolated(name: &str, owned: bool) -> bool {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let channels = if owned {
        vec![(
            DRIVER_CHANNEL_ID,
            if name.contains("unclear") {
                RuntimeHandoffKind::ClaudeTui
            } else {
                RuntimeHandoffKind::CodexTui
            },
        )]
    } else {
        vec![]
    };
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
        .env(CHILD, name)
        .env(test_override::CHILD_ENV, "1")
        .env(
            test_override::CHANNELS_ENV,
            serde_json::to_string(&channels).unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored;"));
    false
}

fn assistant(text: &str, id: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item", "payload":{"type":"message","id":id,"role":"assistant","content":[{"type":"output_text","text":text}]}})
    )
}

fn producer(path: &std::path::Path) -> Vec<StreamMessage> {
    let (tx, rx) = std::sync::mpsc::channel();
    crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
        path,
        0,
        None,
        tx,
        None,
        || false,
    )
    .unwrap();
    rx.into_iter().collect()
}

async fn wait_saved(body: &str, sent: usize) -> InflightTurnState {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(row) =
                inflight::load_inflight_state(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
            {
                assert!(
                    row.response_sent_offset <= row.full_response.len(),
                    "invalid pair: {}/{}",
                    row.full_response.len(),
                    row.response_sent_offset
                );
                if row.full_response == body && row.response_sent_offset == sent {
                    break row;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stream progress saved before the next frame")
}

fn context(driver: &TerminalDeliveryDriver) -> TurnBridgeContext {
    TurnBridgeContext {
        provider: ProviderKind::Codex,
        gateway: driver.gateway.clone(),
        channel_id: ChannelId::new(DRIVER_CHANNEL_ID),
        user_msg_id: Some(MessageId::new(DRIVER_USER_MSG_ID)),
        user_text_owned: "driver prompt".into(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: Some(MessageId::new(DRIVER_CURRENT_MSG_ID)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: driver.inflight.clone(),
    }
}

async fn watchdog(driver: &TerminalDeliveryDriver, open: bool) {
    let channel = ChannelId::new(DRIVER_CHANNEL_ID);
    let mailbox = driver.shared.mailbox_peek(channel).unwrap();
    if open {
        mailbox
            .age_active_turn_for_test(Duration::from_secs(1800))
            .await;
    }
    let output = driver._temp.path().join("driver.jsonl");
    filetime::set_file_mtime(
        output,
        filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() - 1800, 0),
    )
    .unwrap();
    if let Some(watcher) = driver.shared.tmux_watchers.get(&channel) {
        watcher.last_heartbeat_ts_ms.store(0, Ordering::Relaxed);
    }
    let publisher = Publisher::new(channel.get());
    publisher.publish(ChannelProgress {
        debt: 0,
        unsealed: false,
        serial: 96,
    });
    health::o_witness_backdate_for_tests(channel.get(), 96, 400);
    let mut registry = health::HealthRegistry::new();
    registry.age_for_tests(3600);
    registry
        .register("codex".into(), driver.shared.clone())
        .await;
    let snapshot = registry
        .snapshot_watcher_state_for_provider(&ProviderKind::Codex, channel.get())
        .await;
    assert_eq!(
        health::run_stall_watchdog_pass(&registry, &ProviderKind::Codex).await,
        0
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE source = 'stall_watchdog'")
            .fetch_one(driver.shared.pg_pool.as_ref().expect("PG is required"))
            .await
            .unwrap();
    eprintln!(
        "watchdog open={open} verdict={:?} alive={:?} outbox={count}",
        snapshot.as_ref().map(|s| s.transcript_binding_stall),
        snapshot.as_ref().and_then(|s| s.tmux_session_alive)
    );
    assert_eq!(count, i64::from(open));
    let snapshot = snapshot.expect("registered watcher snapshot");
    assert!(
        driver
            .shared
            .tmux_watchers
            .channel_binding(&channel)
            .is_some()
    );
    assert_eq!(
        snapshot.transcript_binding_stall == "o_drained_turn_open",
        open,
        "{}",
        snapshot.transcript_binding_stall
    );
}

async fn live_case(mut body: String, tool: bool, control: &str) {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_i7453_live",
        "O stream finalization regression",
    )
    .await
    .expect("PG required, never skip");
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool);
    let _rest = crate::services::discord::shared_state::test_rest::recording_mock(
        4_600_000,
        DRIVER_CHANNEL_ID,
    )
    .await;
    let fake = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
        DRIVER_TMUX_SESSION,
    );
    fake.enable_fixture_launch();
    inflight::clear_inflight_state(&ProviderKind::Claude, DRIVER_CHANNEL_ID);
    driver.inflight.provider = "codex".into();
    driver.inflight.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    driver.inflight.turn_start_offset = Some(0);
    driver.inflight.started_at = (chrono::Local::now() - chrono::Duration::minutes(30))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let path = driver._temp.path().join("driver.jsonl");
    let raw = format!(
        "{}{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n",
        if control == "partial5311" {
            format!(
                "{}{}",
                assistant(&body, "message-7453"),
                assistant(&"tail".repeat(1000), "tail-7453")
            )
        } else if control == "nonprefix" {
            format!(
                "{}{}",
                assistant(&body, "message-7453"),
                assistant("replacement 답", "final-7453")
            )
        } else {
            assistant(&body, "message-7453")
        }
    );
    let raw = format!(
        "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"native-7453\"}}}}\n{raw}"
    );
    std::fs::write(&path, &raw).unwrap();
    driver.inflight.output_path = Some(path.display().to_string());
    driver.inflight.last_offset = raw.len() as u64;
    if control == "unclear" {
        driver.inflight.full_response = body[..6].to_string();
        driver.inflight.response_sent_offset = 6;
    }
    inflight::save_inflight_state(&driver.inflight).unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(DRIVER_TMUX_SESSION, "generation"),
        "g",
    )
    .unwrap();
    tui_prompt_dedupe::register_tmux_runtime_binding(
        DRIVER_TMUX_SESSION,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: raw.len() as u64,
            relay_last_offset: None,
        },
    );
    let (posted, ledger) =
        crate::services::tui_o::writer::tests::post_codex_records_for_test(DRIVER_CHANNEL_ID, &raw)
            .await;
    assert!(!posted.is_empty());
    assert!(ledger.contains("posted"));
    assert!(ledger.contains("#turn_end"));
    let footers: Vec<_> = posted
        .iter()
        .filter(|text| text.starts_with("-# "))
        .collect();
    assert_eq!(footers.len(), 1, "the real O writer posts one footer");
    let footer = footers[0];
    if control == "nonprefix" {
        assert!(posted.iter().any(|text| text.contains("replacement 답")));
    }
    assert!(
        posted
            .join("\n")
            .contains(&body.trim()[..body.trim().len().min(100)])
    );
    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(
        driver.inflight.turn_nonce.clone(),
    ));
    assert!(
        mailbox_try_start_turn(
            &driver.shared,
            ChannelId::new(DRIVER_CHANNEL_ID),
            actor.clone(),
            UserId::new(1),
            MessageId::new(DRIVER_USER_MSG_ID)
        )
        .await
    );
    crate::services::discord::increment_global_active(
        &driver.shared,
        "stream regression admission",
    );
    driver
        .shared
        .mailbox_peek(ChannelId::new(DRIVER_CHANNEL_ID))
        .unwrap()
        .age_active_turn_for_test(Duration::from_secs(1800))
        .await;
    STREAM_TEST_EVENTS
        .lock()
        .unwrap()
        .retain(|(ch, _)| *ch != DRIVER_CHANNEL_ID);
    if control == "watchdog" {
        watchdog(&driver, true).await;
        return;
    }
    let mut bridge = context(&driver);
    if control == "unclear" {
        bridge.full_response = driver.inflight.full_response.clone();
        bridge.response_sent_offset = 6;
    }
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = Some(completed_tx);
    let (tx, rx) = std::sync::mpsc::channel();
    let frames = producer(&path);
    assert!(
        frames
            .iter()
            .any(|m| matches!(m, StreamMessage::Done { .. }))
    );
    for message in frames
        .iter()
        .filter(|m| matches!(m, StreamMessage::Text { .. }))
        .take(if control == "empty_stream" {
            0
        } else if control == "nonprefix" || control == "partial5311" {
            1
        } else {
            usize::MAX
        })
    {
        if let StreamMessage::Text { content } = message {
            tx.send(StreamMessage::Text {
                content: if control == "unclear" {
                    content[6..].to_string()
                } else {
                    content.clone()
                },
            })
            .unwrap();
        }
    }
    spawn_turn_bridge(driver.shared.clone(), actor.clone(), rx, bridge);
    let mut saved = wait_saved(
        if control == "empty_stream" { "" } else { &body },
        if control == "empty_stream" {
            0
        } else if control == "unclear" {
            6
        } else {
            body.len()
        },
    )
    .await;
    if control == "partial5311" {
        assert_eq!(saved.response_sent_offset, 5311);
        for message in frames
            .iter()
            .filter(|m| matches!(m, StreamMessage::Text { .. }))
            .skip(1)
        {
            if let StreamMessage::Text { content } = message {
                tx.send(StreamMessage::Text {
                    content: content.clone(),
                })
                .unwrap();
            }
        }
        body = frames
            .iter()
            .find_map(|m| {
                if let StreamMessage::Done { result, .. } = m {
                    Some(result.clone())
                } else {
                    None
                }
            })
            .unwrap();
    }
    if control == "foreign" {
        saved.turn_nonce = Some("another-owner".into());
        saved.user_msg_id += 1;
        inflight::save_inflight_state(&saved).unwrap();
    }
    if tool {
        tx.send(StreamMessage::ToolUse {
            name: "exec_command".into(),
            input: r#"{"cmd":"true"}"#.into(),
            tool_use_id: Some("call-7453".into()),
        })
        .unwrap();
        let tool_body = if body.ends_with("\n\n") {
            body.clone()
        } else {
            format!("{body}\n\n")
        };
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let row = wait_saved(&tool_body, tool_body.len()).await;
                if row.any_tool_used {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ToolUse saved before Done");
        tx.send(StreamMessage::ToolResult {
            content: "ok".into(),
            is_error: false,
            tool_use_id: Some("call-7453".into()),
        })
        .unwrap();
    }
    if control == "cancel" {
        actor.cancelled.store(true, Ordering::Relaxed);
    }
    for message in frames {
        if let StreamMessage::Done { result, session_id } = message {
            let message = if control == "native"
                || control == "nonprefix"
                || control == "empty_stream"
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = std::fs::metadata(&path).unwrap();
                StreamMessage::CodexTuiTerminalDone {
                    result: if control == "nonprefix" {
                        "replacement 답".into()
                    } else {
                        result
                    },
                    session_id,
                    rollout_path: path.display().to_string(),
                    tmux_session_name: DRIVER_TMUX_SESSION.into(),
                    turn_nonce: actor.turn_nonce().unwrap().into(),
                    source_start: 0,
                    complete_record_end: metadata.len(),
                    captured_source: Some(
                        crate::services::agent_protocol::CapturedTuiTerminalSource {
                            generation_mtime_ns:
                                crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(
                                    DRIVER_TMUX_SESSION,
                                ),
                            source_file_dev: metadata.dev(),
                            source_file_ino: metadata.ino(),
                            actor: Arc::downgrade(&actor),
                        },
                    ),
                    kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
                }
            } else {
                StreamMessage::Done { result, session_id }
            };
            tx.send(message).unwrap();
        }
    }
    drop(tx);
    let signal = tokio::time::timeout(Duration::from_secs(12), completed_rx)
        .await
        .expect("bridge completion")
        .expect("signal");
    assert!(
        driver
            .published_bodies
            .lock()
            .unwrap()
            .iter()
            .all(|text| !text.contains(footer)),
        "bridge duplicated O's footer"
    );
    let events = STREAM_TEST_EVENTS
        .lock()
        .unwrap()
        .iter()
        .filter(|(ch, _)| *ch == DRIVER_CHANNEL_ID)
        .map(|(_, event)| event.clone())
        .collect::<Vec<_>>();
    if control == "foreign" {
        assert_ne!(signal, BridgeCompletionSignal::Finalized);
        assert_eq!(events.iter().filter(|e| *e == "finalize").count(), 0);
        assert_eq!(
            inflight::load_inflight_state(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap()
                .turn_nonce,
            Some("another-owner".into())
        );
        return;
    }
    if control == "cancel" {
        assert_eq!(signal, BridgeCompletionSignal::Unresolved);
        assert_eq!(events.iter().filter(|e| *e == "finalize").count(), 1);
        assert!(events.contains(&"cancel-finalize".to_string()));
        assert!(
            inflight::load_inflight_state(&ProviderKind::Codex, DRIVER_CHANNEL_ID).is_none(),
            "cancel leaves a row"
        );
        assert!(
            mailbox_snapshot(&driver.shared, ChannelId::new(DRIVER_CHANNEL_ID))
                .await
                .cancel_token
                .is_none(),
            "cancel leaves an active mailbox"
        );
        assert!(
            driver
                .published_bodies
                .lock()
                .unwrap()
                .iter()
                .all(|text| !text.contains(body.trim()))
        );
        watchdog(&driver, false).await;
        return;
    }
    if control == "native" || control == "nonprefix" || control == "empty_stream" {
        assert!(
            events.contains(&"admission-peek=Ok(true)".to_string()),
            "{events:?}"
        );
    }
    if control == "unclear" {
        assert_ne!(signal, BridgeCompletionSignal::Finalized);
        assert_eq!(events.iter().filter(|e| *e == "finalize").count(), 1);
        assert_eq!(events.iter().filter(|e| *e == "replay").count(), 0);
        let retained =
            inflight::load_inflight_state(&ProviderKind::Codex, DRIVER_CHANNEL_ID).unwrap();
        assert_eq!(retained.response_sent_offset, 6);
        assert!(!retained.terminal_delivery_committed);
        assert!(
            driver
                .published_bodies
                .lock()
                .unwrap()
                .iter()
                .all(|b| !b.contains("long answer"))
        );
        return;
    }
    let expected_body = if tool && !body.ends_with("\n\n") {
        format!("{body}\n\n")
    } else {
        body.clone()
    };
    assert!(
        events.contains(&format!(
            "done-body={expected_body:?}/{}",
            if control == "empty_stream" {
                0
            } else if control == "partial5311" {
                5311
            } else {
                expected_body.len()
            }
        )),
        "Done changed the stream representation: {events:?}"
    );
    if control == "native" || control == "nonprefix" || control == "empty_stream" {
        assert!(
            events.contains(&format!(
                "admission-body={:?}/{}",
                if control == "empty_stream" {
                    ""
                } else {
                    &expected_body
                },
                if control == "empty_stream" {
                    0
                } else {
                    expected_body.len()
                }
            )),
            "admission changed the stream representation: {events:?}"
        );
    }
    assert_eq!(signal, BridgeCompletionSignal::Finalized, "{events:?}");
    assert_eq!(events.iter().filter(|e| *e == "finalize").count(), 1);
    assert_eq!(
        events.iter().any(|e| e == "cancel-finalize"),
        control == "cancel"
    );
    assert_eq!(
        events.iter().filter(|e| *e == "replay").count(),
        0,
        "O replay requests"
    );
    assert!(inflight::load_inflight_state(&ProviderKind::Codex, DRIVER_CHANNEL_ID).is_none());
    assert!(
        mailbox_snapshot(&driver.shared, ChannelId::new(DRIVER_CHANNEL_ID))
            .await
            .cancel_token
            .is_none()
    );
    assert!(
        !driver
            .published_bodies
            .lock()
            .unwrap()
            .iter()
            .any(|text| text.contains(body.trim())),
        "bridge reposted O's body"
    );
    if control == "nonprefix" {
        assert!(
            driver
                .published_bodies
                .lock()
                .unwrap()
                .iter()
                .all(|text| !text.contains("replacement 답"))
        );
    }
    assert_eq!(
        driver.shared.restart.global_active.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        driver
            .shared
            .restart
            .finalizing_turns
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        driver
            .shared
            .restart
            .global_finalizing
            .load(Ordering::Relaxed),
        0
    );
    watchdog(&driver, false).await;
}

#[tokio::test]
async fn i7453_o_long_done_finishes_once_without_replay_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_long_done_finishes_once_without_replay_pg"
    );
    if isolated(name, true) {
        live_case("long answer ".repeat(900), false, "normal").await;
    }
}

#[tokio::test]
async fn i7453_o_tool_boundary_preserves_consumed_whitespace_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_tool_boundary_preserves_consumed_whitespace_pg"
    );
    if isolated(name, true) {
        live_case("answer\n\n\n\n".into(), true, "normal").await;
    }
}

#[tokio::test]
async fn i7453_o_watchdog_open_mailbox_pages_pg() {
    let name = concat!(module_path!(), "::i7453_o_watchdog_open_mailbox_pages_pg");
    if isolated(name, true) {
        live_case("answer".into(), false, "watchdog").await;
    }
}

#[tokio::test]
async fn i7453_foreign_owner_stops_before_finalize_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_foreign_owner_stops_before_finalize_pg"
    );
    if isolated(name, true) {
        live_case("answer".into(), false, "foreign").await;
    }
}

#[tokio::test]
async fn i7453_cancel_still_finalizes_pg() {
    let name = concat!(module_path!(), "::i7453_cancel_still_finalizes_pg");
    if isolated(name, true) {
        live_case("answer".into(), true, "cancel").await;
    }
}

#[tokio::test]
async fn i7453_o_native_short_done_finishes_once_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_native_short_done_finishes_once_pg"
    );
    if isolated(name, true) {
        live_case("answer".into(), true, "native").await;
    }
}
#[tokio::test]
async fn i7453_o_plain_short_done_preserves_frontier_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_plain_short_done_preserves_frontier_pg"
    );
    if isolated(name, true) {
        live_case("answer".into(), true, "normal").await;
    }
}

#[tokio::test]
async fn i7453_unclear_long_done_holds_without_replay_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_unclear_long_done_holds_without_replay_pg"
    );
    if isolated(name, true) {
        live_case("long answer ".repeat(900), false, "unclear").await;
    }
}
#[tokio::test]
async fn i7453_o_nonprefix_native_answer_is_posted_only_by_o_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_nonprefix_native_answer_is_posted_only_by_o_pg"
    );
    if isolated(name, true) {
        live_case("answer".into(), true, "nonprefix").await;
    }
}

#[tokio::test]
async fn i7453_o_done_after_saved_frontier_5311_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_done_after_saved_frontier_5311_pg"
    );
    if isolated(name, true) {
        live_case("a".repeat(5311), false, "partial5311").await;
    }
}
#[tokio::test]
async fn i7453_o_unicode_frontier_survives_tool_boundary_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_unicode_frontier_survives_tool_boundary_pg"
    );
    if isolated(name, true) {
        live_case("한글 답\n\n\n\n".into(), true, "normal").await;
    }
}

#[tokio::test]
async fn i7453_o_native_done_extends_empty_stream_pg() {
    let name = concat!(
        module_path!(),
        "::i7453_o_native_done_extends_empty_stream_pg"
    );
    if isolated(name, true) {
        live_case("answer".into(), false, "empty_stream").await;
    }
}
