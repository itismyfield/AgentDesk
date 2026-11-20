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

fn idle_case(missing: bool, restored_after_reconnect: bool) {
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
            1,
            "the actual idle intake makes an early decision after the identity check"
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
        let items = state["items"].as_array().unwrap();
        assert_eq!(items.len(), usize::from(!missing));
        if !missing {
            assert_eq!(items[0]["message_id"], MESSAGE);
            assert_eq!(items[0]["text"], INPUT);
        }
        assert!(
            state["retained_inputs"].as_array().unwrap().is_empty(),
            "refused input was retained"
        );
        assert_eq!(
            discord::queue_io::input_owner::held_watch::has_watch(CHANNEL),
            !missing,
        );
        assert_eq!(
            intake.shared.turn_view_reconciler.ops().iter().any(|op| {
                op.target.message_id.get() == MESSAGE && op.add && op.emoji == '📬'
            }),
            !missing,
            "only an unresolved pane identity keeps this input visibly queued"
        );
        let calls = intake._api.take();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.contains("이 입력을 폐기했습니다"))
                .count(),
            usize::from(missing),
            "only proven terminal failure may discard the original"
        );
        assert_eq!(fx.rig.sends().len(), 2, "no input reached the old pane");
    });
    drop(intake);
    drop(original_runtime);
}

#[test]
fn a_missing_idle_bound_pane_retires_and_a_failed_restart_discards_the_front_pg() {
    idle_case(true, false);
}

// Keep the existing test ID in the PG lane. Its custody assertion now requires the original.
#[test]
fn an_unreadable_idle_bound_pane_is_not_retired_and_its_front_is_discarded_pg() {
    idle_case(false, false);
}

#[test]
fn a_front_restored_after_missing_reconnect_uses_the_same_disposal_rule_pg() {
    idle_case(true, true);
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
