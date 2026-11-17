//! The repaired front continues through production create, attach and the owner's first write.
use super::*;

#[test]
fn a_missing_idle_front_launches_a_fresh_execution_and_delivers_once_pg() {
    let fx = Fixture::new("missing-fresh-delivery", None);
    let launcher = Arc::new(Launcher::default());
    let (old_nonce, previous) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let original_runtime;
    let mut intake = Intake::new(&fx, &previous);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    original_runtime = recording_runtime(&mut intake, &fx, true);
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:1".into(),
    );
    fx.rig.answer(
        "ping",
        json!({"type":"pong","version":"0.9.3","protocol":22}),
    );
    fx.rig.answer("workspace.create",json!({"type":"workspace_created","root_pane":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":0,"cwd":fx.cwd.path()}}));
    fx.rig.show_panes(&[]);
    fx.rig.answer("pane.process_info", json!({"type":"ok"}));
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
    // The initial answer was delivered before O adopted this fixture; its resume needs that cursor.
    {
        use crate::services::tui_o::store::spool::{Cursor, source_key};
        use sha2::{Digest, Sha256};
        let source = source_id_for(&previous.session, &previous.path).unwrap();
        let bytes = std::fs::read(&previous.path).unwrap();
        let cursor = Cursor {
            source: source.clone(),
            captured_through: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
            retired: false,
            transition_cleanup: None,
        };
        let directory = crate::config::runtime_root()
            .unwrap()
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cursor");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{}.json", source_key(&source))),
            serde_json::to_vec(&cursor).unwrap(),
        )
        .unwrap();
        assert_eq!(
            crate::services::claude::session_handoff::o_reads(CHANNEL, &source),
            Ok(true)
        );
    }
    let before = fx.rig.sends().len();
    let (command_sent, command_seen) = std::sync::mpsc::channel();
    let (continue_create, create_ready) = std::sync::mpsc::channel();
    let mut captured = false;
    fx.rig.on_send(move |_| {
        if !captured {
            captured = true;
            command_sent.send(()).unwrap();
            crate::test_support::wait::recv_within(&create_ready, LIMIT);
        }
    });
    let mut events = fx.ingress.state.subscribe();
    let handle = intake.runtime.handle().clone();
    let shared = intake.shared.clone();
    let install = fx.rig.thread_installer_with_launch(NODE, KEY, SESSION);
    let config = crate::config_live_reload::current().unwrap();
    let boot = cutover::boot_snapshot().expect("the managed writer's adopted boot snapshot");
    let root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let http = intake._api.http.clone();
    {
        let caller = std::thread::spawn(move || {
            install();
            crate::services::tui_prompt_dedupe::binding_events::set_test_root(root.as_deref());
            let _boot = test_override::force_boot(boot);
            let _config = crate::config::session_hosts::force_config_for_test(&config);
            let _admission = open_admission();
            let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
            handle.block_on(async {
                let (entered, resume) =
                    discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                        .await
                        .pause();
                let start = async {
                    let result =
                        dispatch::start_idle_for_test(shared, ChannelId::new(CHANNEL), front).await;
                    eprintln!("MISSING_FRESH_START_RESULT {result:?}");
                    result
                };
                let recording = async {
                    tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
                    let guard = test_rest::install(http);
                    resume.send(()).unwrap();
                    guard
                };
                let (result, _rest) = tokio::join!(tokio::time::timeout(LIMIT, start), recording);
                result.unwrap()
            })
        });
        let pending = fx
            .rt
            .block_on(async {
                tokio::time::timeout(LIMIT, async {
                    loop {
                        let raw: Value = sqlx::query_scalar(
                            "SELECT hosted_execution FROM sessions WHERE channel_id=$1",
                        )
                        .bind(CHANNEL.to_string())
                        .fetch_one(&fx.pool)
                        .await
                        .unwrap();
                        if let HostedRecord::Known(record) = HostedRecord::decode(Some(&raw))
                            && record.state == HostedState::Pending
                            && record.execution_nonce != old_nonce
                        {
                            break record;
                        }
                        assert!(
                            !caller.is_finished(),
                            "fresh launch ended before Pending placement"
                        );
                        tokio::task::yield_now().await;
                    }
                })
                .await
            })
            .expect("the repaired input reaches production create");
        let context = crate::services::tui_prompt_dedupe::binding_context::execution_context(
            "claude",
            &pending.execution_nonce,
        )
        .unwrap();
        let session = context.expected_native_session_id.unwrap();
        let context_path = context_of(&pending.execution_nonce);
        crate::test_support::wait::recv_within(&command_seen, LIMIT);
        // Publish the created pane before create resumes into its first evidence read.
        fx.rig.run_provider(&context_path, false);
        fx.rig.foreground(&[20]);
        fx.rig.show_panes(&[PANE]);
        continue_create.send(()).unwrap();
        fx.rt.block_on(async {
            tokio::time::timeout(LIMIT, async {
                loop {
                    let raw: Value = sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                        .bind(CHANNEL.to_string()).fetch_one(&fx.pool).await.unwrap();
                    if matches!(HostedRecord::decode(Some(&raw)), HostedRecord::Known(record)
                        if record.execution_nonce == pending.execution_nonce && record.location.is_some()) {
                        break;
                    }
                    assert!(!caller.is_finished(), "create ended before its location was recorded");
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
        });
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            fx.cwd.path(),
            &session,
            fx.home().as_deref(),
        )
        .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        if !path.exists() {
            std::fs::write(&path, b"").unwrap();
        }
        let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
            (name == "AGENTDESK_BINDING_CONTEXT").then(|| context_path.clone().into_os_string())
        })
        .encode()
        .unwrap();
        let started = Started {
            session,
            path,
            envelope,
        };
        let source = if started.session == previous.session {
            "resume"
        } else {
            "startup"
        };
        assert_eq!(started.hook(&fx,"SessionStart",&json!({"session_id":started.session,"transcript_path":started.path,"source":source})),202);
        while let Ok(event) = events.try_recv() {
            crate::services::claude_tui::hook_server::publish_hook_event_for_tests(event);
        }
        fx.rt
            .block_on(async {
                tokio::time::timeout(LIMIT, async {
                    while fx.rig.sends().len() < before + 3 {
                        assert!(
                            !caller.is_finished(),
                            "input ended before the new pane received it"
                        );
                        tokio::task::yield_now().await;
                    }
                })
                .await
            })
            .expect("new pane takes the prompt");
        fx.finished.store(false, Ordering::SeqCst);
        fx.answer_after(&started, before + 1);
        while let Ok(event) = events.try_recv() {
            crate::services::claude_tui::hook_server::publish_hook_event_for_tests(event);
        }
        assert_eq!(
            crate::test_support::wait::join_within(caller, LIMIT),
            Ok(())
        );
        assert!(
            matches!(fx.record(),HostedRecord::Known(record) if record.state==HostedState::Bound && record.execution_nonce==pending.execution_nonce)
        );
    }
    let state = intake
        .runtime
        .block_on(
            intake
                .mailbox
                .v2_request(|slot| slot.as_ref().unwrap().json()),
        )
        .unwrap();
    assert!(state["items"].as_array().unwrap().is_empty());
    assert!(state["retained_inputs"].as_array().unwrap().is_empty());
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), before + 3);
    assert!(sends[before + 1]["text"].as_str().unwrap().contains(INPUT));
    assert_eq!(sends[before + 2]["keys"], json!(["enter"]));
    assert!(
        !intake
            ._api
            .take()
            .iter()
            .any(|call| call.contains("이 입력을 폐기했습니다"))
    );
    drop(intake);
    drop(original_runtime);
}
