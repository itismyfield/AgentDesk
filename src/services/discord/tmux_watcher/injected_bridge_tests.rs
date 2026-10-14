use super::*;
use crate::services::agent_protocol::{NativeTerminalKind, RuntimeHandoffKind, StreamMessage};
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
use crate::services::discord::turn_bridge::{
    BridgeCompletionSignal, TurnBridgeContext, spawn_turn_bridge,
};
use crate::services::provider::CancelToken;
use poise::serenity_prelude::{MessageId, UserId};

#[derive(Default)]
struct Gateway(Mutex<Vec<&'static str>>);
impl TurnGateway for Gateway {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("send");
            Ok(MessageId::new(77_001))
        })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("edit");
            Ok(())
        })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("replace");
            Ok(ReplaceLongMessageOutcome::EditedOriginal)
        })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("retry");
        })
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a crate::services::discord::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
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
        None
    }
}

fn child(test: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return isolated_in("injected_watcher_tests::injected_bridge_tests", test, &[]);
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let tmux = root.path().join("tmux");
    std::fs::write(
        &tmux,
        FAKE_TMUX.replace("ROOT", &root.path().display().to_string()),
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.path().join("pane"), "busy\n").unwrap();
    let exact = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("PATH", root.path())
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .output()
        .unwrap();
    eprintln!(
        "BRIDGE_CHILD exact={exact} args=--exact,--nocapture\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed; 0 failed"),
        "actual bridge child failed"
    );
    false
}

async fn bridge_case(
    index: u64,
    provider: ProviderKind,
    mode: &str,
    receipt: bool,
) -> Vec<&'static str> {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().pg_pool = Some(pool.clone());
    Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
    let channel = ChannelId::new(600_830_000 + index);
    let pane = provider.build_tmux_session_name(&format!("injected-bridge-{index}"));
    let path = crate::config::runtime_root()
        .unwrap()
        .join(format!("bridge-{index}.jsonl"));
    std::fs::write(&path, []).unwrap();
    let generation_path = crate::services::tmux_common::session_temp_path(&pane, "generation");
    std::fs::create_dir_all(std::path::Path::new(&generation_path).parent().unwrap()).unwrap();
    std::fs::write(generation_path, "fixture").unwrap();
    let runtime = if provider == CLAUDE {
        RuntimeHandoffKind::ClaudeTui
    } else {
        RuntimeHandoffKind::CodexTui
    };
    crate::services::tmux_common::write_tmux_runtime_kind_marker(&pane, runtime).unwrap();
    register_tmux_runtime_binding(
        &pane,
        TuiRuntimeBinding {
            runtime_kind: runtime,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(SESSION.into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "B-owner".into(),
    )));
    let id = if receipt {
        Some(
            c::attach(
                &shared,
                c::Input {
                    channel: channel.get(),
                    pane: &pane,
                    path: path.to_str().unwrap(),
                    owner_nonce: "B-owner",
                    session: SESSION,
                    text: PROMPT,
                    frontier: 0,
                },
            )
            .await,
        )
    } else {
        None
    };
    let user = if mode == "wrong_nonce" {
        record("wrong_nonce")
    } else if mode == "enqueue" {
        record("enqueue")
    } else if mode == "wrong_hash" {
        record("wrong_hash")
    } else {
        record("good")
    };
    let source = if provider == CLAUDE {
        format!(
            "{user}{{\"type\":\"result\",\"result\":\"C result\",\"session_id\":\"B-session\"}}\n"
        )
    } else {
        format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":{}}}]}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"turn-B\"}}}}\n",
            serde_json::to_string(PROMPT).unwrap()
        )
    };
    std::fs::write(&path, &source).unwrap();
    let SourceFileIdentity::Unix { dev, ino } =
        crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
            &std::fs::File::open(&path).unwrap(),
        )
    else {
        panic!("fixture file");
    };
    let generation = dr::current_generation_mtime_ns(&pane);
    let mut row = InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        77,
        0,
        "B original body".into(),
        Some(SESSION.into()),
        Some(pane.clone()),
        Some(path.display().to_string()),
        None,
        0,
    );
    row.runtime_kind = Some(runtime);
    row.turn_nonce = Some("B-owner".into());
    row.turn_start_offset = Some(0);
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &shared,
            channel,
            actor.clone(),
            UserId::new(1),
            MessageId::new(77)
        )
        .await
    );
    save_inflight_state(&row).unwrap();
    let frame_actor = if mode == "wrong_actor" {
        Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "B-owner".into(),
        )))
    } else {
        actor.clone()
    };
    let session = Some(if mode == "wrong_session" {
        "foreign".into()
    } else {
        SESSION.into()
    });
    let nonce = if mode == "wrong_owner" {
        "foreign-owner"
    } else {
        "B-owner"
    }
    .to_string();
    let mut frame = if provider == CLAUDE {
        StreamMessage::ClaudeTuiTerminalDone {
            result: "C result".into(),
            session_id: session,
            transcript_path: path.display().to_string(),
            tmux_session_name: pane.clone(),
            turn_nonce: nonce,
            source_start: 0,
            complete_record_end: source.len() as u64,
            generation_mtime_ns: generation,
            source_file_dev: dev,
            source_file_ino: ino,
            actor: Arc::downgrade(&frame_actor),
            kind: NativeTerminalKind::Completed,
        }
    } else {
        StreamMessage::CodexTuiTerminalDone {
            result: "C result".into(),
            session_id: session,
            rollout_path: path.display().to_string(),
            tmux_session_name: pane.clone(),
            turn_nonce: nonce,
            source_start: 0,
            complete_record_end: source.len() as u64,
            captured_source: Some(crate::services::agent_protocol::CapturedTuiTerminalSource {
                generation_mtime_ns: generation,
                source_file_dev: dev,
                source_file_ino: ino,
                actor: Arc::downgrade(&frame_actor),
            }),
            kind: NativeTerminalKind::Completed,
        }
    };
    if mode == "plain_done" {
        frame = StreamMessage::Done {
            result: "C result".into(),
            session_id: Some(SESSION.into()),
        };
    }
    if mode == "wrong_source" {
        std::fs::rename(&path, path.with_extension("old")).unwrap();
        std::fs::write(&path, &source).unwrap();
    }
    if mode == "cas_miss" {
        c::withhold(channel.get(), id.unwrap(), &pool).await;
    }
    if mode == "ack_lost" {
        c::lose_ack(channel.get());
    }
    if mode == "wrong_attempt" {
        let other = c::attach(
            &shared,
            c::Input {
                channel: channel.get(),
                pane: &pane,
                path: &path.display().to_string(),
                owner_nonce: "B-owner",
                session: SESSION,
                text: "C-nonce other",
                frontier: 0,
            },
        )
        .await;
        c::wrong_attempt(channel.get(), id.unwrap(), other);
    }
    let gateway = Arc::new(Gateway::default());
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let context = TurnBridgeContext {
        provider,
        gateway: gateway.clone(),
        channel_id: channel,
        user_msg_id: Some(MessageId::new(77)),
        user_text_owned: "B original body".into(),
        request_owner_name: "B".into(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: crate::services::memory::TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: None,
        response_sent_offset: 0,
        full_response: "C result".into(),
        tmux_last_offset: None,
        new_session_id: Some(SESSION.into()),
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: false,
        inflight_state: row.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(frame).unwrap();
    drop(tx);
    spawn_turn_bridge(shared.clone(), actor.clone(), rx, context);
    let completion = tokio::time::timeout(Duration::from_secs(15), completion_rx)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        completion,
        BridgeCompletionSignal::EntryAborted,
        "fixture must enter stream loop"
    );
    let trace = c::trace(channel.get());
    if matches!(mode, "cas_miss" | "ack_lost" | "wrong_attempt") {
        assert_eq!(
            trace
                .iter()
                .filter(|event| **event == "classify_attempt")
                .count(),
            1,
            "{trace:?}"
        );
    }
    assert_eq!(
        trace.iter().filter(|event| **event == "normal_ack").count(),
        usize::from(mode == "good" && receipt),
        "actual bridge {mode}: {trace:?}"
    );
    if let Some(id) = id {
        let state = c::state(&pool, id).await;
        assert_eq!(
            state,
            if mode == "good" || mode == "ack_lost" {
                "classified_normal"
            } else if mode == "cas_miss" {
                "withheld"
            } else {
                "started_unclassified"
            }
        );
    } else {
        assert!(trace.is_empty(), "legacy bridge cannot enter C callback");
    }
    assert_eq!(actor.turn_nonce(), Some("B-owner"));
    assert!(
        actor.replay_attempt().is_none(),
        "C never replaces B's live attempt"
    );
    assert!(
        shared.tmux_watchers.len() == 0,
        "C never starts an output owner"
    );
    let calls = gateway.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(!calls.contains(&"retry"));
    eprintln!(
        "BRIDGE case={index} mode={mode} completion={completion:?} trace={trace:?} calls={calls:?}"
    );
    db.drop().await;
    calls
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_bridge_actual_entry_terminal_matrix_pg() {
    if !child("injected_bridge_actual_entry_terminal_matrix_pg") {
        return;
    }
    for (i, mode) in [
        "enqueue",
        "wrong_nonce",
        "wrong_owner",
        "wrong_actor",
        "wrong_session",
        "wrong_source",
        "plain_done",
        "cas_miss",
        "ack_lost",
        "wrong_attempt",
        "wrong_hash",
        "good",
    ]
    .iter()
    .enumerate()
    {
        Box::pin(bridge_case(i as u64, CLAUDE, mode, true)).await;
    }
    Box::pin(bridge_case(50, ProviderKind::Codex, "good", true)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_bridge_without_receipt_main_equivalence_pg() {
    if !child("injected_bridge_without_receipt_main_equivalence_pg") {
        return;
    }
    let calls = Box::pin(bridge_case(100, CLAUDE, "good", false)).await;
    eprintln!("BRIDGE_MAIN_CALLS={calls:?}");
    assert_eq!(calls, ["send", "replace"]);
}
