//! The owner's idle Claude submit after its observer: a retired session notifies, never resends.
use super::*;

/// A session that died before any output reaches the turn's relay as a notice that nothing
/// was resent, and the owner's turn ends in error; an observed end passes through unnoticed.
#[test]
fn a_retired_owner_session_notifies_without_resending() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let retired = ClaudeTuiWarmFollowupSubmitOutcome::FallThroughRecreate;
    assert!(owner_settles(retired, &sender).is_err());
    let sent: Vec<_> = receiver.try_iter().collect();
    assert!(
        matches!(&sent[..], [StreamMessage::Text { content }, StreamMessage::Done { .. }]
            if content.contains(OWNER_RETIRED_NOTICE)),
        "{sent:?}"
    );
    let ended = ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
    assert_eq!(owner_settles(ended, &sender), Ok(()));
    assert!(receiver.try_recv().is_err());
}

#[test]
fn b2_legacy_warm_submit_and_reader_collect_the_actual_token() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view};
    use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-warm.jsonl");
    let (session, sid, frame) = ("b2-legacy-warm", "warm-sid", "warm own");
    let user = |id: &str| {
        serde_json::json!({"type":"user","uuid":id,"sessionId":sid,
        "message":{"role":"user","content":frame}})
    };
    let past = format!(
        "{}\n{{\"type\":\"system\",\"subtype\":\"turn_duration\"}}\n",
        user("past")
    );
    std::fs::write(&path, &past).unwrap();
    tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(sid.into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.on_enter(&path,&format!("{}\n{}\n{}",user("warm-a"),
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"finished"}]}}),
        serde_json::json!({"type":"system","subtype":"stop_hook_summary"})));
    let token = std::sync::Arc::new(CancelToken::new());
    token.bind_claude_tmux_session(session);
    let (sender, _receiver) = std::sync::mpsc::channel();
    let outcome = guarded_outcome::with_owner(None, || {
        run_claude_tui_warm_followup_submit_and_stream(
            &FollowupHost::legacy_tmux(session),
            ClaudeTuiWarmFollowupSubmitPlan {
                submit_existing_session: true,
                recheck_busy_before_submit: false,
            },
            false,
            session,
            sid,
            &path,
            path.to_str().unwrap(),
            frame,
            sender,
            Some(token.clone()),
            None,
            crate::services::claude_tui::hook_server::subscribe_hook_events(),
        )
    });
    assert!(matches!(
        outcome,
        ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()))
    ));
    let pin = token
        .native_stop
        .receipt_sink()
        .observed()
        .expect("legacy warm must forward its token receipt")
        .unwrap();
    assert_eq!(pin.uuid, "warm-a");
    assert_eq!(
        pin.range.0,
        past.len() as u64,
        "the writer boundary excludes the past input"
    );
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}

/// The legacy warm submit takes its token's first-key gate: a stop deciding while the writer reads
/// the composer leaves the pane without a key, and a writer that keyed first leaves MayHaveWritten.
#[test]
fn a_legacy_warm_submit_keys_only_through_its_tokens_first_key_gate() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view, wait};
    use crate::services::provider::cancel_token_claude_interrupt::StopCancel;
    use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (n, stop) in [true, false].into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-warm-gate.jsonl");
        let (session, sid, frame) = (format!("legacy-warm-gate-{n}"), "gate-sid", "gated warm");
        let user = |id: &str| {
            serde_json::json!({"type":"user","uuid":id,"sessionId":sid,
            "message":{"role":"user","content":frame}})
        };
        std::fs::write(
            &path,
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
        )
        .unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(sid.into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let tmux = ScriptedTmux::new(&view("", false));
        tmux.on_enter(&path,&format!("{}\n{}\n{}",user("gate-a"),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}),
            serde_json::json!({"type":"system","subtype":"stop_hook_summary"})));
        let token = std::sync::Arc::new(CancelToken::new());
        token.bind_claude_tmux_session(&session);
        let sink = token.native_stop.receipt_sink();
        sink.arm_first_key();
        let stopper = stop.then(|| {
            let (held, go) = tmux.hold_first("capture-pane");
            let token = token.clone();
            std::thread::spawn(move || {
                wait("the warm writer reads the composer", || held.exists());
                let decided = StopCancel::decide(Some(token), "!stop".into());
                std::fs::write(go, "").unwrap();
                matches!(decided, StopCancel::Published(_))
            })
        });
        let (sender, _receiver) = std::sync::mpsc::channel();
        let outcome = guarded_outcome::with_owner(None, || {
            run_claude_tui_warm_followup_submit_and_stream(
                &FollowupHost::legacy_tmux(&session),
                ClaudeTuiWarmFollowupSubmitPlan {
                    submit_existing_session: true,
                    recheck_busy_before_submit: false,
                },
                false,
                &session,
                sid,
                &path,
                path.to_str().unwrap(),
                frame,
                sender,
                Some(token.clone()),
                None,
                crate::services::claude_tui::hook_server::subscribe_hook_events(),
            )
        });
        if let Some(stopper) = stopper {
            assert!(stopper.join().unwrap(), "the stop published main's cancel");
            assert!(
                matches!(
                    outcome,
                    ClaudeTuiWarmFollowupSubmitOutcome::Stopped(InputRun::Cancelled {
                        confirmed: 0
                    })
                ),
                "stop first: no submit"
            );
            assert!(tmux.sends().is_empty(), "no key: {:?}", tmux.sends());
            assert!(sink.stopped_before_key());
        } else {
            assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
            assert!(sink.may_have_written(), "the writer keyed first");
        }
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}

/// `/clear`'s session is adopted only after the owner's observer was built: the submit's answer is
/// read from the session the pane moved to, not the one the observer resolved before it.
#[test]
fn an_owner_observer_built_before_clear_adoption_reads_the_adopted_session() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view};
    use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
    let _dedupe = tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Rotation tests clear the adopted-session map under their own lock; hold it so they wait.
    let _rotations = tui_prompt_dedupe::lock_claude_session_rotations_for_tests();
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::new(&view("", false));
    let dir = tempfile::tempdir().unwrap();
    let session = format!("AgentDesk-claude-clear-{}", uuid::Uuid::new_v4().simple());
    let (before, after) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    let path = |sid: &str| dir.path().join(format!("{sid}.jsonl"));
    let bind = |sid: &str| {
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path(sid).display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(sid.to_string()),
                last_offset: 0,
                relay_last_offset: None,
            },
        )
    };
    std::fs::write(
        path(&before),
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    bind(&before);
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let observe = owner_observer(
        (&session, "after clear", 7_896_203),
        (None, dir.path()),
        sender,
        cancel.clone(),
    )
    .expect("the observer resolves the pane's session before the submit");
    // The clear's session is verified after the observer: the submitted input runs there.
    let user = serde_json::json!({"type":"user","uuid":"clear-a","sessionId":after,
        "message":{"role":"user","content":"after clear"}});
    let answer = serde_json::json!({"type":"assistant","message":{"role":"assistant",
        "content":[{"type":"text","text":"answered in the adopted session"}]}});
    let stop = serde_json::json!({"type":"system","subtype":"stop_hook_summary"});
    std::fs::write(path(&after), format!("{user}\n{answer}\n{stop}\n")).unwrap();
    bind(&after);
    tui_prompt_dedupe::record_hook_adopted_claude_session_id(&session, &after);
    let (finished, done) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let result = observe();
        let _ = finished.send(());
        result
    });
    let ended = done
        .recv_timeout(std::time::Duration::from_secs(15))
        .is_ok();
    if !ended {
        cancel
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let result = reader.join().unwrap();
    tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    tui_prompt_dedupe::forget_hook_adopted_claude_session_id(&session);
    let text: String = receiver
        .try_iter()
        .filter_map(|message| match message {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        })
        .collect();
    assert!(
        ended && text.contains("answered in the adopted session"),
        "the observer must read the adopted session's answer: ended={ended}, text={text:?}"
    );
    assert_eq!(result, Ok(()));
}
