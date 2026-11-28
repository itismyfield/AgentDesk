//! An evidenced Pending launch can lose its pane before its owner ever reserves the input.
use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::herdr_launch::unplaced::QUIET;

struct WakeGuard;

impl Drop for WakeGuard {
    fn drop(&mut self) {
        set_owner_wake_for_tests(ChannelId::new(CHANNEL), None);
    }
}

fn start(
    fx: &Fixture,
    intake: &Intake,
    front: OwnerFront,
) -> std::thread::JoinHandle<Result<(), String>> {
    let runtime = intake.runtime.handle().clone();
    let shared = intake.shared.clone();
    let install = fx.rig.thread_installer_with_launch(NODE, KEY, SESSION);
    let config = crate::config_live_reload::current().unwrap();
    let boot = cutover::boot_snapshot().unwrap();
    let root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let http = intake._api.http.clone();
    std::thread::spawn(move || {
        install();
        QUIET.with(|quiet| quiet.set(Some(Duration::ZERO)));
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(root.as_deref());
        let _boot = test_override::force_boot(boot);
        let _config = crate::config::session_hosts::force_config_for_test(&config);
        let _admission = open_admission();
        let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
        runtime.block_on(async {
            let (entered, resume) =
                discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                    .await
                    .pause();
            let start = dispatch::start_idle_for_test(shared, ChannelId::new(CHANNEL), front);
            let recording = async {
                tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
                let guard = test_rest::install(http);
                resume.send(()).unwrap();
                guard
            };
            let (result, _rest) = tokio::join!(tokio::time::timeout(LIMIT, start), recording);
            result.unwrap()
        })
    })
}

fn launch(
    fx: &Fixture,
    intake: &Intake,
    front: OwnerFront,
    prior: (&str, &str),
    lose_before_attach: bool,
) -> (std::thread::JoinHandle<Result<(), String>>, String, Started) {
    let (command, command_seen) = std::sync::mpsc::channel();
    let (allow_evidence, allowed) = std::sync::mpsc::channel();
    let mut command = Some(command);
    fx.rig.on_send(move |_| {
        if let Some(command) = command.take() {
            command.send(()).unwrap();
            crate::test_support::wait::recv_within(&allowed, LIMIT);
        }
    });
    let mut events = fx.ingress.state.subscribe();
    let caller = start(fx, intake, front);
    crate::test_support::wait::recv_within(&command_seen, LIMIT);
    let HostedRecord::Known(pending) = fx.record() else {
        panic!("a create must have installed its Pending row");
    };
    assert_eq!(pending.state, HostedState::Pending);
    let nonce = pending.execution_nonce;
    assert_ne!(nonce, prior.0);
    let context = context_of(&nonce);
    fx.rig.run_provider(&context, false);
    fx.rig.foreground(&[20]);
    fx.rig.show_panes(&[PANE]);
    allow_evidence.send(()).unwrap();
    fx.rt.block_on(async {
        tokio::time::timeout(LIMIT, async {
            loop {
                let raw: Value =
                    sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                        .bind(CHANNEL.to_string())
                        .fetch_one(&fx.pool)
                        .await
                        .unwrap();
                if matches!(HostedRecord::decode(Some(&raw)), HostedRecord::Known(record)
                    if record.execution_nonce == nonce && record.expected.is_some())
                {
                    break;
                }
                assert!(
                    !caller.is_finished(),
                    "launch ended before recording evidence"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
    let session =
        crate::services::tui_prompt_dedupe::binding_context::execution_context("claude", &nonce)
            .unwrap()
            .expected_native_session_id
            .unwrap();
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        &session,
        fx.home().as_deref(),
    )
    .unwrap();
    let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
        (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.clone().into_os_string())
    })
    .encode()
    .unwrap();
    let started = Started {
        session,
        path,
        envelope,
    };
    if lose_before_attach {
        fx.rig.show_panes(&[]);
        fx.rig.exists(20, false);
    } else {
        crate::services::claude_tui::readiness::test_support::pre_source(
            fx.logical(),
            &started.path,
        );
    }
    let source = if started.session == prior.1 {
        "resume"
    } else {
        "startup"
    };
    assert_eq!(
        started.hook(
            fx,
            "SessionStart",
            &json!({
                "session_id":started.session,"transcript_path":started.path,"source":source
            })
        ),
        202
    );
    while let Ok(event) = events.try_recv() {
        crate::services::claude_tui::hook_server::publish_hook_event_for_tests(event);
    }
    (caller, nonce, started)
}

fn tick_and_grant(intake: &Intake, original: &OwnerFront) -> Option<OwnerFront> {
    let original = original.clone();
    intake
        .runtime
        .block_on(intake.mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let snapshot = owner.json()?;
            if let Some(due) = snapshot["retry"]["wait"]["due_at_ms"]
                .as_u64()
                .or_else(|| snapshot["head_injection"]["confirmation_deadline_ms"].as_u64())
            {
                owner.tick(due)?;
            }
            let front = owner.front();
            if let Some(front) = &front {
                assert_eq!(front.0, original.0);
                assert_eq!(front.1.message_id, original.1.message_id);
                assert_eq!(front.1.queued_generation, original.1.queued_generation);
                assert!(owner.grant_front(front)?);
            }
            Ok(front)
        }))
        .unwrap()
}

fn snapshot(intake: &Intake) -> Value {
    intake
        .runtime
        .block_on(
            intake
                .mailbox
                .v2_request(|slot| slot.as_ref().unwrap().json()),
        )
        .unwrap()
}

fn assert_wait(intake: &Intake, front: &OwnerFront, nonce: &str) {
    let state = snapshot(intake);
    let wait = &state["retry"]["wait"];
    assert_eq!(
        wait["cause"],
        json!({"kind":"pending_attach","nonce":nonce}),
        "{state}"
    );
    assert_eq!(wait["ticket"], json!(front.0));
    assert_eq!(state["items"], json!([front.1]));
    assert_eq!(state["retained_inputs"], json!([]));
    assert!(state["head_injection"].is_null());
}

fn placed_attach(lose_before_attach: bool) {
    let fx = Fixture::new("placed-attach-owner", None);
    let (old_nonce, previous) = reconnect::launch(&fx, &Arc::new(Launcher::default()));
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:1".into(),
    );
    assert!(set_owner_wake_for_tests(ChannelId::new(CHANNEL), Some(Arc::new(|| {}))).is_none());
    let _wake = WakeGuard;
    assert_eq!(
        previous.hook(
            &fx,
            "Stop",
            &json!({"session_id":previous.session,"transcript_path":previous.path}),
        ),
        202,
    );
    let original_runtime;
    let mut intake = Intake::with_launch(
        &fx,
        &previous,
        ExpectedPin::Verified,
        Arc::default(),
        (true, None),
        None,
    );
    original_runtime = recording_runtime(&mut intake, &fx, true);
    let HostedRecord::Known(mut old) = fx.record() else {
        panic!("bootstrap row")
    };
    old.state = HostedState::Retired;
    fx.store(&old);
    seed_resume_cursor(fx._root.path(), &previous);
    fx.rig.answer(
        "ping",
        json!({"type":"pong","version":"0.9.3","protocol":22}),
    );
    fx.rig.answer("workspace.create",json!({"type":"workspace_created","root_pane":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":0,"cwd":fx.cwd.path()}}));
    fx.rig.answer("pane.process_info", json!({"type":"ok"}));
    fx.rig.show_panes(&[]);
    let front = intake.runtime.block_on(async {
        intake
            .shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&ChannelId::new(CHANNEL))
            .unwrap()
            .session_id = None;
        intake.front().await
    });
    assert!(front.0.is_some());
    let before = fx.rig.sends().len();
    let (first, nonce, started) = launch(
        &fx,
        &intake,
        front.clone(),
        (&old_nonce, &previous.session),
        lose_before_attach,
    );
    let after_command = before + 1;
    if lose_before_attach {
        let result = crate::test_support::wait::join_within(first, LIMIT);
        assert!(result.is_err(), "{result:?}");
        intake
            .runtime
            .block_on(super::wait_for_idle_turn_to_unwind(&intake));
        assert_eq!(fx.rig.sends().len(), after_command);
        assert_eq!(fx.row(), Some(HostedState::Pending));
        let first_state = snapshot(&intake);

        // Let RED reach the retire boundary even if the first refusal spent a start failure.
        let retry = tick_and_grant(&intake, &front).expect("the same front can retry");
        let retry = start(&fx, &intake, retry);
        assert!(crate::test_support::wait::join_within(retry, LIMIT).is_err());
        intake
            .runtime
            .block_on(super::wait_for_idle_turn_to_unwind(&intake));
        assert_eq!(fx.row(), Some(HostedState::Retired));
        assert_wait(&intake, &front, &nonce);
        assert_eq!(
            first_state["retry"]["wait"]["cause"],
            json!({"kind":"pending_attach","nonce":nonce})
        );
        assert_eq!(fx.rig.sends().len(), after_command);

        let retry = tick_and_grant(&intake, &front).unwrap();
        let (successor, new_nonce, new) =
            launch(&fx, &intake, retry, (&nonce, &started.session), false);
        assert_ne!(new_nonce, nonce);
        fx.finished.store(false, Ordering::SeqCst);
        fx.answer_after(&new, after_command + 1);
        assert_eq!(
            crate::test_support::wait::join_within(successor, LIMIT),
            Ok(())
        );
        let sends = fx.rig.sends();
        assert_eq!(sends.len(), after_command + 3);
        assert!(sends[after_command + 1]["text"]
            .as_str()
            .unwrap()
            .contains(INPUT));
        assert_eq!(sends[after_command + 2]["keys"], json!(["enter"]));
        let state = snapshot(&intake);
        assert_eq!(state["items"], json!([]));
        assert_eq!(state["retained_inputs"], json!([]));
    } else {
        assert!(wait_for(&fx.finished, "the owner's first key", || fx
            .rig
            .sends()
            .len()
            == after_command + 2));
        assert_eq!(fx.row(), Some(HostedState::Pending));
        let state = snapshot(&intake);
        assert_eq!(state["head_injection"]["phase"], "may_have_sent", "{state}");
        fx.rig.show_panes(&[]);
        fx.rig.exists(20, false);
        intake.runtime.block_on(async {
            let cancel = intake.mailbox.cancel_token().await.unwrap().unwrap();
            cancel.cancelled.store(true, Ordering::SeqCst);
        });
        let _ = crate::test_support::wait::join_within(first, LIMIT);
        intake
            .runtime
            .block_on(super::wait_for_idle_turn_to_unwind(&intake));
        let install = fx.rig.thread_installer_with_launch(NODE, KEY, SESSION);
        install();
        let _runtime = fx.rt.enter();
        QUIET.with(|quiet| quiet.set(Some(Duration::ZERO)));
        discord::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared(
            &intake.shared,
            &ProviderKind::Claude,
        );
        QUIET.with(|quiet| quiet.set(None));
        drop(_runtime);
        assert_eq!(fx.row(), Some(HostedState::Retired));
        assert!(tick_and_grant(&intake, &front).is_none());
        assert_eq!(fx.rig.sends().len(), after_command + 2);
    }
    drop(intake);
    drop(original_runtime);
}

#[test]
fn s2b1_placed_attach_missing_before_key_retries_same_ticket_pg() {
    placed_attach(true);
}

#[test]
fn s2b1_placed_attach_missing_after_key_does_not_resubmit_pg() {
    placed_attach(false);
}
