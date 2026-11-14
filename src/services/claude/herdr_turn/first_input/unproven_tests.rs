//! A first input whose idle source proves no boundary by the end of its readiness budget ends at
//! the owner retained with a resend ask; one whose turn still runs waits at the front.
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
    /// Resumed and idle, then the daemon restarts and attaches the same transcript again.
    Restarted,
    /// A fresh launch whose SessionStart the ledger refused.
    StartRefused,
    /// Restarted like the first, over a turn the transcript shows still running.
    Running,
}

#[test]
fn an_idle_source_that_proves_nothing_ends_retained_and_a_running_one_waits() {
    for (case, channel) in [
        (Case::Restarted, 816_101u64),
        (Case::StartRefused, 816_102),
        (Case::Running, 816_103),
    ] {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        readiness::initialize();
        let rig = HerdrRig::start();
        let _registry = rig.registry_on_this_thread();
        let logical = format!("i8161-unproven-{}", uuid::Uuid::new_v4().simple());
        let native = uuid::Uuid::new_v4().to_string();
        let projects = root.path().join("projects");
        let path = projects.join("workspace").join(format!("{native}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = chrono::Utc::now() - chrono::Duration::hours(1);
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
            Case::Restarted => rows(&[
                asked,
                said,
                json!({"type":"system","subtype":"turn_duration","timestamp":old}),
                json!({"type":"last-prompt"}),
            ]),
            Case::Running => rows(&[asked, said]),
            Case::StartRefused => (),
        }
        let resumed = case != Case::StartRefused;
        let context: binding_context::BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":chrono::Utc::now(),"execution_nonce":NONCE,
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
            tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
            readiness::tests::daemon_restart(&logical);
            attach();
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
        let (_, target) = bound_target(&rig, &logical, 1);
        let attached = Attached {
            nonce: NONCE.into(),
            session_id: native.clone(),
            transcript: path.clone(),
            target,
            bound: true,
            physical: None,
        };
        let session = logical.clone();
        let leaf = effect::created(
            Box::new(move |frame: String| {
                // The readiness budget runs out on the leaf's own thread.
                let budget = Some(Duration::from_millis(450));
                composer::READY_WAIT_FOR_TESTS.with(|wait| wait.set(budget));
                let result = guarded_outcome::capture_preparation(|| {
                    let (sender, receiver) = std::sync::mpsc::channel();
                    let result = write(&session, None, attached, &frame, &sender).map(drop);
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
        assert!(
            matches!(result, effect::Completion::NotSent(_)),
            "{case:?}: {result:?}"
        );
        assert!(rig.sends().is_empty(), "{case:?}: no key reaches the pane");
        let read = || {
            runtime
                .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
                .unwrap()
        };
        let snapshot = read();
        assert!(snapshot["head_injection"].is_null(), "{case:?}: {snapshot}");
        let (items, retained) = (
            snapshot["items"].as_array().unwrap(),
            snapshot["retained_inputs"].as_array().unwrap(),
        );
        if case == Case::Running {
            assert_eq!(items.len(), 1, "a running turn keeps the original in front");
            assert!(retained.is_empty(), "a running turn retains nothing");
            assert!(
                held_watch::armed(channel),
                "a running turn leaves its watch"
            );
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
            assert!(rig.sends().is_empty(), "{case:?}: still no key");
        }
        drop(entered);
        tui_prompt_dedupe::clear_tmux_runtime_binding(&logical);
    }
}
