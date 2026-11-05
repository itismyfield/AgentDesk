//! Real writer/reader, bridge callback, durable guarded updates and restart entry.
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::agent_protocol::StreamMessage;
use crate::services::claude_tui::{
    guarded_outcome,
    input::submission::tests::{ScriptedTmux, view},
};
use crate::services::discord::{self, inflight::*, turn_bridge::TurnBridgeContext};
use crate::services::memory::TokenUsage;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::{Arc, mpsc};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn spawn_bridge(
    shared: Arc<discord::SharedData>,
    state: InflightTurnState,
    token: Arc<CancelToken>,
) -> mpsc::Sender<StreamMessage> {
    let (tx, rx) = mpsc::channel();
    let gateway = Arc::new(crate::services::discord::gateway::HeadlessGateway);
    let bridge = TurnBridgeContext {
        provider: ProviderKind::Claude,
        gateway,
        channel_id: ChannelId::new(state.channel_id),
        user_msg_id: Some(MessageId::new(state.user_msg_id)),
        user_text_owned: state.user_text.clone(),
        request_owner_name: "pin fixture".into(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: state.tmux_session_name.clone(),
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: Some(MessageId::new(state.current_msg_id)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: Some(0),
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: state,
    };
    bridge.spawn_for_native_pin_test(shared, token, rx);
    tx
}

fn writer_state(
    channel: u64,
    late_callback: bool,
    successor: bool,
    unlearned_source: bool,
) -> (InflightTurnState, serde_json::Value) {
    writer_state_with_busy(channel, late_callback, successor, unlearned_source, None)
}

fn writer_state_with_busy(
    channel: u64,
    late_callback: bool,
    successor: bool,
    unlearned_source: bool,
    busy: Option<(
        &crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture,
        &str,
    )>,
) -> (InflightTurnState, serde_json::Value) {
    let dir = crate::config::runtime_root()
        .unwrap()
        .join(format!("c-native-{channel}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("native.jsonl");
    let session = match busy {
        Some(_) => format!("native-stop-restart-{channel}"),
        None => format!("AgentDesk-claude-c-native-{channel}"),
    };
    let line = |uuid: &str| {
        serde_json::json!({"type":"user","uuid":uuid,"sessionId":"persisted-sid","message":{"role":"user","content":"own"}}).to_string() + "\n"
    };
    let past = line("same-text-past");
    std::fs::write(&path, &past).unwrap();
    let spawn_nonce = if busy.is_some() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        "live-pane-incarnation".into()
    };
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&session, "spawn_nonce"),
        spawn_nonce,
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&session, "generation"),
        "1",
    )
    .unwrap();
    tui_prompt_dedupe::register_tmux_runtime_binding(
        &session,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("persisted-sid".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let token = Arc::new(CancelToken::new());
    let old_actor = Arc::downgrade(&token);
    token.bind_claude_tmux_session(&session);
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        channel + 2,
        "own".into(),
        Some("persisted-sid".into()),
        Some(session.clone()),
        Some(path.display().to_string()),
        None,
        0,
    );
    state.turn_start_offset = Some(past.len() as u64);
    state.last_offset = past.len() as u64;
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    state.born_generation = 0;
    if unlearned_source {
        state.runtime_kind = None;
        state.session_id = None;
        state.output_path = None;
        state.tmux_session_name = None;
    }
    save_inflight_state_create_new(&state).unwrap();
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.on_enter(&path, &line("writer-a"));
    let _ready = crate::services::claude_tui::host_input::SpyGuard::install(
        crate::services::claude_tui::host_input::SpyState {
            captures: std::iter::repeat_n(
                Some(
                    include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt")
                        .into(),
                ),
                64,
            )
            .collect(),
            ..Default::default()
        },
    );
    let rt = runtime();
    let durable = rt.block_on(async {
        let shared = discord::make_shared_data_for_tests();
        let mut stream =
            (!late_callback).then(|| spawn_bridge(shared.clone(), state.clone(), token.clone()));
        if successor {
            let mut next = state.clone();
            next.turn_nonce = Some("successor".into());
            next.user_msg_id += 100;
            save_inflight_state(&next).unwrap();
        }
        guarded_outcome::with_owner(None, || {
            guarded_outcome::fresh_receipt(
                &session,
                "own",
                path.to_str().unwrap(),
                "persisted-sid",
            );
            crate::services::claude::first_input::submit(
                path.to_str().unwrap(),
                0,
                Some(&token),
                &session,
                "own",
            )
            .unwrap();
        });
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}}"
        )
        .unwrap();
        let (tx, _rx) = mpsc::channel();
        crate::services::session_backend::read_claude_tui_output_file_until_result_with_harvest(
            path.to_str().unwrap(),
            0,
            tx,
            Some(token.clone()),
            crate::services::provider::SessionProbe::process(|| true),
        )
        .unwrap();
        let pin = token
            .native_stop
            .receipt_sink()
            .observed()
            .unwrap()
            .unwrap();
        let pin = serde_json::to_value(pin).unwrap();
        drop(_ready);
        drop(tmux);
        if let Some((fixture, case)) = busy {
            hand_busy_children(fixture, &shared, &state, &token, &pin, case).await;
        }
        if late_callback {
            stream = Some(spawn_bridge(shared, state.clone(), token));
        }
        let durable = load_inflight_state(&ProviderKind::Claude, channel).unwrap();
        drop(stream);
        (durable, pin)
    });
    drop(rt); // An app restart drops the old bridge and allocation, not the pane.
    assert!(
        old_actor.upgrade().is_none(),
        "observer must not keep the old allocation alive"
    );
    durable
}

#[test]
fn c2_writer_bridge_guarded_pin_survives_round_trip_and_stale_progress() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (late, unknown) in [(false, false), (true, false), (false, true), (true, true)] {
        let channel = 721_202_100 + u64::from(late) + 2 * u64::from(unknown);
        let (state, input) = writer_state(channel, late, false, unknown);
        let encoded = serde_json::to_value(&state).unwrap();
        assert_eq!(
            encoded["native_stop_user_pin"]["input"], input,
            "C2: actual bridge persists live writer proof, including late callback"
        );
        assert_eq!(input["direct"], false);
        if unknown {
            assert!(
                state.output_path.is_none() && state.runtime_kind.is_none(),
                "collector does not restamp input/runtime authority"
            );
        }
        let mut stale = encoded.clone();
        stale
            .as_object_mut()
            .unwrap()
            .remove("native_stop_user_pin");
        let mut stale: InflightTurnState = serde_json::from_value(stale).unwrap();
        stale.any_tool_used = true;
        assert_eq!(
            save_inflight_state_if_identity_unchanged(&stale, "c2_stale_progress"),
            GuardedSaveOutcome::Saved
        );
        let stored = load_inflight_state(&ProviderKind::Claude, channel).unwrap();
        assert_eq!(
            serde_json::to_value(stored).unwrap()["native_stop_user_pin"],
            encoded["native_stop_user_pin"],
            "adjacent same-episode progress cannot erase pin"
        );
    }
}

#[test]
fn c5_successor_guard_and_carry_never_transfer_native_pin() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (state, _) = writer_state(721_205_100, false, true, false);
    let encoded = serde_json::to_value(&state).unwrap();
    assert_eq!(state.turn_nonce.as_deref(), Some("successor"));
    assert!(
        encoded.get("native_stop_user_pin").is_none(),
        "stale callback cannot pin successor"
    );
    let (old, _) = writer_state(721_205_101, true, false, false);
    assert!(serde_json::to_value(&old).unwrap()["native_stop_user_pin"].is_object());
    for action in ["successor-1", "successor-2", "successor-3"] {
        let mut next = old.clone();
        next.turn_nonce = Some(action.into());
        next.user_msg_id += 1;
        save_inflight_state(&next).unwrap();
        let stored = load_inflight_state(&ProviderKind::Claude, next.channel_id).unwrap();
        assert!(
            serde_json::to_value(stored)
                .unwrap()
                .get("native_stop_user_pin")
                .is_none(),
            "{action}: a successor cannot carry the old proof"
        );
    }
}

fn rewrite_raw(state: &InflightTurnState, encoded: &serde_json::Value) {
    let root = super::inflight_runtime_root().unwrap();
    let path = inflight_state_path(&root, &ProviderKind::Claude, state.channel_id);
    std::fs::write(path, serde_json::to_vec(encoded).unwrap()).unwrap();
}

fn append(path: &std::path::Path, row: serde_json::Value) {
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{row}"
    )
    .unwrap();
}

fn tool(id: &str) -> serde_json::Value {
    serde_json::json!({"type":"assistant","sessionId":"persisted-sid","message":{"content":[
        {"type":"tool_use","id":id,"name":"Bash","input":{"command":"long-running"}}]}})
}

async fn hand_busy_children(
    fixture: &crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture,
    shared: &Arc<discord::SharedData>,
    state: &InflightTurnState,
    token: &Arc<CancelToken>,
    pin: &serde_json::Value,
    case: &str,
) {
    use crate::services::discord::queue_io::input_owner::admission::{
        Admission, AdmissionTicket, TurnBind, admit, exact_turn,
    };
    use crate::services::turn_orchestrator::PendingQueueItem;
    use poise::serenity_prelude::UserId;
    let session = state.tmux_session_name.as_deref().unwrap();
    let path = std::path::Path::new(state.output_path.as_deref().unwrap());
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(pin["range"][1].as_u64().unwrap())
        .unwrap();
    append(path, tool("root-a-tool"));
    let renderer = crate::services::claude_tui::input::submission::tests::renderer_with(
        fixture,
        session,
        "normal",
        view("", false),
        "",
        "",
    );
    std::fs::remove_file(renderer.join("transcript")).unwrap();
    std::os::unix::fs::symlink(path, renderer.join("transcript")).unwrap();
    std::fs::write(renderer.join("native-sid"), "persisted-sid").unwrap();
    std::fs::write(
        renderer.join("after"),
        include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.293-tool-running.txt"),
    )
    .unwrap();
    let source = std::fs::read_to_string(fixture.root.path().join("idle-renderer.py")).unwrap();
    let queued = source.replace(
        "row = {'type':'user','message':{'content':content}}",
        "row = {'type':'attachment','attachment':{'type':'queued_command','prompt':content,'commandMode':'prompt','origin':{'kind':'human'},'humanTurn':True}}",
    );
    let script = renderer.join("queued-renderer.py");
    std::fs::write(&script, queued).unwrap();
    std::fs::remove_file(renderer.join("ready")).unwrap();
    let command = format!("python3 '{}' '{}'", script.display(), renderer.display());
    fixture.tmux(&[
        "respawn-pane",
        "-k",
        "-t",
        &format!("={session}:"),
        &command,
    ]);
    crate::services::claude_tui::input::submission::tests::wait("queued-renderer-started", || {
        renderer.join("ready").exists()
    });
    let nonce = std::fs::read_to_string(crate::services::tmux_common::session_temp_path(
        session,
        "spawn_nonce",
    ))
    .unwrap();
    let context = serde_json::json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),
        "execution_nonce":nonce,"tmux_session":session,"channel_id":state.channel_id,
        "owner_runtime_root":crate::services::tmux_common::current_tmux_owner_marker(),
        "host":"native-stop-busy-fixture","expected_native_session_id":null,
        "launch_mode":"fresh","provider_root":null});
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        serde_json::from_value(context).unwrap(),
    )
    .unwrap();
    let channel = ChannelId::new(state.channel_id);
    let mailbox = shared.mailbox(channel);
    assert!(
        mailbox
            .try_start_turn(
                token.clone(),
                UserId::new(7),
                MessageId::new(state.user_msg_id)
            )
            .await
    );
    let owner_dir = discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/native-stop-busy");
    std::fs::create_dir_all(&owner_dir).unwrap();
    let owner_path = owner_dir.join(format!("{}.json", state.channel_id));
    mailbox
        .v2_adopt(owner_path, "native-busy".into())
        .await
        .unwrap();
    for (index, child) in ["busy-b", "busy-c"].into_iter().enumerate() {
        let row_id = format!("{case}-{child}");
        std::fs::write(renderer.join("native-uuid"), &row_id).unwrap();
        let id = state.user_msg_id + 100 + index as u64;
        let item: PendingQueueItem = serde_json::from_value(serde_json::json!({"author_id":7,
            "message_id":id,"channel_id":state.channel_id,"source_message_ids":[id],
            "queued_generation":1,"text":format!("{case} {child}")}))
        .unwrap();
        let ticket = AdmissionTicket::new();
        assert!(matches!(
            admit(&mailbox, &item, ticket.clone()).await,
            Admission::Accepted(_)
        ));
        let front = mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                let front = owner.front().ok_or("no front")?;
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        assert_eq!(
            discord::router::message_handler::hand_busy(
                (mailbox.clone(), None),
                ProviderKind::Claude,
                (state.channel_id, front),
            )
            .await,
            Ok(()),
            "{case}: {child} reached the real busy input entry"
        );
        let body = std::fs::read_to_string(renderer.join("body")).unwrap();
        assert!(body.contains(child));
        let transcript = std::fs::read_to_string(path).unwrap();
        let submitted: serde_json::Value =
            serde_json::from_str(transcript.lines().last().unwrap()).unwrap();
        assert_eq!(submitted["uuid"], row_id);
        assert_eq!(submitted["attachment"]["type"], "queued_command");
        let TurnBind::Settled(turn) = exact_turn(&mailbox, &ticket, &item).await.unwrap() else {
            panic!("{case}: {child} settled under its actual input attempt");
        };
        if case == "valid" && child == "busy-c" {
            std::fs::write(
                fixture.root.path().join("foreign-busy.json"),
                serde_json::to_vec(
                    &serde_json::json!({"attempt":turn.attempt,"frame":body,"uuid":row_id}),
                )
                .unwrap(),
            )
            .unwrap();
        }
        append(path, tool(&format!("{child}-tool")));
    }
    let durable = load_inflight_state(&ProviderKind::Claude, state.channel_id).unwrap();
    if case == "other-source" {
        let foreign: serde_json::Value = serde_json::from_slice(
            &std::fs::read(fixture.root.path().join("foreign-busy.json")).unwrap(),
        )
        .unwrap();
        let attempt = foreign["attempt"].as_str().unwrap();
        let frame = foreign["frame"].as_str().unwrap();
        record_native_busy_child(&durable, token, attempt);
        append(
            path,
            serde_json::json!({"type":"attachment","uuid":foreign["uuid"],
            "attachment":{"type":"queued_command","prompt":frame,"commandMode":"prompt","origin":{"kind":"human"}}}),
        );
        append(path, tool("foreign-tool"));
    }
}

#[test]
fn actual_busy_input_restart_and_native_stop_keep_only_the_birth_episode_pg() {
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, channel_key, seed, shared_on,
    };
    use crate::services::discord::turn_bridge::{CommandStop, begin_user_stop};
    use crate::services::session_host::test_support::{
        InjectedLivenessGuard, InjectedPresenceGuard,
    };
    use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::after_shared_test_env_lock();
    let shim = fixture.root.path().join("tmux");
    let script = std::fs::read_to_string(&shim).unwrap().replacen("#!/bin/sh\n", &format!(
        "#!/bin/sh\nfor last do :; done\ncase \"$*\" in *send-keys*) [ \"$last\" != Escape ] || echo Escape >> '{}/native-stop-keys' ;; esac\n",
        fixture.root.path().display(),
    ), 1);
    std::fs::write(shim, script).unwrap();
    let cases = [
        "valid",
        "valid-o",
        "other-episode",
        "other-source",
        "other-parent",
    ];
    let selected = [(721_203_801, RuntimeHandoffKind::ClaudeTui)];
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&selected);
    let mut states = Vec::new();
    for (index, case) in cases.into_iter().enumerate() {
        let channel = 721_203_800 + index as u64;
        let (mut state, _) =
            writer_state_with_busy(channel, false, false, false, Some((&fixture, case)));
        state.last_offset = std::fs::metadata(state.output_path.as_ref().unwrap())
            .unwrap()
            .len();
        save_inflight_state(&state).unwrap();
        if matches!(case, "other-episode" | "other-parent") {
            let mut encoded = serde_json::to_value(&state).unwrap();
            if let Some(children) = encoded
                .get_mut("native_stop_busy_children")
                .filter(|children| children.is_object())
            {
                if case == "other-episode" {
                    children["turn_nonce"] = "another-episode".into();
                } else {
                    children["user_msg_id"] = (state.user_msg_id + 1).into();
                }
            }
            rewrite_raw(&state, &encoded);
        }
        tui_prompt_dedupe::clear_tmux_runtime_binding(state.tmux_session_name.as_deref().unwrap());
        states.push((state, case));
    }
    let rt = runtime();
    let recovered = rt.block_on(Box::pin(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut shared = shared_on(&pool).await;
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = 42;
        let mut guards = Vec::new();
        for (state, _) in &states {
            let session = state.tmux_session_name.as_deref().unwrap();
            seed(
                &pool,
                &channel_key(&shared, session),
                session,
                state.channel_id,
                Stored::Legacy,
            )
            .await;
            guards.push((
                InjectedLivenessGuard::set(HostSessionRef::tmux(session), HostLiveness::Live),
                InjectedPresenceGuard::set(HostSessionRef::tmux(session), HostPresence::Present),
            ));
        }
        let recorder =
            discord::recovery_engine::o_cut_recorder::start(states[0].0.channel_id).await;
        discord::recovery_engine::restore_inflight_turns(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
        )
        .await;
        for (state, case) in &states {
            let channel = ChannelId::new(state.channel_id);
            let session = state.tmux_session_name.as_deref().unwrap();
            let path = std::path::Path::new(state.output_path.as_deref().unwrap());
            assert_eq!(
                crate::services::tui_o::cutover::o_owns_tui_output_for_channel(
                    state.channel_id,
                    Some(RuntimeHandoffKind::ClaudeTui),
                ),
                Ok(*case == "valid-o"),
                "{case}: exercise the configured output owner"
            );
            let token = discord::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .expect("the actual restart admitted the live episode");
            assert_eq!(token.turn_nonce(), state.turn_nonce.as_deref());
            tui_prompt_dedupe::register_tmux_runtime_binding(
                session,
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: path.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some("persisted-sid".into()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            let before = std::fs::read_to_string(fixture.root.path().join("native-stop-keys"))
                .unwrap_or_default();
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            let CommandStop::Native(stop) = result else {
                panic!("{case}: a native stop");
            };
            let after = std::fs::read_to_string(fixture.root.path().join("native-stop-keys"))
                .unwrap_or_default();
            let sent = usize::from(case.starts_with("valid"));
            assert_eq!(
                after.lines().count() - before.lines().count(),
                sent,
                "{case}"
            );
            assert!(!token.cancelled.load(std::sync::atomic::Ordering::Acquire));
            if sent == 1 {
                append(
                    path,
                    serde_json::json!({"type":"system","subtype":"turn_duration"}),
                );
                let finished = stop.finish().await;
                assert!(
                    finished
                        .token
                        .as_ref()
                        .is_some_and(|finished| Arc::ptr_eq(finished, &token)),
                    "{case}: finish publishes only the recovered token"
                );
            } else {
                assert!(stop.finish().await.token.is_none(), "{case}");
            }
            tui_prompt_dedupe::clear_tmux_runtime_binding(session);
        }
        pool.close().await;
        db.drop().await;
        Arc::downgrade(&shared)
    }));
    drop(rt);
    assert!(
        recovered.upgrade().is_none(),
        "the recovered runtime releases its watchers"
    );
}

#[test]
fn c2_c3_c4_actual_restart_rechecks_only_persisted_proof_pg() {
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, channel_key, seed, shared_on,
    };
    use crate::services::session_host::test_support::{
        InjectedLivenessGuard, InjectedPresenceGuard,
    };
    use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (mut cases, mut stored) = (Vec::new(), Vec::new());
    for (index, change) in [
        "valid",
        "sid",
        "inode",
        "uuid",
        "digest",
        "frame-digest",
        "spawn",
        "path",
        "rotated-file",
        "old-row",
    ]
    .iter()
    .enumerate()
    {
        let (mut state, input) = writer_state(721_203_100 + index as u64, true, false, false);
        if *change == "valid" {
            record_native_busy_child(&state, &CancelToken::new(), "busy-c");
            state = load_inflight_state(&ProviderKind::Claude, state.channel_id).unwrap();
        }
        let end = input.get("range").unwrap_or(&input[2])[1].as_u64().unwrap();
        state.last_offset = end;
        std::fs::OpenOptions::new()
            .write(true)
            .open(state.output_path.as_ref().unwrap())
            .unwrap()
            .set_len(end)
            .unwrap();
        use std::io::Write;
        writeln!(std::fs::OpenOptions::new().append(true).open(state.output_path.as_ref().unwrap()).unwrap(), "{}", serde_json::json!({"type":"assistant","sessionId":"persisted-sid","message":{"content":[{"type":"tool_use","id":"tool-c","name":"Bash","input":{"command":"long-running"}}]}})).unwrap();
        let mut encoded = serde_json::to_value(&state).unwrap();
        if *change != "old-row" {
            assert!(
                encoded["native_stop_user_pin"].is_object(),
                "verified proof exists before restart: {change}"
            );
        }
        match *change {
            "sid" => encoded["native_stop_user_pin"]["input"]["session_id"] = "other-sid".into(),
            "inode" => encoded["native_stop_user_pin"]["input"]["source_file"][1] = 0.into(),
            "uuid" => encoded["native_stop_user_pin"]["input"]["uuid"] = "other-uuid".into(),
            "digest" => {
                encoded["native_stop_user_pin"]["input"]["row_digest"] = "0".repeat(64).into()
            }
            "frame-digest" => {
                encoded["native_stop_user_pin"]["input"]["frame_digest"] = "0".repeat(64).into()
            }
            "rotated-file" => {
                let path = std::path::Path::new(state.output_path.as_ref().unwrap());
                let replacement = path.with_extension("replacement");
                std::fs::copy(path, &replacement).unwrap();
                std::fs::rename(replacement, path).unwrap();
            }
            "spawn" => std::fs::write(
                crate::services::tmux_common::session_temp_path(
                    state.tmux_session_name.as_deref().unwrap(),
                    "spawn_nonce",
                ),
                "recreated-pane",
            )
            .unwrap(),
            "path" => encoded["native_stop_user_pin"]["input"]["output_path"] = "other-path".into(),
            "old-row" => {
                encoded
                    .as_object_mut()
                    .unwrap()
                    .remove("native_stop_user_pin");
            }
            _ => {}
        }
        rewrite_raw(&state, &encoded);
        stored.push(serde_json::json!([
            encoded["native_stop_user_pin"],
            encoded["native_stop_busy_children"]
        ]));
        cases.push((state, *change));
    }
    let rt = runtime();
    let recovered_app = rt.block_on(Box::pin(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut shared = shared_on(&pool).await;
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = 42;
        let mut guards = Vec::new();
        for (state, _) in &cases {
            let session = state.tmux_session_name.as_deref().unwrap();
            seed(
                &pool,
                &channel_key(&shared, session),
                session,
                state.channel_id,
                Stored::Legacy,
            )
            .await;
            guards.push((
                InjectedLivenessGuard::set(HostSessionRef::tmux(session), HostLiveness::Live),
                InjectedPresenceGuard::set(HostSessionRef::tmux(session), HostPresence::Present),
            ));
        }
        let tmux = ScriptedTmux::new(&view("", false));
        tmux.set_dead(false);
        // A new process has no runtime binding until rehydration registers it.
        for (state, _) in &cases {
            tui_prompt_dedupe::clear_tmux_runtime_binding(
                state.tmux_session_name.as_deref().unwrap(),
            );
        }
        let discord = discord::recovery_engine::o_cut_recorder::start(cases[0].0.channel_id).await;
        discord::recovery_engine::restore_inflight_turns(
            &discord.http,
            &shared,
            &ProviderKind::Claude,
        )
        .await;
        let durable = |state: &InflightTurnState| {
            let row = load_inflight_state(&ProviderKind::Claude, state.channel_id).unwrap();
            let row = serde_json::to_value(row).unwrap();
            serde_json::json!([
                row["native_stop_user_pin"],
                row["native_stop_busy_children"]
            ])
        };
        let mut tokens = Vec::new();
        for (index, (state, change)) in cases.iter().enumerate() {
            let snapshot =
                discord::mailbox_snapshot(&shared, ChannelId::new(state.channel_id)).await;
            let token = snapshot
                .cancel_token
                .expect("actual live-pane restore admitted the episode");
            assert_eq!(token.turn_nonce(), state.turn_nonce.as_deref());
            let sink = token.native_stop.receipt_sink();
            assert_eq!(
                sink.observed(),
                None,
                "{change}: restore accepts no pin itself"
            );
            assert_eq!(
                sink.stop_pin(),
                None,
                "{change}: no binding yet, nothing to target"
            );
            assert_eq!(
                token.native_stop.is_busy_child("busy-c"),
                *change == "valid",
                "{change}: restore returns only the episode's recorded busy input"
            );
            assert_eq!(
                durable(state),
                stored[index],
                "C3 {change}: restore writes no durable pin"
            );
            assert!(!token.cancelled.load(std::sync::atomic::Ordering::Acquire));
            tokens.push(token);
        }
        for ((state, change), token) in cases.iter().zip(&tokens) {
            let path = state.output_path.clone().unwrap();
            tui_prompt_dedupe::register_tmux_runtime_binding(
                state.tmux_session_name.as_deref().unwrap(),
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: path,
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some("persisted-sid".into()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            let pin = token.native_stop.receipt_sink().stop_pin();
            if *change == "valid" {
                assert!(
                    pin.is_some(),
                    "C2: a stop after rehydration accepts the saved pin despite the boot order"
                );
            } else {
                assert_eq!(pin, None, "C3/C4 {change}: no invalid or retroactive pin");
            }
        }
        for (index, (state, change)) in cases.iter().enumerate() {
            assert_eq!(
                durable(state),
                stored[index],
                "C3 {change}: a failed or passed recheck leaves the durable pin as stored"
            );
        }
        pool.close().await;
        db.drop().await;
        Arc::downgrade(&shared)
    }));
    drop(rt);
    assert!(
        recovered_app.upgrade().is_none(),
        "the test runtime owns and releases its recovered watcher tasks"
    );
}
