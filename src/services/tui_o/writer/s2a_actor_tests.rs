use super::*;
use crate::services::provider::CancelToken;
use crate::services::tui_o::store::codex_provenance::submit_floor::{
    SubmitFloor, tests as fixture,
};
use crate::services::tui_o::store::fault::{self, Step};
use crate::services::tui_o::writer::actor::codex_submission::Operation;
use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission;

#[tokio::test(start_paused = true)]
async fn s2a_same_claim_restart_rejects_delayed_close_reply_before_issue_and_attach() {
    let _selected = test_override::force_candidates(&[(CHANNEL, CodexTui)]);
    let mut source = None;
    let harness = Harness::build_channel(CHANNEL, |root| {
        let native = fixture::native_file(root);
        let bytes = std::fs::read(&native.path).unwrap();
        source = Some(native.clone());
        vec![InitSource {
            source_id: native,
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    let source = source.unwrap();
    p5_log(
        harness._runtime.path(),
        CHANNEL,
        &p5_event(CHANNEL, "codex", p5::BindingTarget::Source(source.clone())),
    );
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    p5::set_test_root(Some(harness._runtime.path()));
    let mut handles = start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root(&harness)),
            gate: Arc::clone(&harness.gate),
            readiness: Arc::clone(&ready),
        },
    );
    assert_eq!(handles.len(), 1);
    polls(3).await;
    assert!(ready.accepts(CHANNEL), "{:?}", io.alarms.halted());
    let a = ready
        .submission(CHANNEL)
        .expect("actual Hosted::run installed A");
    let channel = harness.channel();
    let floor = fixture::native_floor(
        fixture::launch(CHANNEL, &source),
        channel.cursor(&source).unwrap(),
    );
    a.request(Operation::Prepare(SubmitFloor::NativeFloor(Box::new(
        floor.clone(),
    ))))
    .await
    .unwrap();
    let span = fixture::terminal(&floor);
    // Keep A's delivered reply unconsumed across the recovery of the same hosting claim.
    let delayed = a
        .request(Operation::Close(Box::new(span.clone())))
        .await
        .unwrap();
    let generation = handles[0].generation();
    let mut next_launch = fixture::launch(CHANNEL, &source);
    next_launch.episode.user_message_id += 1;
    next_launch.episode.turn_nonce = "next-turn".into();
    let next = fixture::native_floor(next_launch, channel.cursor(&source).unwrap());
    let failed = fault::plant(
        root(&harness).as_path(),
        Step::Write,
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    assert!(
        a.request(Operation::Prepare(SubmitFloor::NativeFloor(Box::new(next))))
            .await
            .is_err()
    );
    drop(failed);
    let mut b = None;
    for _ in 0..60 {
        polls(1).await;
        if let Some(port) = ready.submission(CHANNEL)
            && !port.same_run(&a)
            && ready.accepts(CHANNEL)
        {
            b = Some(port);
            break;
        }
    }
    let b = b.unwrap_or_else(|| panic!("A/B recovery: {:?}", io.alarms.0.lock().unwrap()));
    assert_eq!(
        handles[0].generation(),
        generation,
        "the hosting claim stayed the same"
    );
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        span.episode.turn_nonce.clone(),
    )));
    let issued = AtomicUsize::new(0);
    let result = ready.with_closed_reply(CHANNEL, delayed, |closed| {
        issued.fetch_add(1, Ordering::SeqCst);
        admission::issue_for_tests(
            closed.span().execution.clone(),
            closed.span().episode.clone(),
            &token,
        )
    });
    assert!(
        result.is_err(),
        "a same-generation stale reply must not reach issuance"
    );
    assert_eq!(
        issued.load(Ordering::SeqCst),
        0,
        "issuance and its token attach were never called"
    );
    let current = b
        .request(Operation::Close(Box::new(span.clone())))
        .await
        .unwrap();
    let record = ready
        .with_closed_reply(CHANNEL, current, |closed| {
            issued.fetch_add(1, Ordering::SeqCst);
            admission::issue_for_tests(
                closed.span().execution.clone(),
                closed.span().episode.clone(),
                &token,
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        issued.load(Ordering::SeqCst),
        1,
        "a current reply really reaches the existing issuer"
    );
    assert_eq!(Arc::strong_count(&record), 3);
    assert!(harness.port.posts().is_empty());
    admission::forget_channel_for_tests(CHANNEL);
    handles.pop().unwrap().stop_and_join().await.unwrap();
}

async fn s2b1a_host() -> (
    Harness,
    Arc<TestIo>,
    Arc<Readiness>,
    Vec<ManagedWriterHandle>,
    crate::services::tui_o::shadow::SourceId,
) {
    let mut source = None;
    let harness = Harness::build_channel(CHANNEL, |root| {
        let native = fixture::native_file(root);
        let bytes = std::fs::read(&native.path).unwrap();
        source = Some(native.clone());
        vec![InitSource {
            source_id: native,
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    let source = source.unwrap();
    p5_log(
        harness._runtime.path(),
        CHANNEL,
        &p5_event(CHANNEL, "codex", p5::BindingTarget::Source(source.clone())),
    );
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    p5::set_test_root(Some(harness._runtime.path()));
    let handles = start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root(&harness)),
            gate: Arc::clone(&harness.gate),
            readiness: Arc::clone(&ready),
        },
    );
    assert_eq!(handles.len(), 1);
    polls(3).await;
    assert!(ready.accepts(CHANNEL));
    (harness, io, ready, handles, source)
}

#[tokio::test(start_paused = true)]
async fn s2b1a_same_claim_restart_rejects_delayed_typed_prepare_before_submit() {
    let _selected = test_override::force_candidates(&[(CHANNEL, CodexTui)]);
    let (harness, io, ready, mut handles, source) = s2b1a_host().await;
    let a = ready.submission(CHANNEL).unwrap();
    let launch = fixture::launch(CHANNEL, &source);
    let floor = fixture::native_floor(launch.clone(), harness.channel().cursor(&source).unwrap());
    let delayed = a
        .request(Operation::Capture(Box::new((
            launch.clone(),
            floor.execution().clone(),
        ))))
        .await
        .unwrap();
    let generation = handles[0].generation();
    let failed = fault::plant(
        root(&harness).as_path(),
        Step::Write,
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    let mut next_launch = launch.clone();
    next_launch.episode.user_message_id += 1;
    next_launch.episode.turn_nonce = "successor-turn".into();
    assert!(
        a.request(Operation::Capture(Box::new((
            next_launch,
            floor.execution().clone()
        ))))
        .await
        .is_err()
    );
    drop(failed);
    let mut b = None;
    for _ in 0..60 {
        polls(1).await;
        if let Some(port) = ready.submission(CHANNEL)
            && !port.same_run(&a)
            && ready.accepts(CHANNEL)
        {
            b = Some(port);
            break;
        }
    }
    let b = b.unwrap_or_else(|| panic!("recovery: {:?}", io.alarms.0.lock().unwrap()));
    assert_eq!(handles[0].generation(), generation);
    let submits = AtomicUsize::new(0);
    assert!(
        ready
            .with_prepared_reply(CHANNEL, delayed, |_| {
                submits.fetch_add(1, Ordering::SeqCst);
            })
            .is_err(),
        "same-generation A prepare cannot authorize B input"
    );
    assert_eq!(submits.load(Ordering::SeqCst), 0);
    let current = b
        .request(Operation::Capture(Box::new((
            launch,
            floor.execution().clone(),
        ))))
        .await
        .unwrap();
    ready
        .with_prepared_reply(CHANNEL, current, |prepared| {
            assert_eq!(prepared.floor().execution(), floor.execution());
            assert_eq!(prepared.floor().checkpoint(), floor.checkpoint());
            submits.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    assert_eq!(submits.load(Ordering::SeqCst), 1);
    assert!(harness.port.posts().is_empty());
    handles.pop().unwrap().stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn s2b1a_dropping_command_sender_does_not_stop_hosted_actor() {
    let _selected = test_override::force_candidates(&[(CHANNEL, CodexTui)]);
    let (harness, _io, ready, mut handles, _source) = s2b1a_host().await;
    ready.drop_submission_for_tests(CHANNEL);
    polls(3).await;
    assert!(
        ready.accepts(CHANNEL),
        "closed command input must fall back to the existing poll loop"
    );
    assert!(harness.port.posts().is_empty());
    handles.pop().unwrap().stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn s2b1b_actual_driver_two_warm_turns_share_execution_and_post_once_each() {
    actual_driver_case("two").await;
}

#[tokio::test(start_paused = true)]
async fn s2b1b_actual_driver_failures_cancel_and_stale_evidence_issue_nothing() {
    for case in [
        "no_receipt",
        "cancel_before",
        "cancel_submitted",
        "open",
        "synthetic",
        "close_write",
        "close_sync",
        "close_readback",
        "row_swap",
        "source_changed",
        "late_prepare",
        "late_close",
        "cancel_attach",
    ] {
        actual_driver_case(case).await;
    }
}

#[tokio::test(start_paused = true)]
async fn s2b1b_actual_driver_delivery_gate_off_keeps_closed_evidence_without_post() {
    actual_driver_case("delivery_off").await;
}

#[tokio::test(start_paused = true)]
async fn s2b1b_actual_driver_issued_record_fences_cancellation_during_first_poll() {
    actual_driver_case("first_poll").await;
}

// Dedupe state stays isolated until the blocking driver and its actor have both stopped.
struct ActualDriverIsolation {
    _state: std::sync::MutexGuard<'static, ()>,
}

async fn actual_driver_case(mode: &'static str) {
    use crate::services::claude_tui::hook_server::adoption_retry::DurableKind;
    use crate::services::claude_tui::hook_server::observation_ingress::{
        IngressOutcome, observe_binding_hook,
    };
    use crate::services::tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{
            BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope,
            ObservedHookProcess, PreparedIncarnation,
        },
    };
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    // A canonical short parent keeps native claims and temporary IPC paths stable.
    let temp_parent = std::env::temp_dir().canonicalize().unwrap();
    let temp_parent = if temp_parent.as_os_str().len() <= 24 {
        temp_parent
    } else {
        std::fs::canonicalize("/tmp").unwrap()
    };
    let _tmpdir =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("TMPDIR", &temp_parent);
    let _dedupe_lock = ActualDriverIsolation {
        _state: dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner()),
    };
    let (_env_root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    let channel = crate::services::codex_tui::canary::CANARY_CHANNEL;
    let tmux = crate::services::codex_tui::canary::CANARY_TMUX;
    let native_id = "019e660d-4859-7522-9cee-8ba7c4e7c743";
    let _selected = test_override::force_candidates(&[(channel, CodexTui)]);
    admission::forget_channel_for_tests(channel);
    admission::set_ready_for_tests(channel, true);
    let mut source = None;
    let harness = Harness::build_channel(channel, |root| {
        let path = root.join(format!("rollout-{native_id}.jsonl"));
        let at = Utc::now().to_rfc3339();
        let meta = serde_json::json!({"type":"session_meta","timestamp":at,"payload":{
            "id":native_id,"timestamp":at,"cwd":root,"source":"cli","originator":"codex_cli_rs"}});
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        let native =
            crate::services::tui_o::shadow::binding_reader::source_id_for(native_id, &path)
                .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        source = Some(native.clone());
        vec![InitSource {
            source_id: native,
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    let source = source.unwrap();
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now() - chrono::Duration::seconds(60),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: tmux.into(),
        channel_id: Some(channel),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(harness._runtime.path().to_path_buf()),
        first_prompt_digest: Some(format!("sha256:{}", hex::encode(Sha256::digest(b"first")))),
        source_policy: Some("verified".into()),
    };
    PreparedIncarnation::create(context.clone()).unwrap();
    let nonce = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&nonce).parent().unwrap()).unwrap();
    std::fs::write(nonce, &context.execution_nonce).unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(
            tmux,
            crate::services::tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
        ),
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui.as_str(),
    )
    .unwrap();
    dedupe::register_tmux_channel(tmux, channel);
    dedupe::binding_events::set_test_root(Some(harness._runtime.path()));
    dedupe::codex_verified::set_permission_for_tests(
        &context,
        dedupe::codex_verified::DeliveryPermission::Allowed,
    );
    let fake = crate::services::provider_teardown::tests::test_support::FakeTmux::install(tmux);
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let claim = serde_json::json!({"session_id":native_id,"transcript_path":source.path,"source":"startup","prompt":null});
    let observed = observe_binding_hook(
        "codex",
        "session-start",
        Some(native_id),
        Some(native_id),
        &claim,
        &headers,
    );
    assert_eq!(
        observed,
        IngressOutcome::Durable(DurableKind::Adopted),
        "the native hook must durably adopt the real source"
    );
    assert!(
        dedupe::binding_events::codex::read_ownership(&context)
            .unwrap()
            .verified
            .is_some(),
        "real ownership registration must succeed"
    );
    crate::services::codex_tui::warm_followup::write_codex_tui_launch_options_evidence(
        tmux,
        &crate::services::codex::actual_submit_tests::options(native_id, harness._runtime.path()),
    )
    .unwrap();
    harness.gate.acquired();
    let close_fault = if matches!(mode, "close_sync" | "close_readback") {
        Some(
            crate::services::tui_o::store::codex_provenance::ActualCloseFault::install(
                harness
                    ._runtime
                    .path()
                    .join("o_store")
                    .join(channel.to_string()),
                mode == "close_sync",
            ),
        )
    } else {
        None
    };
    let io = TestIo::over(&harness);
    let ready = crate::services::tui_o::writer::host::process_readiness();
    let mut handles = start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root(&harness)),
            gate: Arc::clone(&harness.gate),
            readiness: Arc::clone(&ready),
        },
    );
    assert_eq!(handles.len(), 1);
    polls(3).await;
    assert!(ready.accepts(channel), "{:?}", io.alarms.halted());
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(ChannelId::new(channel));
    admission::set_ready_for_tests(channel, mode == "first_poll");
    let first_poll_cancel = Arc::new(Mutex::new(None));
    let first_poll_raced = Arc::new(Mutex::new(None));
    let mut tokens = Vec::new();
    let mut spans = Vec::new();
    for (i, body) in [(0, "first body"), (1, "second body")]
        .into_iter()
        .take(if mode == "two" { 2 } else { 1 })
    {
        let token = Arc::new(CancelToken::new());
        if mode == "first_poll" {
            harness.port.lazy.store(true, Ordering::SeqCst);
            let (owner, join, raced) = (
                Arc::clone(&token),
                Arc::clone(&first_poll_cancel),
                Arc::clone(&first_poll_raced),
            );
            *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
                let (attempt, attempted) = std::sync::mpsc::channel();
                let stopped = Arc::clone(&owner);
                let thread = std::thread::spawn(move || {
                    attempt.send(()).unwrap();
                    stopped.publish_cancel("first poll stop");
                });
                attempted.recv().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(50));
                *raced.lock().unwrap() = Some(owner.cancelled.load(Ordering::Acquire));
                *join.lock().unwrap() = Some(thread);
            }));
        }
        assert!(
            mailbox
                .try_start_turn(Arc::clone(&token), UserId::new(8), MessageId::new(30 + i))
                .await
        );
        let row =
            crate::services::discord::save_actual_episode_for_tests(channel, tmux, 30 + i, &token);
        let row_before = std::fs::read(&row).unwrap();
        let worker_context = context.clone();
        let worker_token = Arc::clone(&token);
        let path = source.path.clone();
        let worker_mailbox = mailbox.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let task = tokio::task::spawn_blocking(move || {
            use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState};
            use std::io::Write;
            dedupe::binding_events::set_test_root(worker_context.provider_root.as_deref());
            dedupe::codex_verified::set_permission_for_tests(
                &worker_context,
                if mode == "delivery_off" {
                    dedupe::codex_verified::DeliveryPermission::Unknown
                } else {
                    dedupe::codex_verified::DeliveryPermission::Allowed
                },
            );
            let ready_pane = "╭──────────────────────────────────────────────╮\n│ ▌                                            │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
            let draft = "╭──────────────────────────────────────────────╮\n│ first ▌                                      │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
            let native_path = path.clone();
            let spy = SpyGuard::install(SpyState {
                fail_send: if mode == "no_receipt" {
                    Some((1, Err("unacknowledged Enter".into())))
                } else {
                    None
                },
                captures: [ready_pane, draft, ready_pane, ready_pane]
                    .into_iter()
                    .map(|s| Some(s.into()))
                    .collect(),
                on_enter: Some(Box::new(move || {
                    let mut file = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap();
                    let turn = format!("native-{i}");
                    for record in [
                        serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
                        serde_json::json!({"type":"response_item","payload":{"type":"message","id":format!("body-{i}"),"role":"assistant","content":[{"type":"output_text","text":body}]}}),
                        serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":body}}),
                    ] {
                        if matches!(mode, "open") && record["payload"]["type"] == "task_complete" {
                            continue;
                        }
                        let mut record = record;
                        if mode == "synthetic" && record["payload"]["type"] == "task_complete" {
                            record["payload"]["synthetic"] = true.into();
                        }
                        writeln!(file, "{record}").unwrap();
                    }
                })),
                ..SpyState::default()
            });
            let boundaries = Arc::new(Mutex::new(Vec::<String>::new()));
            let stages = Arc::clone(&boundaries);
            let (hook_token, hook_context) = (Arc::clone(&worker_token), worker_context.clone());
            let mut faults = Vec::new();
            crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |stage| {
                    stages.lock().unwrap().push(stage.to_owned());
                    if (mode=="cancel_before" && stage=="prepare_ack") || (mode=="cancel_submitted" && stage=="submitted")
                        || (mode=="cancel_attach" && stage=="close_ack") {hook_token.publish_cancel("boundary stop");}
                    if mode=="close_write" && stage=="before_close" {
                        faults.push(fault::plant(hook_context.provider_root.as_ref().unwrap(),Step::Write,std::io::ErrorKind::StorageFull,Some(1)));
                    }
                    if mode=="row_swap" && stage=="prepare_ack" {
                        let runtime=tokio::runtime::Handle::current();
                        runtime.block_on(async {
                            worker_mailbox.finish_turn_if_token(Some(Arc::clone(&hook_token)),
                                crate::services::turn_orchestrator::QueuePersistenceContext::new(
                                    &crate::services::provider::ProviderKind::Codex,"actual-row-swap",None)).await;
                            let other=Arc::new(CancelToken::from_persisted_turn_nonce(hook_token.turn_nonce().map(str::to_owned)));
                            assert!(worker_mailbox.try_start_turn(other,UserId::new(8),MessageId::new(30)).await);
                        });
                    }
                    if mode=="source_changed" && stage=="prepare_ack" {
                        std::fs::rename(&native_path,native_path.with_extension("old-inode")).unwrap();
                        let bytes=std::fs::read(native_path.with_extension("old-inode")).unwrap();
                        std::fs::write(&native_path,bytes).unwrap();
                    }
                    if (mode=="late_prepare" && stage=="prepare_ack") || (mode=="late_close" && stage=="close_ack") {
                        let runtime=tokio::runtime::Handle::current();
                        let ready=crate::services::tui_o::writer::host::process_readiness();
                        let old=ready.submission(channel).unwrap();
                        let execution=crate::services::tmux_common::with_tmux_source_authority(&hook_context.tmux_session,
                            |a| dedupe::codex_verified::VerifiedSourceSnapshot::capture(a).unwrap().execution().clone());
                        let row=runtime.block_on(crate::services::discord::codex_episode_snapshot(channel,&hook_context.tmux_session,&hook_token)).unwrap();
                        let mut episode=row.episode().clone();episode.user_message_id+=100;episode.turn_nonce.push_str("-halt");
                        let bad=fault::plant(hook_context.provider_root.as_ref().unwrap(),Step::Write,std::io::ErrorKind::StorageFull,Some(1));
                        assert!(runtime.block_on(old.request(Operation::Capture(Box::new((crate::services::tui_o::store::codex_provenance::submit_floor::PreparedLaunch{
                            context:hook_context.clone(),episode},execution))))).is_err());
                        drop(bad);
                        runtime.block_on(async {
                            for _ in 0..60 {if ready.submission(channel).is_some_and(|p|!p.same_run(&old)) && ready.accepts(channel) {return;}
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;}
                            panic!("actual stale reply fixture never restarted its actor: mode={mode}, live={:?}, accepts={}",ready.submission(channel).map(|p|p.same_run(&old)),ready.accepts(channel));
                        });
                    }
                    let _=&worker_mailbox;
                }));
            });
            let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let expired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (done, timeout, stopper) = (
                Arc::clone(&finished),
                Arc::clone(&expired),
                Arc::clone(&worker_token),
            );
            let watchdog = std::thread::spawn(move || {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(if matches!(mode, "two" | "first_poll") {
                        10
                    } else {
                        2
                    });
                while !done.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                if !done.load(Ordering::Acquire) {
                    timeout.store(true, Ordering::Release);
                    stopper.publish_cancel("actual fixture deadline");
                }
            });
            let result = crate::services::codex::actual_submit_tests::driver(
                &worker_context,
                native_id,
                "first",
                worker_token,
                tx,
            );
            finished.store(true, Ordering::Release);
            watchdog.join().unwrap();
            let result_tuple = (
                result,
                spy.calls(),
                expired.load(Ordering::Acquire),
                boundaries.lock().unwrap().clone(),
            );
            crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY
                .with(|slot| slot.borrow_mut().take());
            result_tuple
        });
        while !task.is_finished() {
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let outcome = task.await;
        if outcome.is_err() {
            handles.pop().unwrap().stop_and_join().await.unwrap();
        }
        let (result, calls, expired, stages) = outcome.unwrap();
        assert!(result.is_ok(), "actual driver: {result:?}, calls={calls:?}");
        let expected_input = !matches!(
            mode,
            "cancel_before" | "source_changed" | "late_prepare" | "row_swap"
        );
        assert_eq!(
            calls.iter().any(|c| c == "keys:Enter"),
            expected_input,
            "actual input boundary: mode={mode}, calls={calls:?}, stages={stages:?}"
        );
        if matches!(mode, "two" | "first_poll") {
            let file = harness
                ._runtime
                .path()
                .join("o_store")
                .join(channel.to_string())
                .join("codex_provenance");
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            let floor = saved["floors"]
                .as_array()
                .unwrap()
                .iter()
                .find(|floor| {
                    floor["launch"]["episode"]["user_message_id"] == serde_json::json!(30 + i)
                })
                .unwrap();
            assert_eq!(
                floor["launch"]["episode"]["turn_nonce"].as_str(),
                token.turn_nonce(),
                "actual driver must carry token turn nonce instead of execution nonce"
            );
        }
        if matches!(mode, "two" | "first_poll") {
            assert!(
                !expired,
                "positive driver cannot complete by fixture timeout: {stages:?}"
            );
        }
        assert_eq!(
            std::fs::read(row).unwrap(),
            row_before,
            "live caller must not rewrite its episode row"
        );
        polls(3).await;
        if matches!(mode, "close_sync" | "close_readback") {
            assert!(
                !admission::issued_for_tests(channel, 30 + i, token.turn_nonce().unwrap()),
                "fsync/read-back failure cannot issue or attach"
            );
            assert!(harness.port.posts().is_empty());
            handles.pop().unwrap().stop_and_join().await.unwrap();
            drop(close_fault);
            admission::set_ready_for_tests(channel, false);
            admission::forget_channel_for_tests(channel);
            dedupe::binding_events::set_test_root(None);
            dedupe::reset_state_for_tests();
            return;
        }
        let read = harness.channel().codex_provenance();
        let crate::services::tui_o::store::codex_provenance::Sidecar::Present(read) = read else {
            panic!("closed sidecar absent")
        };
        let expected_registry = matches!(mode, "two" | "delivery_off" | "first_poll");
        assert_eq!(
            admission::issued_for_tests(channel, 30 + i, token.turn_nonce().unwrap()),
            expected_registry,
            "actual issuer must wait for durable close/current run: {mode}, stages={stages:?}"
        );
        let expected_closed = matches!(
            mode,
            "two" | "delivery_off" | "cancel_attach" | "late_close" | "first_poll"
        );
        assert_eq!(
            !read.spans().is_empty(),
            expected_closed,
            "closed evidence: mode={mode}, stages={stages:?}"
        );
        if expected_closed {
            let span = read.spans().last().unwrap().clone();
            assert!(span.end.is_some());
            assert_eq!(span.execution.execution_nonce, context.execution_nonce);
            assert_eq!(span.episode.turn_nonce, token.turn_nonce().unwrap());
            let origin = provenance_origin_for_actual_test(&span);
            admission::set_ready_for_tests(channel, true);
            let found = admission::lookup(&origin);
            let expected_issue = matches!(mode, "two" | "delivery_off" | "first_poll");
            assert_eq!(
                found.is_ok(),
                expected_issue,
                "current live run issuer: mode={mode}, stages={stages:?}"
            );
            if let Ok(record) = found {
                assert!(Arc::ptr_eq(&record, &admission::lookup(&origin).unwrap()));
            }
            admission::set_ready_for_tests(channel, mode == "first_poll");
            spans.push(span);
        }
        if mode == "delivery_off" {
            assert!(
                rx.try_iter().next().is_none(),
                "production delivery-off must emit no body/Done/RuntimeReady"
            );
        }
        tokens.push(token);
        mailbox
            .finish_turn_if_token(
                Some(Arc::clone(tokens.last().unwrap())),
                crate::services::turn_orchestrator::QueuePersistenceContext::new(
                    &crate::services::provider::ProviderKind::Codex,
                    "s2b1b-fixture",
                    None,
                ),
            )
            .await;
        if mode != "first_poll" {
            assert!(
                harness.port.posts().is_empty(),
                "held work cannot POST before release: {mode}"
            );
        }
    }
    if mode == "two" {
        assert_ne!(tokens[0].turn_nonce(), tokens[1].turn_nonce());
        assert!(!Arc::ptr_eq(&tokens[0], &tokens[1]));
        assert_eq!(spans[0].execution, spans[1].execution);
        assert!(spans[0].end.unwrap() <= spans[1].start);
        admission::set_ready_for_tests(channel, true);
        polls(4).await;
        assert_eq!(harness.port.posts(), ["first body", "second body"]);
    } else if mode == "first_poll" {
        polls(4).await;
        first_poll_cancel
            .lock()
            .unwrap()
            .take()
            .expect("actual issued record must reach the real first poll")
            .join()
            .unwrap();
        assert_eq!(
            *first_poll_raced.lock().unwrap(),
            Some(false),
            "cancellation passed the actual first-poll record fence"
        );
        assert_eq!(harness.port.posts(), ["first body"]);
        assert!(tokens[0].cancelled.load(Ordering::Acquire));
    } else {
        assert!(
            harness.port.posts().is_empty(),
            "negative/delivery-off cases are not positive delivery PASS"
        );
    }
    assert!(
        fake.take_calls()
            .iter()
            .all(|c| !c.contains("kill-session")),
        "new warm path has no kill authority"
    );
    handles.pop().unwrap().stop_and_join().await.unwrap();
    admission::set_ready_for_tests(channel, false);
    admission::forget_channel_for_tests(channel);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

fn provenance_origin_for_actual_test(
    span: &crate::services::tui_prompt_dedupe::codex_verified::provenance::CodexEpisodeSpan,
) -> crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::OwedOrigin {
    crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::OwedOrigin {
        span: span.clone(),
        range: span.start..span.end.unwrap(),
    }
}
