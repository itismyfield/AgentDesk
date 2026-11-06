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
