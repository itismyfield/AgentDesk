//! Owner busy inputs reach stop through admission, intake, the pane's Enter and the text command.
use super::*;
use crate::services::discord::{Data, inflight};
use poise::serenity_prelude::{Message, MessageId, UserId};
use std::io::Write;
use std::sync::atomic::Ordering;

#[path = "owner_input_owned_stop_format_tests.rs"]
mod formats;

fn isolated(name: &str) -> bool {
    creator_child(&format!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::owned_stop::{name}"
    ))
}

struct StopRig {
    intake: IntakeRig,
    session: String,
    sid: String,
    opener_end: u64,
}

impl StopRig {
    async fn new(id: u64, marker: Option<&str>) -> Self {
        let running = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tui_input/claude-2.1.293-tool-running.txt"
        ))
        .lines()
        .filter(|line| !line.contains("Tomfoolering"))
        .collect::<Vec<_>>()
        .join("\n");
        let intake = IntakeRig::busy_with_screen(id, running.clone()).await;
        let session = ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{id}"));
        let binding =
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session).unwrap();
        let sid = binding.session_id.unwrap();
        let opener = json!({"type":"user","uuid":"external-opener","sessionId":sid,
            "message":{"role":"user","content":"external parent"}});
        let opener = format!("{opener}\n");
        let opener_end = opener.len() as u64;
        fs::write(&intake.transcript, opener).unwrap();
        append(
            &intake.transcript,
            json!({"type":"assistant","uuid":"external-assistant",
            "message":{"id":"external-assistant","content":[
            {"type":"tool_use","id":"external-tool","name":"Bash","input":{"command":"sleep 60"}}]},"apiBlockIndex":0}),
        );
        fs::write(
            intake.renderer.join("record-path"),
            intake.transcript.display().to_string(),
        )
        .unwrap();
        fs::write(intake.renderer.join("queued-row"), "").unwrap();
        fs::write(intake.renderer.join("native-sid"), &sid).unwrap();
        fs::write(intake.renderer.join("native-uuid"), "owned-busy-row").unwrap();
        if let Some(marker) = marker {
            fs::write(intake.renderer.join(marker), "").unwrap();
        }
        fs::write(intake.renderer.join("after"), running).unwrap();
        let shim = intake._tmux.root.path().join("tmux");
        let source = fs::read_to_string(&shim).unwrap();
        let calls = intake.renderer.join("tmux-calls");
        fs::write(
            &shim,
            source.replacen(
                "#!/bin/sh\n",
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
                    calls.display()
                ),
                1,
            ),
        )
        .unwrap();
        intake.shared.settings.write().await.owner_user_id = Some(7);
        Self {
            intake,
            session,
            sid,
            opener_end,
        }
    }

    async fn paste(&self) {
        let before = events(&self.intake.renderer).len();
        let recorded = self.intake.renderer.join("queue-recorded");
        if recorded.exists() {
            fs::remove_file(recorded).unwrap();
        }
        let (drive, mut settled) = self.intake.drive();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
        let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
            .await
            .unwrap_or_else(|_| panic!("{}", self.intake.trace.text()))
            .unwrap();
        assert_eq!(result, Ok(()), "{}", self.intake.trace.text());
        assert_eq!(&events(&self.intake.renderer)[before..], ["paste", "Enter"]);
        if self.intake.renderer.join("queue-mode").exists() {
            wait("the pane finishes its absorption records", || {
                self.intake.renderer.join("queue-recorded").exists()
            });
        }
        if self.intake.renderer.join("unabsorbed").exists() {
            wait("the pane records the enqueue", || {
                fs::read_to_string(&self.intake.transcript)
                    .unwrap()
                    .contains("\"operation\": \"enqueue\"")
            });
            let contents = fs::read_to_string(&self.intake.transcript).unwrap();
            let rows: Vec<Value> = contents
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let queued = rows
                .iter()
                .find(|row| row["type"] == "queue-operation")
                .unwrap();
            assert_eq!(
                queued["content"],
                fs::read_to_string(self.intake.renderer.join("body")).unwrap()
            );
            assert!(
                !rows
                    .iter()
                    .any(|row| row["attachment"]["type"] == "queued_command")
            );
        } else {
            wait("the pane records the busy row", || {
                fs::read_to_string(&self.intake.transcript)
                    .unwrap()
                    .contains("owned-busy-row")
            });
        }
        assert!(!self.intake.mailbox.has_active_turn().await.unwrap());
        assert!(self.intake.state().await["head_injection"].is_null());
    }

    fn escapes(&self) -> usize {
        fs::read_to_string(self.intake.renderer.join("tmux-calls"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("send-keys") && line.ends_with("Escape"))
            .count()
    }

    async fn command(&self) {
        command(&self.intake.shared, self.intake.channel).await;
    }

    async fn relay_claim(&self) -> Arc<CancelToken> {
        let pin = crate::services::claude_tui::input::UserReceipt::native_pin_before_tail(
            &self.session,
            "external parent",
            Some(&self.sid),
            self.opener_end,
        )
        .expect("the relay pins the external opener");
        assert!(pin.direct);
        self.claim_pin(pin).await
    }

    async fn own_user_claim(&self) -> Arc<CancelToken> {
        let body = fs::read_to_string(self.intake.renderer.join("body")).unwrap();
        let pin = crate::services::claude_tui::input::UserReceipt::native_pin_before_tail(
            &self.session,
            &body,
            Some(&self.sid),
            fs::metadata(&self.intake.transcript).unwrap().len(),
        )
        .expect("the actual Enter's user row pins its owner's native root");
        assert_eq!(pin.uuid, "owned-busy-row");
        self.claim_pin(pin).await
    }

    async fn claim_pin(
        &self,
        pin: crate::services::provider::native_stop::NativeUserPin,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        token.bind_claude_tmux_session(&self.session);
        token.native_stop.receipt_sink().adopt(pin);
        assert!(
            self.intake
                .mailbox
                .try_start_turn(
                    token.clone(),
                    UserId::new(7),
                    MessageId::new(self.intake.channel.get() + 1)
                )
                .await
        );
        let mut row = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            self.intake.channel.get(),
            None,
            7,
            self.intake.channel.get() + 1,
            self.intake.channel.get() + 2,
            "relay parent".into(),
            Some(self.sid.clone()),
            Some(self.session.clone()),
            None,
            None,
            0,
        );
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        row.output_path = Some(self.intake.transcript.display().to_string());
        inflight::save_inflight_state(&row).unwrap();
        token
    }
}

fn append(path: &Path, row: Value) {
    writeln!(
        fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{row}"
    )
    .unwrap();
}

async fn command(shared: &Arc<SharedData>, channel: ChannelId) {
    let ctx = shared.http.cached_serenity_ctx.get().unwrap();
    let data = Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
    };
    let mut message = Message::default();
    message.id = MessageId::new(channel.get() + 900);
    message.channel_id = channel;
    message.author.id = UserId::new(7);
    message.content = "!stop".into();
    assert!(
        Box::pin(discord::commands::handle_text_command(
            ctx, &message, &data, channel, "!stop"
        ))
        .await
        .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_busy_absorbed_without_a_token_stops_the_external_turn() {
    if !isolated("owner_busy_absorbed_without_a_token_stops_the_external_turn") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_101, None).await;
    rig.paste().await;
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        1,
        "absorbed owner input authorizes its open turn"
    );
    assert!(!rig.intake.mailbox.has_active_turn().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_busy_absorbed_into_an_assistant_opened_turn_stops_it() {
    if !isolated("owner_busy_absorbed_into_an_assistant_opened_turn_stops_it") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_111, None).await;
    fs::write(
        &rig.intake.transcript,
        format!("{}\n", json!({"type":"system","subtype":"turn_duration"})),
    )
    .unwrap();
    append(
        &rig.intake.transcript,
        json!({"type":"assistant","uuid":"autonomous-opener",
        "sessionId":rig.sid,"apiBlockIndex":0,
        "message":{"id":"autonomous-opener","role":"assistant","content":[
            {"type":"tool_use","id":"autonomous-tool","name":"Bash","input":{"command":"sleep 60"}}]}}),
    );
    rig.paste().await;
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        1,
        "an absorbed owner input also authorizes an assistant-opened turn"
    );
    assert!(!rig.intake.mailbox.has_active_turn().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_busy_absorbed_before_relay_claim_remains_a_child() {
    if !isolated("owner_busy_absorbed_before_relay_claim_remains_a_child") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_103, None).await;
    rig.paste().await;
    let token = rig.relay_claim().await;
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        1,
        "the ledger keeps an input pasted before the claim"
    );
    assert!(
        !token.cancelled.load(Ordering::Acquire),
        "native stop never raw cancels"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restored_root_without_durable_children_refuses_an_absorbed_owner_input() {
    if !isolated("a_restored_root_without_durable_children_refuses_an_absorbed_owner_input") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_150, None).await;
    rig.paste().await;
    let live = rig.relay_claim().await;
    let body = fs::read_to_string(rig.intake.renderer.join("body")).unwrap();
    let history = crate::services::tui_input::human_frame::history::row_owner(
        "claude",
        &rig.session,
        &[body],
        "owned-busy-row",
    );
    let crate::services::tui_input::human_frame::history::RowOwner::Command(attempt) = history
    else {
        panic!("the actual Enter's absorbed command belongs to its admission attempt");
    };
    assert!(!live.native_stop.is_busy_child(&attempt));
    let row =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, rig.intake.channel.get())
            .unwrap();
    inflight::bind_native_stop_pin(&row, &live);
    let persisted =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, rig.intake.channel.get())
            .unwrap();
    assert!(persisted.native_stop_user_pin.is_some());
    assert!(persisted.native_stop_busy_children.is_none());
    let encoded = serde_json::to_value(&persisted).unwrap();
    assert_eq!(
        encoded["native_stop_user_pin"]["input"]["uuid"],
        "external-opener"
    );
    assert!(encoded["native_stop_busy_children"].is_null());

    let shared = discord::make_shared_data_for_tests();
    discord::host_defer_gate::tests::map_channel(
        &shared,
        rig.intake.channel,
        "a1-rollback-7755150",
    )
    .await;
    shared.settings.write().await.owner_user_id = Some(7);
    shared
        .http
        .cached_serenity_ctx
        .set(
            rig.intake
                .shared
                .http
                .cached_serenity_ctx
                .get()
                .unwrap()
                .clone(),
        )
        .unwrap();
    let recovered = Arc::new(CancelToken::from_persisted_turn_nonce(
        persisted.turn_nonce.clone(),
    ));
    assert!(!Arc::ptr_eq(&live, &recovered));
    assert_eq!(recovered.turn_nonce(), persisted.turn_nonce.as_deref());
    crate::services::discord::turn_bridge::bind_cancel_token_tmux_runtime(
        &ProviderKind::Claude,
        &recovered,
        &rig.session,
        "recovery kickoff",
    );
    inflight::restore_native_stop_pin(&persisted, &recovered);
    assert!(recovered.native_stop.receipt_sink().observed().is_none());
    assert!(!recovered.native_stop.is_busy_child(&attempt));
    let mailbox = shared.mailbox(rig.intake.channel);
    assert!(
        mailbox
            .try_start_turn(
                recovered.clone(),
                UserId::new(persisted.request_owner_user_id),
                MessageId::new(persisted.user_msg_id),
            )
            .await
    );
    eprintln!(
        "H1_RESTORED_PROOF actual_busy_enter=true root_persisted=true children=none new_token=true pin_candidate_unobserved=true"
    );
    command(&shared, rig.intake.channel).await;
    assert_eq!(
        rig.escapes(),
        0,
        "a restored root cannot borrow an absorbed attempt absent from its durable children"
    );
    assert!(!recovered.cancelled.load(Ordering::Acquire));
    assert!(!live.cancelled.load(Ordering::Acquire));
    assert!(
        rig.intake
            .mock
            .written_texts()
            .iter()
            .any(|text| text.contains("중단 미확인")),
        "the refused restored stop uses the existing unconfirmed reply"
    );
    assert!(mailbox.has_active_turn().await.unwrap());
    assert!(
        mailbox
            .cancel_token()
            .await
            .unwrap()
            .is_some_and(|token| Arc::ptr_eq(&token, &recovered))
    );
    assert_eq!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&rig.intake.transcript),
        crate::services::tui_turn_state::TuiTurnState::Streaming,
        "a refused stop leaves the same native turn running"
    );
    assert!(recovered.native_stop.receipt_sink().observed().is_some());
    command(&shared, rig.intake.channel).await;
    assert_eq!(
        rig.escapes(),
        0,
        "a second stop keeps the restored child policy"
    );
    assert!(mailbox.has_active_turn().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_claimed_external_turn_keeps_its_existing_stop_authority() {
    if !isolated("relay_claimed_external_turn_keeps_its_existing_stop_authority") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_001, None).await;
    let token = rig.relay_claim().await;
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        1,
        "a direct relay pin keeps its existing authority"
    );
    assert!(!token.cancelled.load(Ordering::Acquire));
}

async fn rejected(name: &str, id: u64, marker: &str) {
    if !isolated(name) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(id, Some(marker)).await;
    rig.paste().await;
    rig.command().await;
    assert_eq!(rig.escapes(), 0, "{marker} gives no Escape authority");
    assert!(!rig.intake.mailbox.has_active_turn().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_input_between_the_opener_and_owner_attempt_blocks_stop() {
    rejected(
        "foreign_input_between_the_opener_and_owner_attempt_blocks_stop",
        7_755_201,
        "foreign-before",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_input_after_the_owner_attempt_blocks_stop() {
    rejected(
        "foreign_input_after_the_owner_attempt_blocks_stop",
        7_755_202,
        "foreign-after",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_input_on_the_owner_attempt_row_blocks_stop() {
    rejected(
        "foreign_input_on_the_owner_attempt_row_blocks_stop",
        7_755_203,
        "foreign-same",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enqueued_but_unabsorbed_owner_attempt_gives_no_stop_authority() {
    rejected(
        "an_enqueued_but_unabsorbed_owner_attempt_gives_no_stop_authority",
        7_755_204,
        "unabsorbed",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_external_turn_does_not_inherit_the_prior_owner_attempt() {
    if !isolated("a_successor_external_turn_does_not_inherit_the_prior_owner_attempt") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_104, None).await;
    rig.paste().await;
    append(
        &rig.intake.transcript,
        json!({"type":"system","subtype":"turn_duration"}),
    );
    append(
        &rig.intake.transcript,
        json!({"type":"user","uuid":"external-successor",
        "sessionId":rig.sid,"message":{"role":"user","content":"a new external turn"}}),
    );
    rig.command().await;
    assert_eq!(rig.escapes(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_stop_after_the_turn_closes_sends_no_second_escape() {
    if !isolated("repeated_stop_after_the_turn_closes_sends_no_second_escape") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_107, None).await;
    rig.paste().await;
    rig.command().await;
    assert_eq!(rig.escapes(), 1);
    append(
        &rig.intake.transcript,
        json!({"type":"system","subtype":"turn_duration"}),
    );
    rig.command().await;
    assert_eq!(
        rig.escapes(),
        1,
        "closed turns never receive a second Escape"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confirmed_channel_keeps_session_stop_for_an_unabsorbed_attempt() {
    if !isolated("a_confirmed_channel_keeps_session_stop_for_an_unabsorbed_attempt") {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_105, Some("unabsorbed")).await;
    rig.paste().await;
    let p5_root = tempfile::tempdir().unwrap();
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    p5::set_test_root(Some(p5_root.path()));
    let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
        &fs::metadata(&rig.intake.transcript).unwrap(),
    );
    let event = p5::BindingEvent {
        seq: 1,
        channel_id: rig.intake.channel.get(),
        provider: "claude".into(),
        tmux_session: rig.session.clone(),
        execution_nonce: None,
        old: None,
        new: p5::BindingTarget::Source(p5::SourceId {
            session_id: rig.sid.clone(),
            path: rig.intake.transcript.clone(),
            dev,
            ino,
        }),
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    };
    let path = p5_root
        .path()
        .join(p5::BINDING_EVENTS_DIR)
        .join(format!("{}.log", rig.intake.channel.get()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    rig.intake
        .shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&rig.session, rig.intake.channel);
    let _confirmed =
        crate::services::tui_o::turn_mode::TestConfirmation::new(rig.intake.channel.get());
    rig.command().await;
    p5::set_test_root(None);
    assert_eq!(
        rig.escapes(),
        1,
        "O-on preserves its existing SessionStop policy"
    );
}

async fn replay_stop(value: Value) {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .try_init()
        .unwrap();
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as p5};
    let root = crate::config::runtime_root().expect("the restart retains its runtime root");
    p5::set_test_root(Some(&root));
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(value["channel"].as_u64().unwrap());
    let label = value["label"].as_str().unwrap();
    let session = value["session"].as_str().unwrap();
    assert_eq!(
        ProviderKind::Claude.build_tmux_session_name(label),
        session,
        "restart derives the same session from its channel locator"
    );
    discord::host_defer_gate::tests::map_channel(&shared, channel, label).await;
    shared.settings.write().await.owner_user_id = Some(7);
    assert!(
        dedupe::runtime_binding_for_tmux_session(session).is_none(),
        "a restarted process has no runtime binding cache"
    );
    let sources = logged_sources(channel, session);
    let source = sources
        .last()
        .expect("restart retains a durable bound source");
    let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
        &fs::metadata(&source.path).unwrap(),
    );
    assert_eq!((source.dev, source.ino), (dev, ino));
    let contents = fs::read_to_string(&source.path).unwrap();
    let rows: Vec<Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let queued = rows
        .iter()
        .find(|row| row["uuid"] == "owned-busy-row" || row["uuid"] == "owned-busy-row-absorbed")
        .expect("the prior process's actual Enter left its busy input row");
    if queued["type"] != "queue-operation" {
        let prompt = queued["attachment"]["prompt"].as_str().unwrap();
        use crate::services::tui_input::human_frame::history::{self, RowOwner};
        let RowOwner::Command(attempt) =
            history::row_owner("claude", session, &[prompt.to_owned()], "owned-busy-row")
        else {
            panic!("restart recovers the busy command from its kept ledger");
        };
        assert!(
            history::discord_owner_attempt(session, &source.session_id, &source.path, &attempt)
                .unwrap()
        );
    } else {
        assert_eq!(queued["operation"], "remove");
        assert_eq!(queued["reason"], "absorbed_mid_turn");
    }
    use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
    use crate::services::tui_o::writer::input_facts::{InputFacts, TurnState};
    let mut facts = InputFacts::open(SourceBinding {
        channel_id: channel.get(),
        provider: ShadowProvider::Claude,
        source: source.clone(),
    })
    .unwrap();
    let fact = loop {
        let fact = facts
            .poll(crate::services::tui_o::shadow::MAX_READ_BYTES)
            .unwrap();
        if fact.through == facts.source_len().unwrap() {
            break fact;
        }
    };
    assert!(
        matches!(fact.state, TurnState::Open { .. }),
        "the kept transcript still has an open turn"
    );
    eprintln!(
        "REPLAY_INITIAL_PROOF source=true owner=true open=true runtime_cache=false local_tmux={} runtime_kind={:?}",
        discord::host_liveness::local_tmux(session, None),
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(session)
    );
    let mock = discord_mock::DiscordMockState::new();
    mock.allow_channel(channel.get());
    mock.park_first_placeholder.store(false, Ordering::SeqCst);
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    shared
        .http
        .cached_serenity_ctx
        .set(discord_mock::serenity_context(proxy, gateway).await)
        .unwrap();
    assert!(
        shared.mailbox_peek(channel).is_none(),
        "restart has no mailbox token"
    );
    command(&shared, channel).await;
    eprintln!("REPLAY_DISCORD_RESPONSES {:?}", mock.written_texts());
    if value["unconfirmed"] == true {
        assert!(
            mock.written_texts()
                .iter()
                .any(|text| text.contains("중단 미확인")),
            "the unchanged pane policy uses the existing unconfirmed response"
        );
    }
    assert!(
        dedupe::runtime_binding_for_tmux_session(session).is_none(),
        "stop reads the kept binding log without recreating a runtime projection"
    );
    server.abort();
    p5::set_test_root(None);
}

fn logged_sources(
    channel: ChannelId,
    session: &str,
) -> Vec<crate::services::tui_o::shadow::SourceId> {
    use crate::services::tui_o::shadow::ShadowProvider;
    use crate::services::tui_o::writer::binding::{BindingEvents, ChannelBindingLog};
    let events = ChannelBindingLog::new(channel.get(), ShadowProvider::Claude)
        .binding_events_since(channel.get(), 0)
        .unwrap();
    let events: Vec<_> = events
        .into_iter()
        .filter(|event| event.tmux_session == session)
        .collect();
    if events.is_empty() {
        return Vec::new();
    }
    crate::services::tui_o::writer::adoption::logged(&events)
        .unwrap()
        .0
        .into_iter()
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_process_stops_from_the_kept_transcript_and_ledger() {
    restarted_process(
        "a_restarted_process_stops_from_the_kept_transcript_and_ledger",
        false,
        1,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_spinner_turn_uses_its_durable_source() {
    restarted_process("a_restarted_spinner_turn_uses_its_durable_source", true, 1).await;
}

async fn restarted_process(name: &str, spinner: bool, expected: usize) {
    restarted_variant(name, spinner, expected, None, None, false).await;
}

async fn restarted_variant(
    name: &str,
    spinner: bool,
    expected: usize,
    queue_mode: Option<&str>,
    screen: Option<&str>,
    submitted: bool,
) {
    if let Ok(value) = std::env::var("ADK_I7755_STOP_REPLAY") {
        replay_stop(serde_json::from_str(&value).unwrap()).await;
        return;
    }
    if !isolated(name) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as p5};
    p5::set_test_root(Some(root.path()));
    let rig = StopRig::new(if spinner { 7_755_120 } else { 7_755_106 }, None).await;
    if spinner {
        fs::write(
            rig.intake.renderer.join("after"),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/tui_input/claude-2.1.293-thinking.txt"
            )),
        )
        .unwrap();
    }
    if submitted {
        let source = fs::read_to_string(&rig.intake.transcript).unwrap();
        fs::write(
            &rig.intake.transcript,
            format!("{}\n", source.lines().next().unwrap()),
        )
        .unwrap();
    }
    if let Some(queue_mode) = queue_mode {
        fs::write(rig.intake.renderer.join("queue-mode"), queue_mode).unwrap();
    }
    if let Some(screen) = screen {
        fs::write(rig.intake.renderer.join("after"), screen).unwrap();
    }
    rig.paste().await;
    if logged_sources(rig.intake.channel, &rig.session).is_empty() {
        let binding = dedupe::runtime_binding_for_tmux_session(&rig.session).unwrap();
        dedupe::register_tmux_channel(&rig.session, rig.intake.channel.get());
        dedupe::register_tmux_runtime_binding(&rig.session, binding);
    }
    let sources = logged_sources(rig.intake.channel, &rig.session);
    let source = sources
        .last()
        .expect("the live publication kept its bound source");
    let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
        &fs::metadata(&rig.intake.transcript).unwrap(),
    );
    assert_eq!(source.path, rig.intake.transcript);
    assert_eq!(source.session_id, rig.sid);
    assert_eq!((source.dev, source.ino), (dev, ino));
    let data = json!({"channel":rig.intake.channel.get(),"label":format!("a1-rollback-{}",rig.intake.channel.get()),
        "session":rig.session,"unconfirmed":expected == 0});
    let id = format!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::owned_stop::{name}"
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &id, "--nocapture", "--test-threads=1"])
        .env("ADK_I7755_STOP_REPLAY", data.to_string())
        .output()
        .unwrap();
    eprintln!(
        "REPLAY_CHILD_STDOUT\n{}\nREPLAY_CHILD_STDERR\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "restart child: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert_eq!(
        rig.escapes(),
        expected,
        "a restarted process preserves its spinner={spinner} stop policy"
    );
    p5::set_test_root(None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_ledger_never_authorizes_the_owner_stop() {
    if !isolated("an_unreadable_ledger_never_authorizes_the_owner_stop") {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_108, None).await;
    rig.paste().await;
    let ledger = root.path().join("runtime/injected_frames.json");
    assert!(ledger.is_file(), "the paste leaves its durable ledger");
    fs::write(ledger, "{malformed ledger").unwrap();
    rig.command().await;
    assert_eq!(rig.escapes(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_ledger_never_authorizes_the_owner_stop() {
    if !isolated("a_missing_ledger_never_authorizes_the_owner_stop") {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(7_755_109, None).await;
    rig.paste().await;
    let ledger = root.path().join("runtime/injected_frames.json");
    fs::rename(&ledger, ledger.with_extension("kept")).unwrap();
    rig.command().await;
    assert_eq!(rig.escapes(), 0);
}

async fn delivery_race(name: &str, id: u64, close: bool) {
    if !isolated(name) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let rig = StopRig::new(id, None).await;
    rig.paste().await;
    let mutation = if close {
        json!({"type":"system","subtype":"turn_duration"})
    } else {
        json!({"type":"attachment","uuid":"racing-foreign-input","sessionId":rig.sid,
            "attachment":{"type":"queued_command","prompt":"foreign input at delivery",
                "commandMode":"prompt","origin":{"kind":"human"},"humanTurn":true}})
    };
    let data = rig.intake.renderer.join("racing-row");
    fs::write(&data, format!("{mutation}\n")).unwrap();
    let armed = rig.intake.renderer.join("race-armed");
    fs::write(&armed, "").unwrap();
    let shim = rig.intake._tmux.root.path().join("tmux");
    let source = fs::read_to_string(&shim).unwrap();
    let hook = format!(
        "case \"$*\" in *capture-pane*) if [ -f '{armed}' ]; then rm '{armed}'; '{real}' -L '{socket}' -f /dev/null \"$@\"; rc=$?; cat '{data}' >> '{transcript}'; exit $rc; fi ;; esac\n",
        armed = armed.display(),
        real = rig.intake._tmux.real.display(),
        socket = rig.intake._tmux.socket,
        data = data.display(),
        transcript = rig.intake.transcript.display()
    );
    fs::write(
        shim,
        source.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
    )
    .unwrap();
    rig.command().await;
    assert!(
        !armed.exists(),
        "the race changes the transcript after the delivery probe"
    );
    assert_eq!(
        rig.escapes(),
        0,
        "delivery must recheck the same clean open turn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_closed_after_judgment_receives_no_escape() {
    delivery_race(
        "a_turn_closed_after_judgment_receives_no_escape",
        7_755_210,
        true,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_input_after_judgment_receives_no_escape() {
    delivery_race(
        "foreign_input_after_judgment_receives_no_escape",
        7_755_211,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_remove_only_input_stops_from_durable_evidence() {
    restarted_variant(
        "a_restarted_remove_only_input_stops_from_durable_evidence",
        true,
        1,
        Some("remove-only"),
        None,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_spinner_with_a_draft_receives_no_escape() {
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-thinking.txt"
    ))
    .replace("❯", "❯ unsubmitted draft");
    restarted_variant(
        "a_restarted_spinner_with_a_draft_receives_no_escape",
        true,
        0,
        None,
        Some(&screen),
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_spinner_with_a_modal_receives_no_escape() {
    let screen = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel\n";
    assert!(
        crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(screen)
    );
    restarted_variant(
        "a_restarted_spinner_with_a_modal_receives_no_escape",
        true,
        0,
        None,
        Some(screen),
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_but_submitted_durable_source_does_not_invent_streaming() {
    restarted_variant(
        "an_open_but_submitted_durable_source_does_not_invent_streaming",
        true,
        0,
        None,
        None,
        true,
    )
    .await;
}
