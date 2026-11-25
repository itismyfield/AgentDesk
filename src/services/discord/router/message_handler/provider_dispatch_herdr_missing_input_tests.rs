//! Actual idle starts, including a queue restored after startup retired its missing pane.
use super::*;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::discord::shared_state::test_rest;

pub(super) fn recording_runtime(
    intake: &mut Intake,
    fx: &Fixture,
    launch: bool,
) -> tokio::runtime::Runtime {
    let install: Box<dyn Fn() + Send + Sync> = if launch {
        Box::new(fx.rig.thread_installer_with_launch(NODE, KEY, SESSION))
    } else {
        Box::new(fx.rig.thread_installer(NODE, KEY, fx.rig.socket(), SESSION))
    };
    let config = crate::config_live_reload::current().unwrap();
    let boot = cutover::boot_snapshot().expect("the managed writer's adopted boot snapshot");
    let root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let http = intake._api.http.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(move || {
            install();
            crate::services::tui_prompt_dedupe::binding_events::set_test_root(root.as_deref());
            WORKER_GUARDS.with(|guards| {
                guards.borrow_mut().extend([
                    Box::new(test_override::force_boot(boot.clone())) as Box<dyn std::any::Any>,
                    Box::new(open_admission()),
                    Box::new(crate::services::herdr_launch::force_writer_accepts(Some(
                        true,
                    ))),
                    Box::new(crate::config::session_hosts::force_config_for_test(&config)),
                    Box::new(test_rest::install(http.clone())),
                ])
            });
        })
        .on_thread_stop(|| WORKER_GUARDS.with(|guards| guards.borrow_mut().clear()))
        .build()
        .unwrap();
    std::mem::replace(&mut intake.runtime, runtime)
}

fn idle_case(missing: bool, restored_after_reconnect: bool, endpoint_differs: bool) {
    let fx = Fixture::new("missing-idle", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let original_runtime;
    let mut intake = Intake::new(&fx, &started);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    original_runtime = recording_runtime(&mut intake, &fx, false);
    fx.rig.answer("pane.process_info", json!({"type":"ok"}));
    if missing {
        fx.rig.show_panes(&[]);
    } else {
        fx.rig.answer("session.snapshot", json!({"type":"ok"}));
    }
    if endpoint_differs {
        fx.rt.block_on(async {
            sqlx::query("UPDATE sessions SET hosted_execution=jsonb_set(hosted_execution,'{location,named_session}', to_jsonb('foreign-session'::text)) WHERE channel_id=$1")
                .bind(CHANNEL.to_string())
                .execute(&fx.pool)
                .await
                .unwrap();
        });
    }
    if restored_after_reconnect {
        crate::services::tui_prompt_dedupe::release_herdr_execution(fx.logical(), &nonce);
        let _entered = intake.runtime.enter();
        crate::services::discord::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared(&intake.shared, &ProviderKind::Claude);
        assert_eq!(fx.row(), Some(HostedState::Retired));
    }
    intake.runtime.block_on(async {
        let key = discord::adk_session::turn_session_key(
            &intake.shared,
            ChannelId::new(CHANNEL),
            &ProviderKind::Claude,
        )
        .await
        .unwrap();
        let host = crate::services::turn_host::for_turn(
            Some(&fx.pool),
            &ProviderKind::Claude,
            CHANNEL,
            Some(&key),
        )
        .await;
        let TurnHost::Herdr(plan) = &host else {
            panic!("the actual idle start is admitted on Herdr");
        };
        assert!(
            plan.row.as_ref().is_some_and(|row| matches!(&row.record,
            HostedRecord::Known(record) if record.state == if restored_after_reconnect {
                HostedState::Retired
            } else { HostedState::Bound })),
            "the idle start reads the seeded row: {:?}",
            plan.row.as_ref().map(|row| &row.record)
        );
        if !restored_after_reconnect {
            assert_eq!(
                claude_handoff::observe_busy(
                    CHANNEL,
                    fx.logical(),
                    &host,
                    &intake.shared.token_hash
                )
                .state(),
                crate::services::claude_tui::host_input::busy::BusyState::Held
            );
        }
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        let mut front = intake.front().await;
        if restored_after_reconnect {
            let old = front.clone();
            intake
                .mailbox
                .v2_request(move |slot| {
                    slot.as_mut().unwrap().hold_front(&old)?;
                    slot.take();
                    Ok(())
                })
                .await
                .unwrap();
            let persistence = discord::queue_persistence_context(
                &intake.shared,
                &ProviderKind::Claude,
                ChannelId::new(CHANNEL),
            );
            intake
                .mailbox
                .v2_ensure_channel(&persistence, ChannelId::new(CHANNEL))
                .await
                .unwrap();
            front = intake
                .mailbox
                .v2_request(|slot| {
                    let owner = slot.as_mut().unwrap();
                    let front = owner.front().unwrap();
                    owner.grant_front(&front)?;
                    Ok(front)
                })
                .await
                .unwrap();
        }
        let original = front.1.clone();
        let before = dispatch::OwnerTurn::probes(CHANNEL);
        let (entered, resume) =
            discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                .await
                .pause();
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        let record = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            let guard = test_rest::install(intake._api.http.clone());
            resume.send(()).unwrap();
            guard
        };
        let (result, _rest) = tokio::join!(tokio::time::timeout(LIMIT, start), record);
        let result = result.unwrap();
        eprintln!("MISSING_IDLE_RESULT {result:?}");
        let after = dispatch::OwnerTurn::probes(CHANNEL);
        assert_eq!(
            after.0 - before.0,
            1,
            "the actual idle intake made the early decision"
        );
        let row: Value =
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                .bind(CHANNEL.to_string())
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        assert_eq!(
            row["state"],
            if missing { "retired" } else { "bound" },
            "{result:?}"
        );
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        if !missing && !endpoint_differs {
            let notices = intake._api.take();
            assert!(
                notices
                    .iter()
                    .all(|call| !call.contains("이 입력을 폐기했습니다")),
                "{notices:?}"
            );
            assert_eq!(state["items"], json!([original]));
            assert!(discord::queue_io::input_owner::held_watch::has_watch(
                CHANNEL
            ));
            assert_eq!(fx.rig.sends().len(), 2);
            return;
        }
        assert!(
            state["items"].as_array().unwrap().is_empty(),
            "idle input stayed queued"
        );
        assert!(
            state["retained_inputs"].as_array().unwrap().is_empty(),
            "refused input was retained"
        );
        assert!(!discord::queue_io::input_owner::held_watch::has_watch(
            CHANNEL
        ));
        assert!(
            !intake
                .shared
                .turn_view_reconciler
                .ops()
                .iter()
                .any(|op| op.add && op.emoji == '📬')
        );
        let calls = intake._api.take();
        if endpoint_differs {
            assert!(
                calls
                    .iter()
                    .any(|call| call.contains("local execution owner or endpoint differs")),
                "{calls:?}"
            );
        }
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.contains("이 입력을 폐기했습니다"))
                .count(),
            1,
            "one channel reason"
        );
        assert_eq!(fx.rig.sends().len(), 2, "no input reached the old pane");
    });
    drop(intake);
    drop(original_runtime);
}

#[test]
fn a_missing_idle_bound_pane_retires_and_a_failed_restart_discards_the_front_pg() {
    idle_case(true, false, false);
}

#[test]
fn an_unreadable_idle_bound_pane_keeps_its_input_pg() {
    idle_case(false, false, false);
}

#[test]
fn a_confirmed_endpoint_mismatch_discards_the_front_with_one_notice_pg() {
    idle_case(false, false, true);
}

#[test]
fn a_front_restored_after_missing_reconnect_uses_the_same_disposal_rule_pg() {
    idle_case(true, true, false);
}

#[test]
fn a_busy_registration_failure_keeps_live_input_and_submits_once_on_idle_pg() {
    use crate::services::claude_tui::host_input::busy::BusyState;
    use crate::services::discord::queue_io::input_owner::held_watch;

    let fx = Fixture::new("busy-registration-reobserve", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let guards = (
        fx.rig.registry_on_this_thread(),
        open_admission(),
        discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL),
        crate::services::herdr_launch::force_writer_accepts(Some(true)),
    );
    let original_runtime;
    let mut intake = Intake::new(&fx, &started);
    original_runtime = recording_runtime(&mut intake, &fx, false);
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    let screen_at = |text: &str| {
        json!({"type":"pane_read","read":{
            "pane_id":PANE, "source":"recent_unwrapped", "text":text,
            "revision":2, "truncated":false
        }})
    };
    fx.rig
        .answer("pane.read", screen_at(&screen.replace("Baked", "Thinking")));
    let now = chrono::Utc::now().to_rfc3339();
    append(
        &started.path,
        &[
            json!({"type":"user","uuid":uuid(),"sessionId":started.session,"timestamp":now,
            "message":{"role":"user","content":"keep working"}}),
            json!({"type":"assistant","uuid":uuid(),"apiBlockIndex":0,"sessionId":started.session,
            "timestamp":now,"message":{"id":"registration-working","role":"assistant",
            "content":[{"type":"text","text":"working"}]}}),
        ],
    );
    let before = fx.rig.sends().len();
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let channel = ChannelId::new(CHANNEL);
        let key = discord::adk_session::turn_session_key(&intake.shared, channel, &ProviderKind::Claude)
            .await.unwrap();
        let host = crate::services::turn_host::for_turn(Some(&fx.pool), &ProviderKind::Claude, CHANNEL, Some(&key)).await;
        assert_eq!(claude_handoff::observe_busy(CHANNEL, fx.logical(), &host, &intake.shared.token_hash).state(), BusyState::Busy);
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        let front = intake.front().await;
        let original = front.1.clone();
        let ticket = front.0.clone();
        let parked = started.path.with_extension("registration-race");
        let path = started.path.clone();
        let saved = parked.clone();
        dispatch::OwnerTurn::before_busy_registration(CHANNEL, move || {
            std::fs::rename(path, saved).unwrap();
        });
        let rest = test_rest::install(intake._api.http.clone());
        let result = tokio::time::timeout(LIMIT, dispatch::start_idle_for_test(intake.shared.clone(), channel, front)).await.unwrap();
        assert!(parked.exists(), "the Busy observation reached registration before its transcript disappeared: {result:?}");
        let state = intake.mailbox.v2_request(|slot| slot.as_ref().unwrap().json()).await.unwrap();
        let before_recovery = intake._api.take();
        let discards: Vec<_> = before_recovery.iter().filter(|call| call.contains("이 입력을 폐기했습니다")).collect();
        assert!(discards.is_empty(), "live pane discarded input: {discards:?}");
        assert!(result.is_err(), "the original is deferred before submit: {result:?}");
        assert_eq!(state["items"], json!([original]));
        assert!(state["head_injection"].is_null());
        assert_eq!(state["retained_inputs"], json!([]));
        let same = intake.mailbox.v2_request(|slot| Ok(slot.as_ref().unwrap().front().unwrap())).await.unwrap();
        assert_eq!((same.0, same.1.queued_generation), (ticket, original.queued_generation));
        assert!(held_watch::has_watch(CHANNEL), "the early hold starts its own reobservation");
        assert_eq!(fx.rig.sends().len(), before);

        // Keep the original inode so the same pinned source can become Idle.
        std::fs::rename(parked, &started.path).unwrap();
        append_terminal(&started, "registration-working");
        fx.rig.answer("pane.read", screen_at(screen));
        assert!(crate::services::claude_tui::readiness::observe(fx.logical(), Some(&started.path)).is_some());
        let state = super::source_input_settled(&intake, &original, &heads).await;
        assert_eq!(state["items"], json!([]));
        super::wait_for_idle_turn_to_unwind(&intake).await;
        let sends = fx.rig.sends();
        assert_eq!(sends.len() - before, 2, "one paste and one Enter");
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        assert_eq!(heads.lock().unwrap()[0]["attempt"]["target"]["execution_nonce"], nonce);
        assert!(intake._api.take().iter().all(|call| !call.contains("이 입력을 폐기했습니다")));
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        drop(rest);
    });
    drop(intake);
    drop(original_runtime);
    drop(guards);
}

#[test]
fn an_old_readiness_due_cannot_discard_a_front_reobserving_a_live_pane_pg() {
    use crate::services::discord::queue_io::input_owner::held_watch;

    let fx = Fixture::new("busy-readiness-to-reobserve", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let original_runtime;
    let mut intake = Intake::new(&fx, &started);
    original_runtime = recording_runtime(&mut intake, &fx, false);
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    let before = fx.rig.sends().len();
    intake.runtime.block_on(async {
        let channel = ChannelId::new(CHANNEL);
        let _rest = test_rest::install(intake._api.http.clone());
        intake.install_holder(&fx, &started).await;
        let front = intake.front().await;
        let original = front.1.clone();
        let parked = started.path.with_extension("readiness-unreadable");
        std::fs::rename(&started.path, &parked).unwrap();
        let result = dispatch::hand_busy(
            (intake.mailbox.clone(), Some(fx.pool.clone())),
            ProviderKind::Claude,
            (CHANNEL, front.clone()),
            Some(intake.shared.clone()),
        )
        .await;
        assert!(
            result.is_err_and(|error| error.contains("busy_source_unavailable")),
            "the active handoff holds for its unreadable source"
        );
        let (started_at, due) = intake
            .mailbox
            .v2_request(|slot| {
                let owner = slot.as_ref().unwrap();
                let hold = owner.front_hold().ok_or("active Held did not save a due")?;
                assert_eq!(owner.settle_due(), Some(hold.due_at_ms));
                Ok((hold.started_at_ms, hold.due_at_ms))
            })
            .await
            .unwrap();
        assert_eq!(due - started_at, 10_000);

        let persistence =
            discord::queue_persistence_context(&intake.shared, &ProviderKind::Claude, channel);
        let finished = intake.mailbox.finish_turn(persistence).await;
        assert!(finished.removed_token.is_some());
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        assert!(inflight::clear_inflight_state(
            &ProviderKind::Claude,
            CHANNEL
        ));
        let next = intake
            .mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().unwrap();
                let front = owner.front().unwrap();
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        assert_eq!(next.0, front.0);
        let result = tokio::time::timeout(
            LIMIT,
            dispatch::start_idle_for_test(intake.shared.clone(), channel, next),
        )
        .await
        .unwrap();
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.contains("retire: Unproven")),
            "the live pane needs reobservation: {result:?}"
        );
        assert!(held_watch::has_watch(CHANNEL));
        assert!((chrono::Utc::now().timestamp_millis() as u64) < due);
        assert_eq!(fx.rig.sends().len(), before);

        let (same, state) = intake
            .mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                owner.tick(due + 1)?;
                Ok((owner.front(), owner.json()?))
            })
            .await
            .unwrap();
        assert_eq!(
            state["items"],
            json!([original]),
            "old due discarded the front: {state}"
        );
        let same = same.unwrap();
        assert_eq!(
            (same.0, same.1.queued_generation),
            (front.0, original.queued_generation)
        );
        assert!(state["refused_fronts"].is_null(), "{state}");
        assert!(
            intake
                ._api
                .take()
                .iter()
                .all(|call| !call.contains("이 입력을 폐기했습니다"))
        );
        assert_eq!(fx.rig.sends().len(), before);

        std::fs::rename(parked, &started.path).unwrap();
        assert!(
            crate::services::claude_tui::readiness::observe(fx.logical(), Some(&started.path))
                .is_some()
        );
        super::source_input_settled(&intake, &original, &heads).await;
        super::wait_for_idle_turn_to_unwind(&intake).await;
        let sends = fx.rig.sends();
        assert_eq!(sends.len() - before, 2);
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        assert_eq!(
            heads.lock().unwrap()[0]["attempt"]["target"]["execution_nonce"],
            nonce
        );
        assert!(
            intake
                ._api
                .take()
                .iter()
                .all(|call| !call.contains("이 입력을 폐기했습니다"))
        );
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
    drop(intake);
    drop(original_runtime);
}

pub(super) fn append_terminal(started: &Started, message: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    append(
        &started.path,
        &[
            json!({"type":"assistant","uuid":uuid(),"apiBlockIndex":1,"sessionId":started.session,
            "timestamp":now,"message":{"id":message,"role":"assistant",
            "content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}),
            json!({"type":"system","subtype":"turn_duration","sessionId":started.session,"timestamp":now}),
        ],
    );
}

#[path = "provider_dispatch_herdr_missing_success_tests.rs"]
mod success;

#[path = "provider_dispatch_herdr_missing_race_tests.rs"]
mod capture_race;
