//! Replay the sanitized native range through the managed reader and the live bridge.

use super::*;
use crate::services::{
    agent_protocol::RuntimeHandoffKind,
    codex_tui::rollout_tail::tail_warm_followup_rollout_for_tmux,
    discord::{
        self, inflight, turn_bridge::bridge_entry_persist::entry_abort_mailbox_tests::seed_context,
    },
    provider::{CancelToken, ReadOutputResult},
    tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding},
};
use serde_json::{Value, json};
use std::{io::Write, sync::mpsc, time::Duration};

const START: u64 = 160_325;
const END: u64 = 169_583;
const PROMPT: &str = "managed empty probe";
const FIXTURE: &str = include_str!("../../../../../../tests/fixtures/codex_empty_turn.jsonl");

fn records() -> Vec<Value> {
    let mut records: Vec<Value> = FIXTURE
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    records[2]["payload"]["internal_chat_message_metadata_passthrough"]["create_time"] =
        json!(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
    records
}

fn event(kind: &str, turn: &str) -> Value {
    json!({"type":"event_msg","payload":{"type":kind,"turn_id":turn,"last_agent_message":null}})
}

fn encode(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .flat_map(|record| {
            let mut line = serde_json::to_vec(record).unwrap();
            line.push(b'\n');
            line
        })
        .collect()
}

fn hook(tmux: &str) {
    crate::services::claude_tui::hook_server::publish_hook_event_for_tests(
        crate::services::claude_tui::hook_server::HookEvent {
            provider: "codex".into(),
            session_id: tmux.into(),
            kind: crate::services::claude_tui::hook_server::HookEventKind::Stop,
            ..Default::default()
        },
    );
}

struct DedupeGuard {
    _held: std::sync::MutexGuard<'static, ()>,
}

fn lock_dedupe() -> DedupeGuard {
    DedupeGuard {
        _held: dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    }
}

struct NativeTail {
    path: std::path::PathBuf,
    token: Arc<CancelToken>,
    messages: Vec<StreamMessage>,
}

fn replay(driver: &TerminalDeliveryDriver, case: &str) -> NativeTail {
    let path = driver._temp.path().join("native-empty.jsonl");
    let token = Arc::new(CancelToken::new());
    let tmux = DRIVER_TMUX_SESSION;
    let _handoff = dedupe::reserve_discord_handoff("codex", tmux, PROMPT);
    let mut rows = records();
    let complete = rows.pop().unwrap();
    match case {
        "previous" | "previous_nonempty" => {
            rows[0] = event("task_started", "previous-turn");
            if case == "previous_nonempty" {
                rows[7]["payload"]["content"][0]["text"] = json!("current native answer");
            }
        }
        "open_tool" => rows.push(json!({"type":"response_item","payload":{
            "type":"function_call","name":"exec","call_id":"open-tool","arguments":"{}"}})),
        "stale_origin" => {
            rows[2]["payload"]["internal_chat_message_metadata_passthrough"]["create_time"] =
                json!(1.0)
        }
        "late_before_origin" => {
            let mut previous = event("task_complete", "previous-turn");
            previous["payload"]["last_agent_message"] =
                json!("previous answer must not be adopted");
            rows.insert(2, previous);
        }
        "commentary_only" => {
            let mut commentary = rows[7].clone();
            commentary["payload"]["id"] = json!("commentary-answer");
            commentary["payload"]["phase"] = json!("commentary");
            commentary["payload"]["content"][0]["text"] = json!("current commentary");
            rows.insert(7, commentary);
        }
        _ => {}
    }
    let mut prefix = encode(&[json!({"type":"response_item","payload":{
        "type":"message","role":"assistant","content":[{"type":"output_text","text":"previous answer must stay outside"}]}})]);
    prefix.resize(START as usize, b'\n');
    let head = encode(&rows);
    prefix.extend(&head);
    std::fs::write(&path, prefix).unwrap();
    dedupe::register_tmux_runtime_binding(
        tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("empty-session".into()),
            last_offset: START,
            relay_last_offset: None,
        },
    );
    let generation = crate::services::tmux_common::session_temp_path(tmux, "generation");
    std::fs::write(generation, "isolated-generation").unwrap();
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (tail_token, tail_path) = (token.clone(), path.clone());
    let thread = std::thread::spawn(move || {
        tail_warm_followup_rollout_for_tmux(
            &tail_path,
            START,
            "empty-session",
            tx,
            Some(tail_token),
            move || {
                let _ = ready_tx.send(());
                true
            },
            tmux,
            PROMPT,
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("managed tail reaches native EOF");
    if !matches!(case, "hook_after" | "previous_nonempty") {
        hook(tmux);
    }
    if !matches!(case, "hook_only" | "late_before_origin") {
        let terminal = if case == "foreign" {
            event("task_complete", "foreign-turn")
        } else {
            complete
        };
        let mut bytes = encode(&[terminal]);
        let padding = END as usize - START as usize - head.len() - bytes.len();
        bytes.splice(0..0, std::iter::repeat_n(b' ', padding));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&bytes).unwrap();
    }
    let negative = matches!(
        case,
        "previous" | "foreign" | "open_tool" | "hook_only" | "stale_origin" | "late_before_origin"
    );
    let mut messages = Vec::new();
    let deadline =
        std::time::Instant::now() + Duration::from_millis(if negative { 400 } else { 2000 });
    while std::time::Instant::now() < deadline {
        if let Ok(message) = rx.recv_timeout(Duration::from_millis(20)) {
            let terminal = matches!(
                message,
                StreamMessage::Done { .. } | StreamMessage::CodexTuiTerminalDone { .. }
            );
            messages.push(message);
            if terminal {
                break;
            }
        }
    }
    if !messages.iter().any(|message| {
        matches!(
            message,
            StreamMessage::Done { .. } | StreamMessage::CodexTuiTerminalDone { .. }
        )
    }) {
        token.publish_cancel("isolated tail finished");
    }
    let result = thread.join().unwrap().unwrap();
    messages.extend(rx.try_iter());
    if negative {
        assert!(
            matches!(result.read_result, ReadOutputResult::Cancelled { .. }),
            "{case}: foreign or incomplete turn completed"
        );
        assert!(
            !messages.iter().any(|m| matches!(
                m,
                StreamMessage::Done { .. } | StreamMessage::CodexTuiTerminalDone { .. }
            )),
            "{case}: unverified terminal {messages:?}"
        );
    } else if case == "previous_nonempty" {
        assert!(
            matches!(
                result.read_result,
                ReadOutputResult::Completed { offset: END }
            ),
            "previous_nonempty: managed nonempty completion finishes promptly: {messages:?}"
        );
        assert_eq!(
            messages.iter().filter(|m| matches!(m, StreamMessage::Done { result, .. } if result == "current native answer")).count(),
            1,
            "previous_nonempty: one Done retains the current answer"
        );
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(
                    m,
                    StreamMessage::Done { .. } | StreamMessage::CodexTuiTerminalDone { .. }
                ))
                .count(),
            1,
            "previous_nonempty: exactly one terminal"
        );
        assert!(messages.iter().all(|m| !matches!(m, StreamMessage::Text { content } if content != "current native answer")), "previous_nonempty: no previous or synthesized text");
    } else {
        assert!(
            matches!(
                result.read_result,
                ReadOutputResult::Completed { offset: END }
            ),
            "{case}: verified empty completion did not finish promptly: {messages:?}"
        );
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, StreamMessage::CodexTuiTerminalDone { .. }))
                .count(),
            1,
            "{case}: typed terminal exactly once: {messages:?}"
        );
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Done { .. })),
            "{case}: no plain Done"
        );
        let expected = if case == "commentary_only" {
            "current commentary"
        } else {
            ""
        };
        assert!(messages.iter().any(|m| matches!(m, StreamMessage::CodexTuiTerminalDone { result, .. } if result == expected)), "{case}: only this turn's body is retained");
        assert!(
            messages
                .iter()
                .all(|m| !matches!(m, StreamMessage::Text { content } if content != expected)),
            "{case}: no previous or synthesized text"
        );
    }
    if case == "hook_after" {
        hook(tmux);
    }
    NativeTail {
        path,
        token,
        messages,
    }
}

#[tokio::test]
async fn managed_empty_native_turn_rejects_unrelated_completion_and_open_tools() {
    for case in [
        "previous",
        "foreign",
        "hook_only",
        "open_tool",
        "stale_origin",
        "late_before_origin",
        "previous_nonempty",
    ] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        let _state = lock_dedupe();
        replay(&driver, case);
    }
}

fn row(driver: &TerminalDeliveryDriver, native: &NativeTail) -> InflightTurnState {
    inflight::clear_inflight_state(&ProviderKind::Claude, DRIVER_CHANNEL_ID);
    let mut row = driver.inflight.clone();
    row.provider = "codex".into();
    row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    row.turn_nonce = native.token.turn_nonce().map(str::to_owned);
    row.turn_start_offset = Some(START);
    row.last_offset = START;
    row.output_path = Some(native.path.display().to_string());
    row.session_id = Some("empty-session".into());
    row.full_response.clear();
    inflight::save_inflight_state(&row).unwrap();
    row
}

async fn drive_bridge(
    driver: &TerminalDeliveryDriver,
    token: Arc<CancelToken>,
    row: InflightTurnState,
    messages: Vec<StreamMessage>,
) -> discord::turn_bridge::BridgeCompletionSignal {
    let channel = ChannelId::new(DRIVER_CHANNEL_ID);
    driver.shared.tmux_watchers.remove(&channel);
    assert!(
        discord::mailbox_try_start_turn(
            &driver.shared,
            channel,
            token.clone(),
            UserId::new(1),
            MessageId::new(row.user_msg_id)
        )
        .await
    );
    discord::increment_global_active(&driver.shared, "empty fixture");
    let mut bridge = seed_context("", row);
    bridge.gateway = driver.gateway.clone();
    bridge.current_msg_id = Some(MessageId::new(DRIVER_CURRENT_MSG_ID));
    bridge.user_msg_id = Some(MessageId::new(DRIVER_USER_MSG_ID));
    bridge.tmux_last_offset = Some(END);
    bridge.new_session_id = Some("empty-session".into());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = Some(done_tx);
    let (tx, rx) = mpsc::channel();
    for message in messages {
        tx.send(message).unwrap();
    }
    // Keep the producer open: a disconnected stream must not masquerade as native completion.
    discord::turn_bridge::spawn_turn_bridge(driver.shared.clone(), token.clone(), rx, bridge);
    let signal = tokio::time::timeout(Duration::from_secs(10), done_rx)
        .await
        .expect("managed empty terminal reaches finalizer")
        .unwrap();
    drop(tx);
    signal
}

#[tokio::test]
async fn managed_empty_native_turn_releases_mailbox_through_finalizer_and_shows_notice() {
    for (o, order) in [
        (false, "headless"),
        (false, "hook_before"),
        (false, "commentary_only"),
        (true, "hook_before"),
        (true, "hook_after"),
    ] {
        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        if order == "headless" {
            driver.gateway = Arc::new(crate::services::discord::gateway::HeadlessGateway);
            assert!(!driver.gateway.can_deliver_directly());
        }
        let _state = lock_dedupe();
        let selected = [(DRIVER_CHANNEL_ID, RuntimeHandoffKind::CodexTui)];
        let _o = crate::services::tui_o::cutover::test_override::force_channels(if o {
            &selected
        } else {
            &[]
        });
        let native = replay(&driver, order);
        let footer_posts = if o {
            native_footer(&driver, &native.path).await
        } else {
            Vec::new()
        };
        let row = row(&driver, &native);
        let channel = ChannelId::new(DRIVER_CHANNEL_ID);
        let signal = drive_bridge(&driver, native.token.clone(), row, native.messages).await;
        assert_eq!(
            signal,
            discord::turn_bridge::BridgeCompletionSignal::Finalized,
            "managed empty terminal reaches finalizer: o={o} order={order}"
        );
        let snapshot = discord::mailbox_snapshot(&driver.shared, channel).await;
        assert!(
            snapshot.cancel_token.is_none(),
            "mailbox releases cancel token"
        );
        assert!(
            snapshot.active_turn_nonce.is_none() && snapshot.active_user_message_id.is_none(),
            "mailbox is idle"
        );
        assert!(
            inflight::load_inflight_state(&ProviderKind::Codex, DRIVER_CHANNEL_ID).is_none(),
            "inflight removed"
        );
        if order != "headless" {
            let bodies = driver.published_bodies.lock().unwrap().clone();
            let writes = driver.observations();
            assert_eq!(
                writes
                    .iter()
                    .filter(|write| write.content.contains("응답 내용 없이 턴을 종료"))
                    .count(),
                usize::from(order != "commentary_only"),
                "send, edit and replace show one empty-response notice"
            );
            assert_eq!(
                bodies
                    .iter()
                    .filter(|body| body.contains("응답 내용 없이 턴을 종료"))
                    .count(),
                usize::from(order != "commentary_only"),
                "one visible empty-response notice on o={o}: {bodies:?}"
            );
            assert!(
                !bodies.iter().any(|body| body.contains("previous answer")),
                "never reuse a previous answer"
            );
            if order == "commentary_only" {
                assert!(
                    bodies
                        .iter()
                        .any(|body| body.contains("current commentary")),
                    "current commentary remains visible"
                );
            }
            if o {
                assert_eq!(footer_posts.len(), 1, "O posts the native completion once");
                assert!(footer_posts[0].starts_with("-# ✅"));
                assert!(
                    !writes.iter().any(|write| write.content.contains("-# ✅")),
                    "bridge does not duplicate O's footer"
                );
            }
            assert!(
                driver
                    .observations()
                    .iter()
                    .any(|call| call.call == DriverCall::Replace),
                "placeholder ends on terminal delivery"
            );
        }
        assert!(
            discord::mailbox_try_start_turn(
                &driver.shared,
                channel,
                Arc::new(CancelToken::new()),
                UserId::new(1),
                MessageId::new(DRIVER_USER_MSG_ID + 1)
            )
            .await,
            "next input is accepted"
        );
        discord::mailbox_finish_turn(&driver.shared, &ProviderKind::Codex, channel).await;
    }
}

async fn native_footer(driver: &TerminalDeliveryDriver, path: &std::path::Path) -> Vec<String> {
    use crate::services::tui_o::{
        ownership::OwnershipGate,
        shadow::{CapturedRecord, ShadowProvider},
        store::{Initialized, OStore, StoreConfig},
        writer::{
            AlarmSink, DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm,
            deliver::{ChannelWriter, Step},
            pieces::{SummaryInputs, UnitDeriver},
        },
    };
    #[derive(Default)]
    struct Port(Arc<Mutex<Vec<String>>>);
    impl DiscordPort for Port {
        fn bot_id(&self) -> u64 {
            1
        }
        fn post(
            &self,
            _: u64,
            content: String,
        ) -> impl Future<Output = PostOutcome> + Send + 'static {
            let posted = self.0.clone();
            async move {
                posted.lock().unwrap().push(content.clone());
                PostOutcome::Created(SeenMessage {
                    id: 727_501,
                    author_id: 1,
                    content,
                })
            }
        }
        async fn history_after(&self, _: u64, _: u64) -> Result<Vec<SeenMessage>, String> {
            panic!("confirmed POST needs no history")
        }
        async fn message(&self, _: u64, _: u64) -> Result<Option<SeenMessage>, String> {
            panic!("confirmed POST needs no lookup")
        }
    }
    struct Lease;
    impl DeliveryLease for Lease {
        type Held = ();
        fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
            Some(())
        }
    }
    struct Alarms;
    impl AlarmSink for Alarms {
        fn raise(&self, _: u64, alarm: WriterAlarm) {
            panic!("unexpected O alarm: {alarm:?}")
        }
    }
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, driver._temp.path())
        .unwrap()
        .unwrap();
    let at = chrono::Utc::now();
    store
        .begin_era(&[DRIVER_CHANNEL_ID], at, |channel| {
            Ok(Initialized {
                channel,
                sources: Vec::new(),
                initial_anchor: 1,
                build_digest: "empty-native-test".into(),
                at,
            })
        })
        .unwrap();
    let era = store.read_era().unwrap().unwrap();
    let channel = store
        .open_channel(&era, DRIVER_CHANNEL_ID)
        .unwrap()
        .unwrap();
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let port = Arc::new(Port::default());
    let mut writer = ChannelWriter::new(channel, gate, port.clone(), Lease, Alarms);
    let mut deriver = UnitDeriver::new(DRIVER_CHANNEL_ID, ShadowProvider::Codex).with_turn_summary(
        SummaryInputs {
            turn_marks: Default::default(),
            disposed: Default::default(),
            context_window: None,
        },
    );
    let bytes = std::fs::read(path).unwrap();
    let mut start = START;
    for line in bytes[START as usize..END as usize].split_inclusive(|byte| *byte == b'\n') {
        let end = start + line.len() as u64;
        for piece in deriver.derive(&CapturedRecord {
            start,
            end,
            line: line.to_vec(),
        }) {
            assert_eq!(writer.deliver(&piece).await, Step::Done);
            assert_eq!(
                writer.deliver(&piece).await,
                Step::Done,
                "O replay cannot post twice"
            );
        }
        start = end;
    }
    assert_eq!(start, END);
    assert_eq!(
        writer.store().ledger().next_serial(),
        1,
        "empty native body is excluded; exactly one footer"
    );
    port.0.lock().unwrap().clone()
}

#[tokio::test]
async fn managed_empty_native_turn_admission_rejects_wrong_source_nonce_and_range() {
    for case in ["source", "nonce", "range"] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        let _state = lock_dedupe();
        let native = replay(&driver, "hook_before");
        let mut local = row(&driver, &native);
        if case == "range" {
            local.last_offset = START + 1;
            inflight::save_inflight_state(&local).unwrap();
        }
        let mut terminal = native
            .messages
            .into_iter()
            .find(|m| matches!(m, StreamMessage::CodexTuiTerminalDone { .. }))
            .unwrap();
        if let StreamMessage::CodexTuiTerminalDone {
            rollout_path,
            turn_nonce,
            ..
        } = &mut terminal
        {
            match case {
                "source" => {
                    let other = driver._temp.path().join("wrong-source.jsonl");
                    std::fs::copy(&native.path, &other).unwrap();
                    *rollout_path = other.display().to_string();
                }
                "nonce" => *turn_nonce = "wrong-nonce".into(),
                "range" => {}
                _ => unreachable!(),
            }
        }
        let signal = drive_bridge(&driver, native.token.clone(), local, vec![terminal]).await;
        let snapshot =
            discord::mailbox_snapshot(&driver.shared, ChannelId::new(DRIVER_CHANNEL_ID)).await;
        assert!(
            snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &native.token)),
            "{case}: invalid empty terminal must keep mailbox authority: {signal:?}"
        );
        assert!(
            driver.published_bodies.lock().unwrap().is_empty(),
            "{case}: rejected terminal must not publish"
        );
        assert_eq!(
            inflight::load_inflight_state(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap()
                .last_offset,
            if case == "range" { START + 1 } else { START },
            "{case}: rejected range must not advance durable state"
        );
    }
}
