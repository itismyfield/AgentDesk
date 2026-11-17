//! A first input whose idle source proves no boundary by the end of its readiness budget ends at
//! the owner retained with a resend ask; one whose turn still runs waits at the front. A launch
//! older than the daemon restart proves its idle composer on its last hard terminal.
use super::tests::{NONCE, bound_target};
use super::*;
use crate::services::claude_tui::{hook_server::HookEventKind, readiness};
use crate::services::discord::{
    owner_effect_for_test as effect, owner_held_watch_for_test as held_watch,
};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding, binding_context};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    /// Resumed and idle, then the daemon restarts and attaches the same transcript again; the
    /// launch is no older than this run, so only its own Start could admit it.
    Restarted,
    /// The same, launched before the restart: its last hard terminal admits the input.
    RestartedOld,
    /// A fresh launch whose SessionStart the ledger refused.
    StartRefused,
    /// Restarted like the first, over a turn the transcript shows still running.
    Running,
    /// Restarted like the first, keeping the start record its accepted resume left.
    Recorded,
    /// A continuing turn's summary arrives after the restart lost its hook's volatile latch.
    Continuing,
    /// Restarted like the first; one metadata row lands while the input waits.
    Moved,
    /// The same wait, but what lands is a turn the idle-looking screen does not show.
    MovedRunning,
    /// The same wait; a continuation fence closes over the metadata row.
    MovedFenced,
    /// The same wait and metadata row under a screen that shows a turn.
    MovedBusy,
}
use Case::{Moved, MovedBusy, MovedFenced, MovedRunning};

// The resumed cases restart without the start record a resume accepted before its build lacks.
#[test]
fn an_idle_source_that_proves_nothing_ends_retained_and_a_running_one_waits() {
    owner_outcome(&[
        (Case::Restarted, 816_101u64),
        (Case::RestartedOld, 816_104),
        (Case::StartRefused, 816_102),
        (Case::Running, 816_103),
    ]);
}

#[test]
fn a_restarted_resume_with_its_recorded_start_takes_its_input_once() {
    owner_outcome(&[(Case::Recorded, 816_105)]);
}

#[test]
fn a_restart_during_a_continuing_stop_retains_with_one_resend_notice_pg() {
    owner_outcome(&[(Case::Continuing, 816_106)]);
}

#[test]
fn a_wait_whose_transcript_moved_once_ends_retained_with_one_resend_notice_pg() {
    owner_outcome(&[(Moved, 821_711)]);
}

#[test]
fn a_moved_transcript_under_a_turn_a_fence_or_a_busy_screen_keeps_the_front_waiting() {
    owner_outcome(&[
        (MovedRunning, 821_712),
        (MovedFenced, 821_713),
        (MovedBusy, 821_714),
    ]);
}

fn owner_outcome(cases: &[(Case, u64)]) {
    for &(case, channel) in cases {
        let env_guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        // Keep shared binding resets out of this readiness observation.
        let dedupe_guard = tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        readiness::initialize();
        let rig = HerdrRig::start();
        let _registry = rig.registry_on_this_thread();
        let logical = format!("i8161-unproven-{}", uuid::Uuid::new_v4().simple());
        let native = uuid::Uuid::new_v4().to_string();
        let projects = root.path().join("projects");
        let path = projects.join("workspace").join(format!("{native}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // The earlier run's history predates every launch here.
        let old = chrono::Utc::now() - chrono::Duration::hours(8);
        let rows = |rows: &[Value]| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            for row in rows {
                let mut row = row.clone();
                row["sessionId"] = json!(native);
                writeln!(file, "{row}").unwrap();
            }
        };
        let asked = json!({"type":"user","timestamp":old,"message":{"role":"user","content":"earlier prompt"}});
        let said = json!({"type":"assistant","timestamp":old,"message":{"role":"assistant","content":[{"type":"text","text":"earlier answer"}]}});
        match case {
            Case::Running => rows(&[asked.clone(), said.clone()]),
            Case::StartRefused => (),
            _ => rows(&[
                asked.clone(),
                said.clone(),
                json!({"type":"system","subtype":"turn_duration","timestamp":old}),
                json!({"type":"last-prompt"}),
            ]),
        }
        let moves = matches!(case, Moved | MovedRunning | MovedFenced | MovedBusy);
        let resumed = case != Case::StartRefused;
        let context: binding_context::BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","execution_nonce":NONCE,
            "created_at":if case == Case::RestartedOld { old + chrono::Duration::hours(1) } else { chrono::Utc::now() },
            "tmux_session":logical,"channel_id":1,"owner_runtime_root":"test","host":null,
            "expected_native_session_id":native,
            "launch_mode":if resumed { "resume" } else { "fresh" },"provider_root":projects
        }))
        .unwrap();
        let launch = binding_context::PreparedIncarnation::create(context.clone()).unwrap();
        rig.run_provider(&launch.path, false);
        for (marker, value) in [("host_kind", "herdr"), ("spawn_nonce", NONCE)] {
            let marker = crate::services::tmux_common::session_temp_path(&logical, marker);
            std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, value).unwrap();
        }
        let _admission = crate::services::herdr_admission::force_for_test(
            crate::services::herdr_admission::Admission::new(
                true,
                Some(root.path().join("no-stop")),
            ),
        );
        // A start published before its launch was created is refused, as one lost would be.
        let published = match resumed {
            true => chrono::Utc::now(),
            false => old,
        };
        readiness::note_hook(
            "claude",
            &HookEventKind::SessionStart,
            &logical,
            &json!({"session_id":native,"transcript_path":path,
                "source":if resumed { "resume" } else { "startup" }}),
            Some(&context),
            Some(published),
            false,
        );
        let attach = || {
            tui_prompt_dedupe::register_tmux_runtime_binding(
                &logical,
                TuiRuntimeBinding {
                    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                    output_path: path.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some(native.clone()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            )
        };
        attach();
        if resumed {
            assert!(
                readiness::observe(&logical, Some(&path)).is_some() || case == Case::Running,
                "{case:?}: the resumed start admits its first input before the restart"
            );
            if case == Case::Continuing {
                rows(&[
                    json!({"type":"user","timestamp":chrono::Utc::now(),"message":{"role":"user","content":"current prompt"}}),
                    json!({"type":"assistant","timestamp":chrono::Utc::now(),"message":{"role":"assistant","content":[{"type":"text","text":"still continuing"}]}}),
                ]);
                assert!(readiness::observe(&logical, Some(&path)).is_none());
            }
            tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
            if !matches!(case, Case::Recorded | Case::Continuing) {
                readiness::tests::lose_resume_record(NONCE);
            }
            readiness::tests::daemon_restart(&logical);
            attach();
            if case == Case::Continuing {
                // This ledger belongs to the restarted daemon; the earlier Stop's latch is gone.
                rows(&[json!({"type":"system","subtype":"stop_hook_summary",
                    "hookCount":1,"hookErrors":[],"hookAdditionalContext":["continue"],
                    "preventedContinuation":false,"hasOutput":true})]);
                assert_eq!(
                    crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(
                        &path
                    ),
                    crate::services::tui_turn_state::TuiTurnState::Idle
                );
                assert!(readiness::observe_restored(&logical, Some(&path)).is_none());
            }
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .on_thread_start(rig.thread_installer(NODE, KEY, rig.socket(), SESSION))
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
        let socket = rig.socket().display().to_string();
        let attempt = json!({"nonce":format!("unproven-{channel}"),"provider":"claude",
            "target":{"host":"herdr","owner":logical,"root":socket,"execution_nonce":NONCE},
            "source":{"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}});
        runtime
            .block_on(mailbox.v2_cold_fixture(
                root.path().join("unproven.json"),
                attempt,
                "one original".into(),
                channel,
            ))
            .unwrap();
        let border = "─".repeat(52);
        rig.answer("pane.read", json!({"type":"pane_read","read":{"pane_id":PANE,
            "source":"recent_unwrapped","text":format!("✻ Baked for 2s\n\n{border}\n❯ \n{border}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"),
            "revision":1,"truncated":false}}));
        if case == Case::Continuing {
            rig.answer(
                "pane.read",
                json!({"type":"pane_read","read":{"pane_id":PANE,
                "source":"recent_unwrapped","text":"✻ Working… (esc to interrupt)",
                "revision":2,"truncated":false}}),
            );
        }
        if case == MovedBusy {
            rig.answer("pane.read", json!({"type":"pane_read","read":{"pane_id":PANE,
                "source":"recent_unwrapped","text":format!("✻ Thinking… (3s · esc to interrupt)\n\n{border}\n❯ \n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"),
                "revision":2,"truncated":false}}));
        }
        // What lands inside the wait, after it kept its Stamp.
        let moved = moves.then(|| {
            let (path, native, logical) = (path.clone(), native.clone(), logical.clone());
            let (context, asked, said) = (context.clone(), asked.clone(), said.clone());
            readiness::tests::after_first_look(&logical.clone(), move || {
                use std::io::Write;
                if case == MovedFenced {
                    readiness::note_hook(
                        "claude",
                        &HookEventKind::Stop,
                        &logical,
                        &json!({"session_id":native,"transcript_path":path}),
                        Some(&context),
                        Some(chrono::Utc::now()),
                        true,
                    );
                }
                let landed = match case {
                    MovedRunning => vec![asked, said],
                    _ => vec![json!({"type":"ai-title"})],
                };
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                for mut row in landed {
                    row["sessionId"] = json!(native);
                    writeln!(file, "{row}").unwrap();
                }
            })
        });
        let (_, target) = bound_target(&rig, &logical, 1);
        let attached = Attached {
            nonce: NONCE.into(),
            session_id: native.clone(),
            transcript: path.clone(),
            target,
            bound: true,
            awaiting_clear: false,
            physical: None,
        };
        let session = logical.clone();
        let leaf = effect::created(
            Box::new(move |frame: String| {
                // The readiness budget runs out on the leaf's own thread.
                let budget = Some(Duration::from_millis(if moves { 900 } else { 450 }));
                composer::READY_WAIT_FOR_TESTS.with(|wait| wait.set(budget));
                let result = guarded_outcome::capture_preparation(|| {
                    let (sender, receiver) = std::sync::mpsc::channel();
                    let result = write(&session, None, attached, &frame, &sender, None).map(drop);
                    drop(receiver);
                    result
                });
                composer::READY_WAIT_FOR_TESTS.with(|wait| wait.set(None));
                result
            }),
            Some(format!("{socket}#{PANE}")),
        );
        let result = runtime
            .block_on(effect::run(&mailbox, Arc::new(|| 10_000), leaf))
            .unwrap();
        if let Some(moved) = moved {
            crate::test_support::wait::join_within(moved, crate::test_support::wait::WAIT_CAP);
        }
        let read = || {
            runtime
                .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
                .unwrap()
        };
        if matches!(case, Case::Recorded | Case::RestartedOld) {
            assert_eq!(result, effect::Completion::Submitted, "{case:?}");
            let sends = rig.sends();
            let pasted = sends
                .iter()
                .filter(|s| s["text"].as_str() == Some("\u{1b}[200~one original\u{1b}[201~"));
            assert_eq!((sends.len(), pasted.count()), (2, 1), "{sends:?}");
            let snapshot = read();
            assert!(snapshot["items"].as_array().unwrap().is_empty());
            assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
            drop(entered);
            tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
            continue;
        }
        assert!(
            matches!(result, effect::Completion::NotSent(_)),
            "{case:?}: {result:?}"
        );
        assert!(rig.sends().is_empty(), "{case:?}: no key reaches the pane");
        if case == Case::Continuing {
            assert!(
                matches!(&result, effect::Completion::NotSent(reason) if reason.contains("unproven"))
            );
        }
        let snapshot = read();
        assert!(snapshot["head_injection"].is_null(), "{case:?}: {snapshot}");
        let (items, retained) = (
            snapshot["items"].as_array().unwrap(),
            snapshot["retained_inputs"].as_array().unwrap(),
        );
        if matches!(case, Case::Running | MovedRunning | MovedFenced | MovedBusy) {
            assert_eq!(items.len(), 1, "{case:?}: the original stays in front");
            assert!(retained.is_empty(), "{case:?}: nothing is retained");
            assert!(held_watch::armed(channel), "{case:?}: its watch is left");
        } else {
            assert!(items.is_empty(), "{case:?}: the front is released");
            assert_eq!(retained.len(), 1, "{case:?}: {snapshot}");
            let entry = &retained[0];
            assert_eq!(entry["input"]["original"]["text"], "one original");
            assert_eq!(entry["disposition"], "known_not_sent", "{case:?}");
            assert_eq!(
                entry["notice"],
                json!({"kind":"retained_retry_required","enqueued":false}),
                "{case:?}: one resend ask is owed"
            );
            assert!(!held_watch::armed(channel), "{case:?}: no look resends it");
            let notice_db = matches!(case, Case::Continuing | Moved).then(|| {
                runtime.block_on(async {
                    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
                    let pool = db.connect_and_migrate().await;
                    let namespace = snapshot["namespace"].as_str().unwrap();
                    let enqueue = crate::services::turn_orchestrator::ChannelMailboxHandle::v2_notice;
                    let first = enqueue(&pool, "claude", channel, namespace, entry).await.unwrap();
                    let repeated = enqueue(&pool, "claude", channel, namespace, entry).await.unwrap();
                    assert_eq!(first, repeated, "replaying an owed notice keeps its identity");
                    let posts: Vec<(String, String)> = sqlx::query_as(
                        "SELECT source, content FROM message_outbox WHERE target = $1 ORDER BY id",
                    )
                    .bind(format!("channel:{channel}"))
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                    assert_eq!(posts.len(), 1, "one retained original owes one actual notice: {posts:?}");
                    assert_eq!(posts[0].0, "retained_input_notice");
                    assert!(posts[0].1.contains("필요하면 같은 내용을 다시 보내 주세요"));
                    assert!(crate::services::turn_orchestrator::ChannelMailboxHandle::v2_notice_is_non_turn(&posts[0].1));
                    (db, pool)
                })
            });
            let key =
                json!({"identity":entry["input"]["identity"],"attempt":entry["input"]["attempt"]});
            let moved = runtime.block_on(mailbox.v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
                owner.notice_enqueued(&key)?;
                owner.requeue_known_not_sent().map(|moved| moved.len())
            }));
            assert_eq!(moved, Ok(0), "{case:?}: a told input is never resubmitted");
            let told = read();
            assert!(told["items"].as_array().unwrap().is_empty(), "{case:?}");
            assert_eq!(told["retained_inputs"].as_array().unwrap().len(), 1);
            if let Some((db, pool)) = notice_db {
                assert_eq!(told["retained_inputs"][0]["notice"]["enqueued"], true);
                runtime.block_on(async {
                    pool.close().await;
                    db.drop().await;
                });
            }
            assert!(rig.sends().is_empty(), "{case:?}: still no key");
        }
        drop(entered);
        tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
        drop(runtime);
        drop(root_guard);
        drop(root);
        drop(dedupe_guard);
        drop(env_guard);
    }
}

// A launch older than the restart, held by a person's draft, is woken by the
// restart-aware watch once the draft clears, and submits the original exactly once.
#[test]
fn a_restored_front_held_by_a_draft_wakes_once_the_draft_clears() {
    use crate::services::discord::owner_drive_for_test as drive;
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    readiness::initialize();
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let logical = format!("i8161-draft-{}", uuid::Uuid::new_v4().simple());
    let channel = 816_110u64;
    let native = uuid::Uuid::new_v4().to_string();
    let projects = root.path().join("projects");
    let path = projects.join("workspace").join(format!("{native}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let old = chrono::Utc::now() - chrono::Duration::hours(6);
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&path).unwrap();
        for row in [
            json!({"type":"user","timestamp":old,"message":{"role":"user","content":"earlier prompt"}}),
            json!({"type":"assistant","timestamp":old,"message":{"role":"assistant","content":[{"type":"text","text":"earlier answer"}]}}),
            json!({"type":"system","subtype":"turn_duration","timestamp":old}),
        ] {
            let mut row = row;
            row["sessionId"] = json!(native);
            writeln!(file, "{row}").unwrap();
        }
    }
    let context: binding_context::BindingContext = serde_json::from_value(json!({
        "schema":1,"provider":"claude","created_at":old,"execution_nonce":NONCE,
        "tmux_session":logical,"channel_id":channel,"owner_runtime_root":"test","host":null,
        "expected_native_session_id":native,"launch_mode":"resume","provider_root":projects
    }))
    .unwrap();
    let launch = binding_context::PreparedIncarnation::create(context).unwrap();
    rig.run_provider(&launch.path, false);
    for (marker, value) in [("host_kind", "herdr"), ("spawn_nonce", NONCE)] {
        let marker = crate::services::tmux_common::session_temp_path(&logical, marker);
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, value).unwrap();
    }
    let _admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(true, Some(root.path().join("no-stop"))),
    );
    tui_prompt_dedupe::register_tmux_runtime_binding(
        &logical,
        TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(native.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    readiness::tests::daemon_restart(&logical);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_start(rig.thread_installer(NODE, KEY, rig.socket(), SESSION))
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let socket = rig.socket().display().to_string();
    let attempt = json!({"nonce":"restored-draft-1","provider":"claude",
        "target":{"host":"herdr","owner":logical,"root":socket,"execution_nonce":NONCE},
        "source":{"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null}});
    runtime
        .block_on(mailbox.v2_cold_fixture(
            root.path().join("restored-draft.json"),
            attempt.clone(),
            "one original".into(),
            channel,
        ))
        .unwrap();
    let border = "─".repeat(52);
    let screen = |draft: &str| {
        json!({"type":"pane_read","read":{"pane_id":PANE,
        "source":"recent_unwrapped","text":format!("✻ Baked for 2s\n\n{border}\n❯ {draft}\n{border}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"),
        "revision":1,"truncated":false}})
    };
    rig.answer("pane.read", screen("a person's draft"));
    let attached = |target: HerdrTarget| Attached {
        nonce: NONCE.into(),
        session_id: native.clone(),
        transcript: path.clone(),
        target,
        bound: true,
        awaiting_clear: false,
        physical: None,
    };
    let (_, target) = bound_target(&rig, &logical, 1);
    let first = attached(target.clone());
    let writer = format!("{socket}#{PANE}");
    let session = logical.clone();
    let leaf = effect::created(
        Box::new(move |frame: String| {
            guarded_outcome::capture_preparation(|| {
                let (sender, _receiver) = std::sync::mpsc::channel();
                write(&session, None, first, &frame, &sender, None).map(drop)
            })
        }),
        Some(writer.clone()),
    );
    let held = runtime
        .block_on(effect::run(&mailbox, Arc::new(|| 10_000), leaf))
        .unwrap();
    assert!(matches!(held, effect::Completion::NotSent(_)), "{held:?}");
    assert!(rig.sends().is_empty(), "the draft takes no key");
    let snapshot = runtime
        .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
        .unwrap();
    assert_eq!(
        snapshot["items"].as_array().unwrap().len(),
        1,
        "the front stays"
    );
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    let message = snapshot["items"][0]["message_id"].as_u64().unwrap();
    rig.answer("pane.read", screen(""));
    let (completed, completion) = tokio::sync::oneshot::channel();
    let completed = Arc::new(std::sync::Mutex::new(Some(completed)));
    let second = Arc::new(std::sync::Mutex::new(Some(attached(target))));
    let (m, a, session) = (mailbox.clone(), attempt, logical.clone());
    let drive = Arc::new(drive::Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: Arc::new(move |front| {
            assert_eq!(front.1.text, "one original");
            let (m, mut a, attached, done, writer, session) = (
                m.clone(),
                a.clone(),
                second.lock().unwrap().take().unwrap(),
                completed.clone(),
                writer.clone(),
                session.clone(),
            );
            Box::pin(async move {
                a["nonce"] = "restored-draft-2".into();
                m.v2_request(move |slot| {
                    slot.as_mut()
                        .unwrap()
                        .reserve(
                            serde_json::from_value(a).unwrap(),
                            serde_json::from_value(
                                json!({"frame":"one original","digest":"fixture","attribution":{}}),
                            )
                            .unwrap(),
                        )
                        .map(drop)
                })
                .await?;
                let leaf = effect::created(
                    Box::new(move |frame| {
                        guarded_outcome::capture_preparation(|| {
                            let (sender, _receiver) = std::sync::mpsc::channel();
                            write(&session, None, attached, &frame, &sender, None).map(drop)
                        })
                    }),
                    Some(writer),
                );
                let result = effect::run(&m, Arc::new(|| 10_001), leaf).await?;
                done.lock().unwrap().take().unwrap().send(result).unwrap();
                Ok(())
            })
        }),
        settled: None,
    });
    let observed = runtime.block_on(async {
        let (_, read) = held_watch::watch_observed(drive, message);
        tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(
        observed.verdict,
        held_watch::Verdict::Wake,
        "the restart-aware watch wakes the front once the draft clears"
    );
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), completion)
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(result, effect::Completion::Submitted);
    assert_eq!(rig.sends().len(), 2, "one paste and one Enter");
    tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
}
