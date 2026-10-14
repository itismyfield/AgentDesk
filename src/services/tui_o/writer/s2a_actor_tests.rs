use super::*;
use crate::services::provider::CancelToken;
use crate::services::tui_o::store::codex_provenance::submit_floor::{
    SubmitFloor, tests as fixture,
};
use crate::services::tui_o::store::fault::{self, Step};
use crate::services::tui_o::writer::actor;
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
        "cancel_after_payload",
        "cancel_after_enter",
        "wrong_nonce",
        "missing_context",
        "source_changed",
        "wrong_uuid",
        "missing_source",
        "wrong_runtime_marker",
        "missing_options",
        "missing_endpoint",
        "wrong_channel",
    ] {
        actual_driver_case(case).await;
    }
}

#[tokio::test(start_paused = true)]
async fn s2b1b_actual_driver_unknown_permission_posts_without_closed_episode() {
    actual_driver_case("unknown").await;
}

#[tokio::test(start_paused = true)]
async fn s2b1b_actual_driver_source_read_survives_cancellation_during_first_poll() {
    actual_driver_case("first_poll").await;
}

// Fixture guards stay isolated until the blocking driver and its actor have both stopped.
struct ActualDriverIsolation {
    _state: std::sync::MutexGuard<'static, ()>,
    _endpoint: std::sync::MutexGuard<'static, ()>,
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
        _endpoint: crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    };
    let (env_root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    let binary = env_root.path().join("codex");
    let probes = env_root.path().join("probes");
    let unsupported = env_root.path().join("unsupported");
    let signal = probes.with_extension("signal");
    let ack = probes.with_extension("ack");
    let target = match mode {
        "s1w_prepare" | "source_changed" => 2,
        "s1w_enter" => 3,
        _ => 0,
    };
    std::fs::write(&binary, format!(
        "#!/bin/sh\nif [ \"$1 $2\" = 'resume --help' ]; then echo probe >> '{}'; n=$(wc -l < '{}'); if [ \"$n\" = '{}' ]; then touch '{}'; while [ ! -f '{}' ]; do sleep 0.002; done; fi; if [ -f '{}' ]; then echo unsupported; else echo --dangerously-bypass-hook-trust; fi; else echo 'codex-cli 0.157.1'; fi\n",
        probes.display(), probes.display(), target, signal.display(), ack.display(), unsupported.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _hooks = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        std::ffi::OsStr::new("1"),
    );
    if mode == "s1w_support" {
        std::fs::write(&unsupported, "unsupported").unwrap();
    }
    let channel = crate::services::codex_tui::canary::CANARY_CHANNEL;
    let tmux = crate::services::codex_tui::canary::CANARY_TMUX;
    let native_id = "019e660d-4859-7522-9cee-8ba7c4e7c743";
    let _selected = test_override::force_candidates(&[(channel, CodexTui)]);
    admission::forget_channel_for_tests(channel);
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
    let canonical = PreparedIncarnation::create(context.clone()).unwrap().path;
    let archive_dir =
        std::path::Path::new(&context.owner_runtime_root).join("runtime/binding_evidence/codex");
    let archive = archive_dir.join(format!("{}.json", context.execution_nonce));
    let identity_paths = [canonical, archive, source.path.clone()];
    let identity_before: Vec<_> = identity_paths
        .iter()
        .map(|path| {
            (
                std::fs::read(path).unwrap(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect();
    let archive_count = std::fs::read_dir(&archive_dir).unwrap().count();
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
    let fake = crate::services::provider_teardown::tests::test_support::FakeTmux::install(tmux);
    let _binary = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CODEX_PATH",
        &binary,
    );
    assert_eq!(
        std::env::var_os("AGENTDESK_CODEX_PATH").as_deref(),
        Some(binary.as_os_str())
    );
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
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(native_id),
        "production Unknown selects the actual proven UUID"
    );
    harness.gate.acquired();
    let _ = fake.take_calls();
    let (stop, stopped) = watch::channel(false);
    let (_command_port, receiver) = actor::codex_submission::Port::new(1);
    let log = crate::services::tui_o::writer::binding::ChannelBindingLog::new(
        channel,
        ShadowProvider::Codex,
    );
    let task = actor::codex_submission::spawning(receiver, || {
        actor::spawn_projecting(
            &WriterConfig { enabled: true },
            None,
            writer_over(&harness, harness.channel()),
            ShadowProvider::Codex,
            Arc::new(log),
            (
                stopped,
                watch::channel(false).0,
                watch::channel(None).0,
                actor::Owing::default(),
            ),
        )
    })
    .unwrap();
    polls(3).await;
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(ChannelId::new(channel));
    let mut outcomes = Vec::new();
    let mut tokens = Vec::new();
    let post_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (i, body) in [(0, "first body"), (1, "second body")]
        .into_iter()
        .take(if mode == "two" { 2 } else { 1 })
    {
        let token = Arc::new(CancelToken::new());
        if mode == "first_poll" {
            let owner = Arc::clone(&token);
            let observed = Arc::clone(&post_cancelled);
            *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
                owner.publish_cancel("first poll stop");
                observed.store(owner.cancelled.load(Ordering::Acquire), Ordering::Release);
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
        let row_modified = std::fs::metadata(&row).unwrap().modified().unwrap();
        let worker_context = context.clone();
        let worker_token = Arc::clone(&token);
        let path = source.path.clone();
        let canonical = identity_paths[0].clone();
        let probe_path = probes.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:9".into(),
        );
        let worker = tokio::task::spawn_blocking(move || {
            use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState};
            use std::io::Write;
            dedupe::binding_events::set_test_root(worker_context.provider_root.as_deref());
            let endpoint = if matches!(mode, "s1w_initial" | "s1w_rollback" | "missing_endpoint") {
                drop(endpoint);
                None
            } else {
                Some(endpoint)
            };
            if mode == "s1w_rollback" {
                crate::services::codex::SOURCE_MODE_TEST.with(|slot| slot.set(Some(crate::services::codex_tui::session::source_observation::CodexSourceMode::Legacy)));
            }
            match mode {
                "cancel_before" => worker_token.publish_cancel("before input"),
                "wrong_nonce" => std::fs::write(
                    crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce"),
                    "other-nonce",
                )
                .unwrap(),
                "missing_context" => std::fs::remove_file(&canonical).unwrap(),
                "wrong_uuid" => {
                    let bytes = std::fs::read_to_string(&path).unwrap();
                    std::fs::write(
                        &path,
                        bytes.replace(native_id, "019e660d-4859-7522-9cee-8ba7c4e7c744"),
                    )
                    .unwrap();
                }
                "missing_source" => std::fs::remove_file(&path).unwrap(),
                "wrong_runtime_marker" => std::fs::write(
                    crate::services::tmux_common::session_temp_path(
                        tmux,
                        crate::services::tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
                    ),
                    "claude-tui",
                )
                .unwrap(),
                "missing_options" => {
                    let options_path = crate::services::tmux_common::session_temp_path(
                        tmux,
                        "codex-tui-launch-options.sha256",
                    );
                    std::fs::remove_file(options_path).unwrap();
                }
                _ => (),
            }
            // A probe handshake changes real eligibility or descriptor evidence at an effect boundary.
            let signal = probe_path.with_extension("signal");
            let ack = probe_path.with_extension("ack");
            let changed_path = path.clone();
            let (finished, done) = (
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
            let monitor_done = Arc::clone(&done);
            let monitor = std::thread::spawn(move || {
                let mut endpoint = endpoint;
                while !monitor_done.load(Ordering::Acquire) {
                    if signal.exists() {
                        if mode == "source_changed" {
                            std::fs::rename(
                                &changed_path,
                                changed_path.with_extension("old-inode"),
                            )
                            .unwrap();
                            std::fs::copy(changed_path.with_extension("old-inode"), &changed_path)
                                .unwrap();
                        } else {
                            endpoint.take();
                        }
                        std::fs::write(&ack, "changed").unwrap();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                endpoint
            });
            let ready_pane = "╭──────────────────────────────────────────────╮\n│ ▌                                            │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
            let draft = "╭──────────────────────────────────────────────╮\n│ first ▌                                      │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
            let native_path = path.clone();
            let enter_token = Arc::clone(&worker_token);
            let spy = SpyGuard::install(SpyState {
                fail_send: (mode == "no_receipt").then(|| (1, Err("unacknowledged Enter".into()))),
                cancel_after_send: (mode == "cancel_after_payload")
                    .then(|| (0, Arc::clone(&worker_token))),
                captures: [ready_pane, draft, ready_pane, ready_pane]
                    .into_iter()
                    .map(|s| Some(s.into()))
                    .collect(),
                on_enter: Some(Box::new(move || {
                    let mut file = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&native_path)
                        .unwrap();
                    let turn = format!("native-{i}");
                    for record in [
                        serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
                        serde_json::json!({"type":"response_item","payload":{"type":"message","id":format!("body-{i}"),"role":"assistant","content":[{"type":"output_text","text":body}]}}),
                        serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":body}}),
                    ] {
                        writeln!(file, "{record}").unwrap();
                    }
                    if mode == "cancel_after_enter" {
                        enter_token.publish_cancel("after Enter");
                    }
                })),
                ..SpyState::default()
            });
            let boundaries = Arc::new(Mutex::new(Vec::<String>::new()));
            let stages = Arc::clone(&boundaries);
            let hook_token = Arc::clone(&worker_token);
            crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |stage| {
                    stages.lock().unwrap().push(stage.to_owned());
                    if stage.starts_with("held:") {
                        hook_token.publish_cancel("observed guard refusal");
                    }
                }));
            });
            let expired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (watch_done, timeout, stopper) = (
                Arc::clone(&finished),
                Arc::clone(&expired),
                Arc::clone(&worker_token),
            );
            let watchdog = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !watch_done.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if !watch_done.load(Ordering::Acquire) {
                    timeout.store(true, Ordering::Release);
                    stopper.publish_cancel("fixture deadline");
                }
            });
            let mut requested = worker_context.clone();
            if mode == "wrong_channel" {
                requested.channel_id = Some(channel + 1);
            }
            let result = crate::services::codex::actual_submit_tests::driver(
                &requested,
                native_id,
                "first",
                worker_token,
                tx,
            );
            finished.store(true, Ordering::Release);
            done.store(true, Ordering::Release);
            watchdog.join().unwrap();
            let endpoint = monitor.join().unwrap();
            crate::services::codex::SOURCE_MODE_TEST.with(|slot| slot.set(None));
            crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY
                .with(|slot| slot.borrow_mut().take());
            let observed = (
                result,
                spy.calls(),
                expired.load(Ordering::Acquire),
                boundaries.lock().unwrap().clone(),
            );
            drop(endpoint);
            observed
        });
        while !worker.is_finished() {
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        outcomes.push((
            worker.await.unwrap(),
            rx.try_iter().collect::<Vec<_>>(),
            row.clone(),
            row_before,
            row_modified,
        ));
        tokens.push(token);
        mailbox
            .finish_turn_if_token(
                Some(Arc::clone(tokens.last().unwrap())),
                crate::services::turn_orchestrator::QueuePersistenceContext::new(
                    &crate::services::provider::ProviderKind::Codex,
                    "source-read-fixture",
                    None,
                ),
            )
            .await;
        polls(3).await;
    }
    polls(4).await;
    if stop.send(true).is_err() {
        assert!(
            task.is_finished(),
            "source guard may already have halted the actor"
        );
    }
    task.await.unwrap();
    if mode == "two" {
        assert_eq!(
            dedupe::binding_events::codex::read_ownership(&context)
                .unwrap()
                .verified
                .unwrap()
                .source,
            source,
            "both caller turns keep the same execution source proof"
        );
    }
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
    for token in &tokens {
        assert!(
            !admission::issued_for_tests(
                channel,
                if std::sync::Arc::ptr_eq(token, &tokens[0]) {
                    30
                } else {
                    31
                },
                token.turn_nonce().unwrap()
            ),
            "source delivery requires no episode issuance"
        );
    }
    admission::forget_channel_for_tests(channel);
    let expects_enter = matches!(
        mode,
        "two" | "unknown" | "first_poll" | "no_receipt" | "cancel_after_enter"
    );
    let expects_payload = expects_enter || matches!(mode, "cancel_after_payload" | "s1w_enter");
    for ((result, calls, expired, stages), frames, row, row_before, row_modified) in outcomes {
        assert!(result.is_ok(), "{mode}: {result:?}; {calls:?}; {stages:?}");
        assert!(
            !expired,
            "watchdog cannot be PASS: {mode}; {stages:?}; {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c.as_str(), "literal:first" | "load:first"))
                .count(),
            usize::from(expects_payload),
            "payload once: {mode}; {calls:?}"
        );
        assert_eq!(
            calls.iter().filter(|c| c.as_str() == "keys:Enter").count(),
            usize::from(expects_enter),
            "Enter once: {mode}; {calls:?}"
        );
        assert_eq!(
            std::fs::read(&row).unwrap(),
            row_before,
            "caller episode bytes preserved"
        );
        assert_eq!(
            std::fs::metadata(row).unwrap().modified().unwrap(),
            row_modified,
            "caller episode mtime preserved"
        );
        if mode.starts_with("s1w_") {
            assert!(
                stages.iter().any(|s| s.contains("HooksUnavailable")
                    || (matches!(mode, "s1w_prepare" | "s1w_enter") && s.contains("Conflict"))),
                "real eligibility refusal: {stages:?}"
            );
            assert!(
                frames.is_empty(),
                "held driver emits no body/Done/Error/RuntimeReady/handback"
            );
            assert!(
                !stages
                    .iter()
                    .any(|s| matches!(s.as_str(), "prepare_ack" | "close_ack" | "submitted")),
                "no episode admission required"
            );
            if matches!(mode, "s1w_initial" | "s1w_rollback" | "s1w_support") {
                assert!(calls.is_empty(), "initial refusal before pane transport");
            }
            for (path, before) in identity_paths.iter().zip(&identity_before) {
                assert_eq!(
                    &(
                        std::fs::read(path).unwrap(),
                        std::fs::metadata(path).unwrap().modified().unwrap()
                    ),
                    before,
                    "identity bytes/mtime: {path:?}"
                );
            }
            assert_eq!(
                std::fs::read_dir(&archive_dir).unwrap().count(),
                archive_count
            );
        }
        assert!(
            calls.iter().all(|c| !c.contains("kill")),
            "no cleanup or fallback"
        );
    }
    assert!(
        matches!(
            harness.channel().codex_provenance(),
            crate::services::tui_o::store::codex_provenance::Sidecar::Missing
        ),
        "source reading does not require a closed sidecar"
    );
    if mode == "two" {
        assert_ne!(tokens[0].turn_nonce(), tokens[1].turn_nonce());
        assert!(!Arc::ptr_eq(&tokens[0], &tokens[1]));
    }
    let expected: Vec<_> = if expects_enter {
        if mode == "two" {
            vec!["first body", "second body"]
        } else {
            vec!["first body"]
        }
    } else {
        vec![]
    };
    assert_eq!(harness.port.posts(), expected, "real POST: {mode}");
    let store = harness.channel();
    let ledger = store.ledger();
    let posted = (0..ledger.next_serial())
        .filter_map(|i| ledger.piece(i))
        .filter(|p| {
            matches!(
                p.outcome,
                Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
                    _
                ))
            )
        })
        .count();
    assert_eq!(posted, expected.len(), "durable Posted: {mode}");
    if mode == "first_poll" {
        assert!(
            post_cancelled.load(Ordering::Acquire),
            "source POST remains independent of episode cancellation"
        );
        assert!(tokens[0].cancelled.load(Ordering::Acquire));
    }
    if mode == "s1w_support" {
        assert!(std::fs::read_to_string(&probes).unwrap().contains("probe"));
    }
    if matches!(mode, "s1w_prepare" | "s1w_enter" | "source_changed") {
        assert!(
            signal.exists() && ack.exists(),
            "real guard fault handshake must execute"
        );
        assert_eq!(
            std::fs::read_to_string(&probes).unwrap().lines().count(),
            target,
            "fault must run at the selected actual effect probe"
        );
    }
    assert!(
        fake.take_calls().iter().all(|c| ![
            "kill-session",
            "new-session",
            "send-keys",
            "paste-buffer"
        ]
        .iter()
        .any(|effect| c.starts_with(effect))),
        "no cleanup/fallback/replay effect"
    );
}

#[tokio::test(start_paused = true)]
async fn s1w_actual_warm_missing_endpoint_refuses_before_capture() {
    actual_driver_case("s1w_initial").await;
}

#[tokio::test(start_paused = true)]
async fn s1w_actual_warm_rechecks_eligibility_before_payload() {
    actual_driver_case("s1w_prepare").await;
}

#[tokio::test(start_paused = true)]
async fn s1w_actual_warm_rechecks_before_enter_without_claiming_no_attempt() {
    actual_driver_case("s1w_enter").await;
}

#[tokio::test(start_paused = true)]
async fn s1w_pinned_verified_warm_cannot_skip_checks_after_launch_mode_rollback() {
    actual_driver_case("s1w_rollback").await;
}

#[tokio::test(start_paused = true)]
async fn s1w_actual_warm_support_refusal_is_not_an_empty_fixture_pass() {
    actual_driver_case("two").await;
    actual_driver_case("s1w_support").await;
}
