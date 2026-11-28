//! An evidenced Pending launch can lose its pane before its owner ever reserves the input.
use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::discord::queue_io::input_owner::drive;
use crate::services::herdr_launch::unplaced::QUIET;
use std::sync::mpsc;

struct WakeGuard;

impl Drop for WakeGuard {
    fn drop(&mut self) {
        set_owner_wake_for_tests(ChannelId::new(CHANNEL), None);
    }
}

struct KeyReply {
    sent: mpsc::Sender<()>,
    reply: mpsc::Receiver<()>,
}

#[derive(Clone, Copy)]
enum PaneLoss {
    BeforeAttach,
    BeforeKey(PreKeyRecovery),
    AfterKey,
}

#[derive(Clone, Copy)]
enum PreKeyRecovery {
    Hold,
    Requeue,
    Restart,
}

fn start(
    fx: &Fixture,
    intake: &Intake,
    front: OwnerFront,
    launch_signal: Option<mpsc::Sender<Result<(), String>>>,
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
        let result = runtime.block_on(async {
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
        });
        if let (Some(send), Err(error)) = (launch_signal, &result) {
            let _ = send.send(Err(error.clone()));
        }
        result
    })
}

fn launch(
    fx: &Fixture,
    intake: &Intake,
    front: OwnerFront,
    prior: (&str, &str),
    lose_before_attach: bool,
    mut key_reply: Option<KeyReply>,
) -> (std::thread::JoinHandle<Result<(), String>>, String, Started) {
    let (command, command_seen) = mpsc::channel();
    let (allow_evidence, allowed) = mpsc::channel();
    if key_reply.is_some() {
        fx.rig.answer_sends_then_leave_unanswered(2);
    }
    let launch_signal = command.clone();
    let mut command = Some(command);
    fx.rig.on_send(move |params| {
        if let Some(command) = command.take() {
            command.send(Ok(())).unwrap();
            crate::test_support::wait::recv_within(&allowed, LIMIT);
        } else if let Some(key) = key_reply.take_if(|_| params["keys"] == json!(["enter"])) {
            key.sent.send(()).unwrap();
            crate::test_support::wait::recv_within(&key.reply, LIMIT);
        }
    });
    let mut events = fx.ingress.state.subscribe();
    let caller = start(fx, intake, front, Some(launch_signal));
    crate::test_support::wait::recv_within(&command_seen, LIMIT)
        .expect("the owner starts its successor before the binding read can fail");
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
    intake
        .runtime
        .block_on(tick_and_grant_on_mailbox(&intake.mailbox, original.clone()))
        .unwrap()
}

async fn tick_and_grant_on_mailbox(
    mailbox: &ChannelMailboxHandle,
    original: OwnerFront,
) -> Result<Option<OwnerFront>, String> {
    mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            if let Some(due) = owner.settle_due() {
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
        })
        .await
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
    let original = front.1.clone();
    let failures =
        intake
            .runtime
            .block_on(intake.mailbox.v2_request(move |slot| {
                Ok(slot.as_ref().unwrap().start_failures_for_test(&original))
            }))
            .unwrap();
    assert_eq!(failures, 0, "the pane loss does not spend a start failure");
}

fn placed_attach(loss: PaneLoss) {
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
    fx.rt.block_on(async {
        let id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE channel_id=$1")
            .bind(CHANNEL.to_string())
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        assert!(
            crate::db::dispatched_sessions::provider_resume_fence::set_for_execution_pg(
                &fx.pool, id, &old,
            )
            .await
            .unwrap()
        );
    });
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
    let (sent, key_seen) = mpsc::channel();
    let (release, reply) = mpsc::channel();
    let (capture_ready, resume_capture) = match loss {
        PaneLoss::BeforeKey(_) => {
            let (entered, ready) = mpsc::channel();
            let (resume, continued) = mpsc::channel();
            let continued = Mutex::new(Some(continued));
            let (mailbox, runtime) = (intake.mailbox.clone(), intake.runtime.handle().clone());
            fx.rig.on_reply(move |call| {
                use crate::services::session_host::herdr_socket_rig_tests::HerdrRequest;
                if !matches!(call.request, HerdrRequest::PaneRead { .. }) {
                    return;
                }
                let mut continued = continued.lock().unwrap();
                if continued.is_none() {
                    return;
                }
                let reserved = tokio::task::block_in_place(|| {
                    runtime.block_on(
                        mailbox
                            .v2_request(|slot| Ok(slot.as_ref().unwrap().active_attempt().is_ok())),
                    )
                })
                .unwrap();
                if reserved {
                    let continued = continued.take().unwrap();
                    entered.send(()).unwrap();
                    crate::test_support::wait::recv_within(&continued, LIMIT);
                }
            });
            (Some(ready), Some(resume))
        }
        _ => (None, None),
    };
    let (first, nonce, started) = launch(
        &fx,
        &intake,
        front.clone(),
        (&old_nonce, &previous.session),
        matches!(loss, PaneLoss::BeforeAttach),
        matches!(loss, PaneLoss::AfterKey).then_some(KeyReply { sent, reply }),
    );
    assert_ne!(started.session, previous.session);
    assert!(
        fx.rt
            .block_on(
                crate::db::dispatched_sessions::provider_resume_fence::load_for_owner_pg(
                    &fx.pool, &old.owner,
                )
            )
            .unwrap()
            .is_none()
    );
    let after_command = before + 1;
    match loss {
        PaneLoss::BeforeAttach => {
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
            let retry = start(&fx, &intake, retry, None);
            assert!(crate::test_support::wait::join_within(retry, LIMIT).is_err());
            intake
                .runtime
                .block_on(super::wait_for_idle_turn_to_unwind(&intake));
            assert_eq!(fx.row(), Some(HostedState::Retired));
            assert_wait(&intake, &front, &nonce);
            assert!(
                snapshot(&intake)["retry"]["wait"]["wait_seq"]
                    .as_u64()
                    .unwrap()
                    > first_state["retry"]["wait"]["wait_seq"].as_u64().unwrap()
            );
            assert_eq!(
                first_state["retry"]["wait"]["cause"],
                json!({"kind":"pending_attach","nonce":nonce})
            );
            assert_eq!(fx.rig.sends().len(), after_command);

            assert_successor(
                &fx,
                &intake,
                &front,
                (&nonce, &started.session),
                after_command,
            );
        }
        PaneLoss::BeforeKey(recovery) => {
            crate::test_support::wait::recv_within(&capture_ready.unwrap(), LIMIT);
            let state = snapshot(&intake);
            assert_eq!(
                state["head_injection"]["attempt"]["target"]["execution_nonce"],
                nonce
            );
            assert_eq!(state["head_injection"]["original"], json!(front.1));
            assert_eq!(fx.row(), Some(HostedState::Pending));
            assert_eq!(fx.rig.sends().len(), after_command);
            let head = state["head_injection"].clone();
            fx.rig.show_panes(&[]);
            fx.rig.exists(20, false);
            resume_capture.unwrap().send(()).unwrap();
            assert!(crate::test_support::wait::join_within(first, LIMIT).is_err());
            intake
                .runtime
                .block_on(super::wait_for_idle_turn_to_unwind(&intake));
            let state = snapshot(&intake);
            assert!(state["head_injection"].is_null(), "{state}");
            assert_eq!(state["items"], json!([front.1]));
            assert_eq!(state["retained_inputs"], json!([]));
            assert!(state["retry"]["wait"].is_null());
            match recovery {
                PreKeyRecovery::Hold => {}
                PreKeyRecovery::Requeue => {
                    retain_and_requeue(&fx, &intake, &front, &nonce, &started);
                }
                PreKeyRecovery::Restart => {
                    restore_reserved(&intake, &front, head);
                    let restored = snapshot(&intake);
                    assert!(restored["head_injection"].is_null());
                    assert_eq!(restored["items"], json!([front.1]));
                }
            }
            periodic_retire(&fx, &intake);
            assert!(snapshot(&intake)["retry"]["wait"].is_null());
            assert_eq!(fx.rig.sends().len(), after_command);
            assert_successor(
                &fx,
                &intake,
                &front,
                (&nonce, &started.session),
                after_command,
            );
        }
        PaneLoss::AfterKey => {
            crate::test_support::wait::recv_within(&key_seen, LIMIT);
            assert_eq!(fx.rig.sends().len(), after_command + 2);
            assert_eq!(fx.row(), Some(HostedState::Pending));
            let state = snapshot(&intake);
            assert_eq!(state["head_injection"]["phase"], "may_have_sent", "{state}");
            fx.rig.show_panes(&[]);
            fx.rig.exists(20, false);
            release.send(()).unwrap();
            assert!(crate::test_support::wait::join_within(first, LIMIT).is_err());
            intake
                .runtime
                .block_on(super::wait_for_idle_turn_to_unwind(&intake));
            periodic_retire(&fx, &intake);
            assert!(tick_and_grant(&intake, &front).is_none());
            let state = snapshot(&intake);
            assert_eq!(state["retained_inputs"][0]["reason"]["kind"], "unconfirmed");
            assert_eq!(
                state["retained_inputs"][0]["input"]["original"],
                json!(front.1)
            );
            let mut owner_drive = super::source_retry_drive(&intake);
            let (done, complete) = tokio::sync::mpsc::unbounded_channel();
            Arc::get_mut(&mut owner_drive).unwrap().settled = Some(Arc::new(move || {
                let _ = done.send(());
                Box::pin(async {})
            }));
            intake.runtime.block_on(async {
                let mut complete = complete;
                drive::wake(owner_drive);
                tokio::time::timeout(LIMIT, complete.recv())
                    .await
                    .unwrap()
                    .unwrap();
            });
            assert_eq!(fx.rig.sends().len(), after_command + 2);
        }
    }
    drop(intake);
    drop(original_runtime);
}

fn restore_reserved(intake: &Intake, front: &OwnerFront, head: Value) {
    let mut attempt = head["attempt"].clone();
    assert_eq!(attempt["source"]["kind"], "pre_source");
    attempt["nonce"] = json!(uuid());
    let attempt = serde_json::from_value(attempt).unwrap();
    let payload = serde_json::from_value(head["payload"].clone()).unwrap();
    let channel = ChannelId::new(CHANNEL);
    let persistence =
        discord::queue_persistence_context(&intake.shared, &ProviderKind::Claude, channel);
    intake.runtime.block_on(async {
        let granted = tick_and_grant_on_mailbox(&intake.mailbox, front.clone())
            .await
            .unwrap()
            .unwrap();
        let claim = drive::claim_turn(&intake.mailbox, front.1.message_id, &granted)
            .await
            .unwrap()
            .unwrap();
        intake
            .mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                owner.reserve_claimed((claim.front(), claim.number()), attempt, payload)?;
                assert_eq!(owner.json()?["head_injection"]["phase"], "reserved");
                slot.take();
                Ok(())
            })
            .await
            .unwrap();
        intake
            .mailbox
            .v2_ensure_channel(&persistence, channel)
            .await
            .unwrap();
    });
}

fn periodic_retire(fx: &Fixture, intake: &Intake) {
    fx.rig.thread_installer_with_launch(NODE, KEY, SESSION)();
    let _runtime = fx.rt.enter();
    QUIET.with(|quiet| quiet.set(Some(Duration::ZERO)));
    discord::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared(
        &intake.shared,
        &ProviderKind::Claude,
    );
    QUIET.with(|quiet| quiet.set(None));
    assert_eq!(fx.row(), Some(HostedState::Retired));
}

fn retain_and_requeue(
    fx: &Fixture,
    intake: &Intake,
    front: &OwnerFront,
    nonce: &str,
    started: &Started,
) {
    use crate::services::discord::queue_io::input_owner::{effect, pump};
    let prepared = effect::PreparedPayload {
        frame: front.1.text.clone(),
        sources: vec![front.1.message_id.to_string()],
        attempt: json!({
            "nonce":uuid(), "provider":"claude",
            "target":{"host":"herdr","owner":fx.logical(),"root":fx.rig.socket(),"execution_nonce":nonce},
            "source":{"kind":"pre_source","expected_session_id":started.session,
                "expected_transcript":started.path,"binding_watermark":null}
        }),
    };
    intake.runtime.block_on(async {
        let granted = tick_and_grant_on_mailbox(&intake.mailbox, front.clone())
            .await
            .unwrap();
        let claim = drive::claim_turn(&intake.mailbox, front.1.message_id, &granted)
            .await
            .unwrap()
            .unwrap();
        let clock = Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64);
        let result = pump::submit_front(
            &intake.mailbox,
            Some(claim),
            prepared,
            clock,
            Box::new(|_| Ok(effect::refused("pane unavailable".into(), false))),
        )
        .await;
        assert_eq!(
            result,
            Ok(effect::Completion::NotSent("pane unavailable".into()))
        );
        let original = front.1.clone();
        intake
            .mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                let state = owner.json()?;
                assert_eq!(state["retained_inputs"][0]["disposition"], "known_not_sent");
                assert_eq!(
                    state["retained_inputs"][0]["input"]["original"],
                    json!(original)
                );
                assert!(state["retry"]["wait"].is_null());
                assert_eq!(owner.requeue_known_not_sent()?.len(), 1);
                Ok(())
            })
            .await
            .unwrap();
    });
}

fn assert_successor(
    fx: &Fixture,
    intake: &Intake,
    front: &OwnerFront,
    prior: (&str, &str),
    after_command: usize,
) {
    let original = front.1.clone();
    intake
        .runtime
        .block_on(intake.mailbox.v2_request(move |slot| {
            assert_eq!(slot.as_ref().unwrap().start_failures_for_test(&original), 0);
            Ok(())
        }))
        .unwrap();
    let retry = tick_and_grant(intake, front).unwrap();
    let (successor, new_nonce, new) = launch(fx, intake, retry, prior, false, None);
    assert_ne!(new_nonce, prior.0);
    assert_ne!(new.session, prior.1);
    fx.finished.store(false, Ordering::SeqCst);
    fx.answer_after(&new, after_command + 1);
    assert_eq!(
        crate::test_support::wait::join_within(successor, LIMIT),
        Ok(())
    );
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), after_command + 3);
    assert!(
        sends[after_command + 1]["text"]
            .as_str()
            .unwrap()
            .contains(INPUT)
    );
    assert_eq!(sends[after_command + 2]["keys"], json!(["enter"]));
    let state = snapshot(intake);
    assert_eq!(state["items"], json!([]));
    assert_eq!(state["retained_inputs"], json!([]));
    let original = front.1.clone();
    intake
        .runtime
        .block_on(intake.mailbox.v2_request(move |slot| {
            assert_eq!(slot.as_ref().unwrap().start_failures_for_test(&original), 0);
            Ok(())
        }))
        .unwrap();
}

#[test]
fn s2b1_placed_attach_missing_before_key_retries_same_ticket_pg() {
    placed_attach(PaneLoss::BeforeAttach);
}

#[test]
fn s2b1_placed_attach_missing_after_key_does_not_resubmit_pg() {
    placed_attach(PaneLoss::AfterKey);
}

#[test]
fn s2b1_placed_attach_missing_after_reserve_retries_same_ticket_pg() {
    placed_attach(PaneLoss::BeforeKey(PreKeyRecovery::Hold));
}

#[test]
fn s2b1_placed_attach_missing_known_not_sent_requeues_same_ticket_pg() {
    placed_attach(PaneLoss::BeforeKey(PreKeyRecovery::Requeue));
}

#[test]
fn s2b1_retired_resume_restored_reserved_owner_starts_fresh_pg() {
    placed_attach(PaneLoss::BeforeKey(PreKeyRecovery::Restart));
}
