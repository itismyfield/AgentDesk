//! Actual idle starts, including a queue restored after startup retired its missing pane.
use super::*;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::discord::shared_state::test_rest;

fn recording_runtime(intake: &mut Intake, fx: &Fixture, launch: bool) -> tokio::runtime::Runtime {
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
        let after = dispatch::OwnerTurn::probes(CHANNEL);
        assert_eq!(
            after.0 - before.0,
            if endpoint_differs { 0 } else { 1 },
            "a confirmed route refusal ends before ordinary Busy admission"
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
        let keep = !missing && !endpoint_differs;
        let items = state["items"].as_array().unwrap();
        assert_eq!(items.len(), usize::from(keep));
        if keep {
            assert_eq!(state["items"], json!([original]));
            assert_eq!(items[0]["message_id"], MESSAGE);
            assert_eq!(items[0]["text"], INPUT);
        }
        assert!(
            state["retained_inputs"].as_array().unwrap().is_empty(),
            "refused input was retained"
        );
        assert_eq!(
            discord::queue_io::input_owner::held_watch::has_watch(CHANNEL),
            keep,
        );
        assert_eq!(
            intake.shared.turn_view_reconciler.ops().iter().any(|op| {
                op.target.message_id.get() == MESSAGE && op.add && op.emoji == '📬'
            }),
            keep,
            "only an unresolved pane identity keeps this input visibly queued"
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
            usize::from(missing || endpoint_differs),
            "only proven terminal failure may discard the original"
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

// Keep the existing test ID in the PG lane. Its custody assertion now requires the original.
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
        super::idle_turn_unwound(&intake).await;
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
                .is_err_and(|error| error.contains("pane identity or end unproven")),
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
        super::idle_turn_unwound(&intake).await;
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

fn restarted_front(busy: bool, pathless: bool, thread_parent: Option<u64>) {
    use crate::services::discord::inflight;
    use crate::services::tui_prompt_dedupe;

    let fx = Fixture::new(
        if busy {
            "restart-busy-front"
        } else {
            "restart-idle-front"
        },
        None,
    );
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let historical_reply = "reply produced before the pathless turn";
    if pathless {
        // The old response is already behind the fixture's delivered frontier when its writer starts.
        let timestamp = chrono::Utc::now().to_rfc3339();
        append(
            &started.path,
            &[
                json!({"type":"user","uuid":uuid(),"sessionId":started.session,"timestamp":timestamp,
                "message":{"role":"user","content":"prompt preceding the pathless turn"}}),
                json!({"type":"assistant","uuid":uuid(),"sessionId":started.session,
                "timestamp":timestamp,"apiBlockIndex":0,
                "message":{"id":uuid(),"role":"assistant","stop_reason":"end_turn",
                    "content":[{"type":"text","text":historical_reply}]}}),
                json!({"type":"system","subtype":"turn_duration","sessionId":started.session,"timestamp":timestamp}),
            ],
        );
    }
    let mut intake = match thread_parent {
        Some(parent) => Intake::new_thread(&fx, &started, parent),
        None => Intake::new(&fx, &started),
    };
    if let Some(parent) = thread_parent {
        assert!(!crate::config::session_hosts::has_herdr_endpoint(CHANNEL));
        assert!(crate::config::session_hosts::has_herdr_endpoint(parent));
        assert_eq!(
            crate::services::session_host::surface_policy::policy_channel(CHANNEL),
            parent
        );
    }
    let old_runtime = recording_runtime(&mut intake, &fx, true);
    let _rest = test_rest::install(intake._api.http.clone());
    let endpoint =
        crate::services::session_host::HerdrEndpoint::new(NODE, KEY, fx.rig.socket(), SESSION)
            .unwrap();
    let _named = fx.rig.policy_registry_on_this_thread(endpoint, |host| host);
    tui_prompt_dedupe::reset_state_for_tests();
    tui_prompt_dedupe::release_herdr_execution(fx.logical(), &nonce);
    tui_prompt_dedupe::binding_events::forget_channel_for_tests(CHANNEL);
    fx.rig.answer("session.snapshot", json!({"type":"session_snapshot","snapshot":{
        "version":"0.9.3", "protocol":22,
        "workspaces":[{"workspace_id":"w1","label":format!("{}#{nonce}", fx.logical()),"pane_count":1,"tab_count":1}],
        "panes":[{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":7}]
    }}));
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    let screen = if busy {
        screen.replace("Baked", "Thinking")
    } else {
        screen.to_owned()
    };
    fx.rig.answer(
        "pane.read",
        json!({"type":"pane_read","read":{
            "pane_id":PANE,"source":"recent_unwrapped","text":screen,"revision":2,"truncated":false
        }}),
    );
    let before = fx.rig.sends().len();
    let before_requests = fx.rig.requests().len();
    let reply = "answer after Herdr reattach";
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    fx.rig.on_send(move |params| {
        if params["keys"] == json!(["enter"]) {
            let _ = send.send(());
        }
    });
    let mut hooks = fx.ingress.state.subscribe();
    let HostedRecord::Known(mut record) = fx.record() else {
        panic!("expected a Bound execution");
    };
    record.location.as_mut().unwrap().pane_id = "w0:p1".into();
    intake.runtime.block_on(async {
        let channel = ChannelId::new(CHANNEL);
        let old_context = "user: history that must stay out of the first live input";
        discord::turn_bridge::recovery_text::store_session_retry_context_with_audit(
            Some(&fx.pool),
            CHANNEL,
            old_context,
            None,
        )
        .unwrap();
        let row_id: i64 = sqlx::query_scalar(
            "UPDATE sessions SET hosted_execution=$1 WHERE channel_id=$2 RETURNING id",
        )
        .bind(serde_json::to_value(&record).unwrap())
        .bind(CHANNEL.to_string())
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        intake
            .shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .session_id = None;
        let offset = std::fs::metadata(&started.path).unwrap().len();
        if busy {
            let old_prompt = "already running before restart";
            if pathless {
                use crate::services::tui_input::human_frame::history::v2::Registry;
                use std::os::unix::fs::MetadataExt;
                let file = std::fs::metadata(&started.path).unwrap();
                let original = MESSAGE - 1;
                let target = json!({"host":"herdr","owner":fx.logical(),
                    "root":record.location.as_ref().unwrap().socket_addr,"execution_nonce":nonce});
                Registry::runtime().unwrap().prepare(&format!("claude/{TOKEN}"), &json!({
                    "identity":{"primary":original,"queued_generation":1,"sources":[{"message_id":original,"queued_generation":1}]},
                    "attempt":{"nonce":"original-herdr-attempt","provider":"claude","target":target,
                        "source":{"kind":"pre_source","expected_session_id":started.session,
                            "expected_transcript":started.path,"binding_watermark":null}},
                    "resolved_source":{"target":target,"provider":"claude","session_id":started.session,
                        "transcript":started.path,"file":[file.dev(),file.ino()],"offset":offset,"binding_watermark":null},
                    "original":{"author_id":7},"payload":{"frame":old_prompt}
                })).unwrap();
            }
            append(
                &started.path,
                &[
                    json!({"type":"user","uuid":uuid(),"sessionId":started.session,
                "message":{"role":"user","content":old_prompt}}),
                ],
            );
            let mut row = inflight::InflightTurnState::new(
                ProviderKind::Claude,
                CHANNEL,
                None,
                7,
                MESSAGE - 1,
                MESSAGE - 2,
                "already running before restart".into(),
                Some(started.session.clone()),
                Some(fx.logical().to_owned()),
                (!pathless).then(|| started.path.display().to_string()),
                None,
                if pathless { 0 } else { offset },
            );
            row.session_key = discord::adk_session::turn_session_key(
                &intake.shared,
                channel,
                &ProviderKind::Claude,
            )
            .await;
            row.turn_nonce = Some("old-active-episode".into());
            row.turn_start_offset = Some(if pathless { 0 } else { offset });
            row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
            if !pathless {
                row.hosted_record_id = Some(row_id);
                row.hosted_execution_nonce = Some(nonce.clone());
            }
            row.born_generation = intake.shared.restart.current_generation.saturating_sub(1);
            row.restart_generation = Some(row.born_generation);
            inflight::save_inflight_state(&row).unwrap();
            if pathless {
                let stored = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).unwrap();
                assert!(stored.output_path.is_none() && stored.hosted_execution_nonce.is_none());
                assert_eq!(stored.last_offset, 0);
            }
        }
        let front = intake.front().await;
        let start = async {
            if !busy {
                return dispatch::start_idle_for_test(intake.shared.clone(), channel, front).await;
            }
            discord::queue_io::input_owner::drive::wake(source_retry_drive(&intake));
            loop {
                let empty = intake.mailbox.v2_request(|slot| {
                    Ok(slot.as_ref().unwrap().json()?["items"].as_array().unwrap().is_empty())
                }).await.map_err(|error| error.to_string())?;
                if empty {
                    return Ok(());
                }
                tokio::task::yield_now().await;
            }
        };
        let claimant = async {
            if !busy {
                return;
            }
            use crate::services::discord::router::{
                claim_reserved_headless_turn, reserve_headless_turn,
            };
            let reserved = reserve_headless_turn();
            let _completion = intake.shared.session_transition_completion(channel);
            let race = claim_reserved_headless_turn(
                &intake.shared,
                channel,
                UserId::new(7),
                &reserved,
                (ProviderKind::Claude, None),
                (None, Some(&intake.shared)),
            )
            .await;
            assert!(matches!(
                race,
                Err(crate::services::discord::router::HeadlessTurnStartError::Conflict(_))
            ));
            let held = discord::mailbox_snapshot(&intake.shared, channel).await;
            assert_eq!(
                held.active_turn_nonce.as_deref(),
                Some("old-active-episode")
            );
            if pathless {
                let stored = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).unwrap();
                assert_eq!(stored.hosted_execution_nonce.as_deref(), Some(nonce.as_str()));
                assert_eq!(stored.hosted_record_id, Some(row_id));
                assert_eq!(stored.output_path.as_deref(), started.path.to_str());
                assert!(offset > 0);
                assert_eq!(stored.turn_start_offset, Some(offset));
                assert!(stored.last_offset >= offset);
            }
        };
        let provider_reply = async {
            crate::test_support::wait::await_within(receive.recv(), LIMIT)
                .await
                .expect("provider receives Enter");
            let sends = fx.rig.sends();
            let paste = sends[before]["text"].as_str().unwrap();
            let prompt = paste.trim_start_matches("\u{1b}[200~").trim_end_matches("\u{1b}[201~");
            let session = &started.session;
            append(&started.path, &[json!({
                "type":"user", "uuid":uuid(), "sessionId":session, "timestamp":chrono::Utc::now().to_rfc3339(),
                "message":{"role":"user","content":prompt}
            })]);
            let payload = json!({"session_id": session, "prompt": prompt, "transcript_path": started.path});
            assert_eq!(tokio::task::block_in_place(|| started.hook(&fx, "UserPromptSubmit", &payload)), 202);
            let now = chrono::Utc::now().to_rfc3339();
            append(&started.path, &[
                json!({"type":"assistant","uuid":uuid(),"sessionId":session,"timestamp":now,
                    "apiBlockIndex":0,"message":{"id":uuid(),"role":"assistant","stop_reason":"end_turn",
                    "content":[{"type":"text","text":reply}]}}),
                json!({"type":"system","subtype":"turn_duration","sessionId":session,"timestamp":now}),
            ]);
            let payload = json!({"session_id": session, "transcript_path": started.path});
            assert_eq!(tokio::task::block_in_place(|| started.hook(&fx, "Stop", &payload)), 202);
            while let Ok(event) = hooks.try_recv() {
                crate::services::claude_tui::hook_server::publish_hook_event_for_tests(event);
            }
        };
        let (result, (), replay) = tokio::join!(
            tokio::time::timeout(LIMIT, start), claimant, tokio::time::timeout(LIMIT, provider_reply),
        );
        replay.expect("provider replied to the original");
        assert_eq!(result.expect("first input resolves"), Ok(()));
        let sends = fx.rig.sends();
        assert_eq!(sends.len(), before + 2, "{sends:?}");
        assert_eq!(sends[before]["pane_id"], PANE);
        assert_eq!(sends[before + 1]["pane_id"], PANE);
        let payload = sends[before]["text"].as_str().unwrap();
        assert_eq!(payload.matches(INPUT).count(), 1);
        assert!(!payload.contains("already running before restart"));
        assert!(!payload.contains(old_context));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        let busy_marks = || {
            intake.shared.turn_view_reconciler.ops().iter()
                .filter(|op| op.target.message_id.get() == MESSAGE && op.emoji == '➕' && op.add)
                .count()
        };
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        assert!(
            !fx.rig.requests()[before_requests..]
                .iter()
                .any(|request| request["method"] == "workspace.create")
        );
        let saved: Value = sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE id=$1")
            .bind(row_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        assert_eq!(saved["location"]["pane_id"], PANE);
        assert_eq!(saved["state"], "bound");
        assert_eq!(saved["execution_nonce"], nonce);
        let mut calls = Vec::new();
        tokio::time::timeout(LIMIT, async {
            loop {
                calls.extend(intake._api.take());
                let reached = |message: &String| message.contains(reply);
                let delivered = intake.posts.to(CHANNEL).iter().any(reached)
                    || intake.mock.delivered_texts().iter().any(reached)
                    || calls.iter().any(|call| call.contains(&format!("/channels/{CHANNEL}/messages")) && reached(call));
                if delivered && busy_marks() == usize::from(busy) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap_or_else(|error| panic!(
            "same-pane reply and input view did not settle: {error}; busy marks={} O posts={} mock replies={} REST calls={}",
            busy_marks(), intake.posts.to(CHANNEL).len(), intake.mock.delivered_texts().len(), calls.len(),
        ));
        assert_eq!(busy_marks(), usize::from(busy));
        calls.extend(intake._api.take());
        assert!(!calls.iter().any(|call|
            call.contains("이 입력을 폐기했습니다") || call.contains("세션 복구 중") || call.contains("세션 복원")));
        if pathless {
            assert!(!intake.posts.to(CHANNEL).iter().chain(&intake.mock.delivered_texts())
                .chain(&calls).any(|post| post.contains(historical_reply)));
        }
    });
    drop(intake);
    drop(old_runtime);
}

#[test]
fn first_idle_input_after_restart_reuses_named_herdr_and_posts_once_pg() {
    restarted_front(false, false, None);
}

#[test]
fn first_busy_input_after_restart_reuses_named_herdr_while_headless_claim_waits_pg() {
    restarted_front(true, false, None);
}

#[test]
fn first_pathless_busy_claude_input_uses_the_durable_prompt_boundary_pg() {
    restarted_front(true, true, None);
}

#[test]
fn first_named_live_thread_input_inherits_its_verified_parent_endpoint_pg() {
    restarted_front(false, false, Some(CHANNEL + 1));
}

#[test]
fn first_busy_thread_input_inherits_parent_and_keeps_the_child_lease_pg() {
    restarted_front(true, false, Some(CHANNEL + 1));
}

fn native_modal_after_identity_hold(adopt_after_attach: bool) {
    use crate::services::discord::queue_io::input_owner::held_watch;
    use crate::services::discord::recovery_engine::herdr_reattach::{
        VerifiedReattach, continue_owner,
    };
    use crate::services::discord::session_transition::SessionTransitionLease;
    use crate::services::discord::tui_prompt_relay::synthetic_start::claim::claim_tui_direct_synthetic_turn_for_tests;
    use crate::services::tui_prompt_dedupe::{
        self as dedupe, ExternalInputRelayLease, ExternalInputRelayOwner,
    };

    let fx = Fixture::new("native-modal-after-attach", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let mut intake = Intake::new(&fx, &started);
    let old_runtime = recording_runtime(&mut intake, &fx, true);
    let _rest = test_rest::install(intake._api.http.clone());
    let endpoint =
        crate::services::session_host::HerdrEndpoint::new(NODE, KEY, fx.rig.socket(), SESSION)
            .unwrap();
    let _named = fx.rig.policy_registry_on_this_thread(endpoint, |host| host);
    fx.rig.answer("session.snapshot", json!({"type":"session_snapshot","snapshot":{
        "version":"0.9.3","protocol":22,
        "workspaces":[{"workspace_id":"w1","label":format!("{}#{nonce}", fx.logical()),"pane_count":1,"tab_count":1}],
        "panes":[{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":7}]
    }}));
    let channel = ChannelId::new(CHANNEL);
    let mailbox = &intake.mailbox;
    let provider = &ProviderKind::Claude;
    let persistence = || discord::queue_persistence_context(&intake.shared, provider, channel);
    let follower = MESSAGE + 1;
    let native = MESSAGE + 2;
    dedupe::release_herdr_execution(fx.logical(), &nonce);
    fx.rig
        .foreground(&[crate::services::session_host::herdr_socket_rig_tests::SHELL]);

    let (original, old_due, key) = intake.runtime.block_on(async {
        for message in [MESSAGE, follower] {
            let item: PendingQueueItem = serde_json::from_value(json!({
                "author_id":7,"channel_id":CHANNEL,"message_id":message,
                "source_message_ids":[message],"queued_generation":1,"text":INPUT,
                "merge_consecutive":true,
            }))
            .unwrap();
            assert!(matches!(
                admit(mailbox, &item, AdmissionTicket::new()).await,
                OwnerAdmission::Accepted(_)
            ));
        }
        let original = mailbox
            .v2_request(|slot| Ok(slot.as_ref().unwrap().front().unwrap()))
            .await
            .unwrap();
        let front = original.clone();
        let old_due = mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
                let held = owner.hold_readiness(&front, "busy_readiness_unproven", now)?;
                assert_eq!(held.members.unwrap().len(), 2);
                slot.take();
                Ok(held.due_at_ms)
            })
            .await
            .unwrap();
        mailbox
            .v2_ensure_channel(&persistence(), channel)
            .await
            .unwrap();
        let front = mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().unwrap();
                let front = owner.front().unwrap();
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        assert_eq!(front.0, original.0);
        intake
            .shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .session_id = None;
        let error = dispatch::start_idle_for_test(intake.shared.clone(), channel, front)
            .await
            .expect_err("unconfirmed identity holds A");
        assert!(error.contains("herdr_restart_held"), "{error}");
        assert_eq!(mailbox.has_active_turn().await, Ok(false));
        assert!(held_watch::has_watch(CHANNEL));
        let held = mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert!(held["readiness_holds"][0]["identity_held_at_ms"].is_u64());
        let key = discord::adk_session::turn_session_key(&intake.shared, channel, provider)
            .await
            .unwrap();
        if adopt_after_attach {
            mailbox
                .v2_request(|slot| {
                    slot.take();
                    Ok(())
                })
                .await
                .unwrap();
        }
        (original, old_due, key)
    });

    fx.rig
        .foreground(&[crate::services::session_host::herdr_socket_rig_tests::PROVIDER]);
    intake.runtime.block_on(async {
        use crate::db::dispatched_sessions::hosted_execution::{
            HostedLookup, HostedLookupKey, load_hosted_execution_pg,
        };
        assert!(inflight::load_inflight_state_read_only(provider, CHANNEL).is_none());
        let HostedLookup::Found(row) =
            load_hosted_execution_pg(&fx.pool, HostedLookupKey::SessionKey(&key)).await
        else {
            panic!("the separate recovery has a canonical hosted row")
        };
        let transition = SessionTransitionLease::try_acquire(&intake.shared, channel).unwrap();
        assert_eq!(
            continue_owner(&intake.shared, &fx.pool, &transition, &row)
                .await
                .0,
            VerifiedReattach::Live,
        );
    });
    assert_eq!(
        intake
            .shared
            .tmux_watchers
            .owner_channel_for_tmux_session(fx.logical()),
        Some(channel),
    );

    intake.runtime.block_on(async {
        if adopt_after_attach {
            mailbox
                .v2_ensure_channel(&persistence(), channel)
                .await
                .unwrap();
        }
        let recovered = mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        let body = "native B after separate reattachment";
        append(
            &started.path,
            &[json!({"type":"user","sessionId":started.session,
            "uuid":uuid(),"timestamp":chrono::Utc::now().to_rfc3339(),
            "message":{"role":"user","content":body}})],
        );
        let mut lease = ExternalInputRelayLease::unassigned(Some(CHANNEL));
        lease.turn_id = Some("native-b-after-restart".into());
        lease.session_key = Some(key.clone());
        lease.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        lease.relay_owner = ExternalInputRelayOwner::TmuxWatcher;
        lease.hosted_source = Some(
            dedupe::hosted_source::capture(
                "claude",
                fx.logical(),
                CHANNEL,
                &started.path,
                Some(&started.session),
            )
            .unwrap(),
        );
        let lease = dedupe::record_external_input_turn_lease("claude", fx.logical(), lease);
        assert!(
            claim_tui_direct_synthetic_turn_for_tests(
                &intake.shared,
                provider,
                channel,
                fx.logical(),
                body,
                MessageId::new(native),
                &lease,
            )
            .await
        );
        assert_eq!(mailbox.has_active_turn().await, Ok(true));
        let active = inflight::load_inflight_state_read_only(provider, CHANNEL).unwrap();
        assert_eq!(active.user_msg_id, native);
        assert!(inflight::row_is_current_generation(
            &active,
            intake.shared.restart.current_generation
        ));
        assert!(matches!(
            crate::services::turn_host::for_turn(Some(&fx.pool), provider, CHANNEL, Some(&key))
                .await,
            TurnHost::Herdr(_)
        ));
        assert!(crate::services::tui_o::writer::host::channel_accepts(
            CHANNEL
        ));
        let modal = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel";
        assert!(crate::services::claude_tui::busy_inject::modal(modal));
        fx.rig.answer("pane.read", json!({"type":"pane_read","read":{
            "pane_id":PANE,"source":"recent_unwrapped","text":modal,"revision":7,"truncated":false
        }}));
        let previous = dispatch::OwnerTurn::drive_results(CHANNEL).len();
        let (_, observed) = held_watch::watch_observed(source_retry_drive(&intake), MESSAGE);
        let observed = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.verdict, held_watch::Verdict::Wake);
        let result = tokio::time::timeout(LIMIT, async {
            loop {
                if let Some(result) = dispatch::OwnerTurn::drive_results(CHANNEL).get(previous) {
                    break result.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(result.contains("busy_source_unavailable"), "{result}");
        let (due, snapshot) = mailbox
            .v2_request(|slot| {
                let owner = slot.as_ref().unwrap();
                Ok((owner.settle_due(), owner.json()?))
            })
            .await
            .unwrap();
        let due = due.expect("the new composer refusal has an active due");
        if adopt_after_attach {
            assert!(recovered["readiness_holds"][0]["identity_held_at_ms"].is_u64());
        } else {
            assert!(
                recovered["readiness_holds"].is_null(),
                "background marker: {}",
                recovered["readiness_holds"]
            );
        }
        assert!(due > old_due);
        assert!(snapshot["readiness_holds"][0]["identity_held_at_ms"].is_null());
        assert!(snapshot["head_injection"].is_null());
        mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                owner.tick(old_due + 1)?;
                let front = owner.front().unwrap();
                assert_eq!(front.0, original.0);
                assert_eq!(
                    serde_json::to_value(front.1).unwrap(),
                    serde_json::to_value(original.1).unwrap()
                );
                let kept = owner.json()?;
                assert_eq!(kept["items"][1]["message_id"], follower);
                owner.tick(due)?;
                owner.tick(due + 1)?;
                let settled = owner.json()?;
                assert_eq!(settled["items"], json!([]));
                let refusals = settled["refused_fronts"].as_array().unwrap();
                assert_eq!(refusals.len(), 1);
                assert_eq!(refusals[0]["members"].as_array().unwrap().len(), 2);
                Ok(())
            })
            .await
            .unwrap();
    });
    drop(intake);
    drop(old_runtime);
}

#[test]
fn native_busy_after_background_reattach_gets_a_new_composer_due_pg() {
    native_modal_after_identity_hold(false);
}

#[test]
fn native_busy_after_queue_adoption_gets_a_new_composer_due_pg() {
    native_modal_after_identity_hold(true);
}
