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

/// What an in-CLI `/clear` appends to the session it leaves (Claude 2.1.316, production launch).
fn clear_suffix() -> String {
    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hook_payload/claude-2.1.316.transcript-prefix.jsonl"
    ));
    let line = FIXTURE
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|row| row["case"] == "suffix_after_cli_clear")
        .unwrap();
    format!("{}\n", line["record"])
}

/// A live pane whose observer is built on session `before` while `after` may be adopted later.
struct ClearedPane {
    _dedupe: std::sync::MutexGuard<'static, ()>,
    _rotations: std::sync::MutexGuard<'static, ()>,
    // Declared first so its PATH is restored while `_root` still holds the environment lock.
    tmux: crate::services::claude_tui::input::submission::tests::ScriptedTmux,
    _root: crate::config::TestRuntimeRootGuard,
    dir: tempfile::TempDir,
    session: String,
    before: String,
    after: String,
}

impl ClearedPane {
    fn new() -> Self {
        use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view};
        let dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Rotation tests clear the adopted-session map under their own lock; hold it so they wait.
        let rotations =
            crate::services::tui_prompt_dedupe::lock_claude_session_rotations_for_tests();
        let root = crate::config::TestRuntimeRootGuard::new();
        let pane = Self {
            _dedupe: dedupe,
            _rotations: rotations,
            _root: root,
            tmux: ScriptedTmux::new(&view("", false)),
            dir: tempfile::tempdir().unwrap(),
            session: format!("AgentDesk-claude-clear-{}", uuid::Uuid::new_v4().simple()),
            before: uuid::Uuid::new_v4().to_string(),
            after: uuid::Uuid::new_v4().to_string(),
        };
        // A live pane, so only a receipt, a rebind or the grace ends the observer's wait.
        pane.tmux.set_dead(false);
        let idle = "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n";
        std::fs::write(pane.path(&pane.before), idle).unwrap();
        pane.bind(&pane.before.clone());
        pane
    }

    fn path(&self, sid: &str) -> std::path::PathBuf {
        self.dir.path().join(format!("{sid}.jsonl"))
    }

    fn bind(&self, sid: &str) {
        use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &self.session,
            TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: self.path(sid).display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(sid.to_string()),
                last_offset: 0,
                relay_last_offset: None,
            },
        )
    }

    /// One answered turn of `prompt` in session `sid`.
    fn turn(sid: &str, prompt: &str, text: &str) -> String {
        let user = serde_json::json!({"type":"user","uuid":format!("u-{sid}"),"sessionId":sid,
            "message":{"role":"user","content":prompt}});
        let answer = serde_json::json!({"type":"assistant","message":{"role":"assistant",
            "content":[{"type":"text","text":text}]}});
        let stop = serde_json::json!({"type":"system","subtype":"stop_hook_summary"});
        format!("{user}\n{answer}\n{stop}\n")
    }

    /// Runs `observe` to its end (cancelled after 15 s) and returns what it relayed and how long.
    fn read(
        &self,
        observe: Box<dyn FnOnce() -> Result<(), String> + Send>,
        cancel: &std::sync::Arc<CancelToken>,
        receiver: std::sync::mpsc::Receiver<StreamMessage>,
    ) -> (Result<(), String>, String, std::time::Duration) {
        let started = std::time::Instant::now();
        let (finished, done) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let result = observe();
            let _ = finished.send(());
            result
        });
        if done
            .recv_timeout(std::time::Duration::from_secs(15))
            .is_err()
        {
            cancel
                .cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let result = reader.join().unwrap();
        let text = receiver
            .try_iter()
            .filter_map(|message| match message {
                StreamMessage::Text { content } => Some(content),
                _ => None,
            })
            .collect();
        (result, text, started.elapsed())
    }
}

impl Drop for ClearedPane {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.session);
        crate::services::tui_prompt_dedupe::forget_hook_adopted_claude_session_id(&self.session);
    }
}

/// `/clear`'s session is adopted after the owner's observer was built and the old session took
/// the clear's metadata: the submit's answer is read from the session the pane moved to.
#[test]
fn an_owner_observer_built_before_clear_adoption_reads_the_adopted_session() {
    let pane = ClearedPane::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let observe = owner_observer(
        (&pane.session, "after clear", 7_896_203),
        (None, pane.dir.path()),
        sender,
        cancel.clone(),
    )
    .expect("the observer resolves the pane's session before the submit");
    let mut old = std::fs::OpenOptions::new()
        .append(true)
        .open(pane.path(&pane.before))
        .unwrap();
    std::io::Write::write_all(&mut old, clear_suffix().as_bytes()).unwrap();
    let answered = ClearedPane::turn(
        &pane.after,
        "after clear",
        "answered in the adopted session",
    );
    std::fs::write(pane.path(&pane.after), answered).unwrap();
    pane.bind(&pane.after.clone());
    crate::services::tui_prompt_dedupe::record_hook_adopted_claude_session_id(
        &pane.session,
        &pane.after,
    );
    let (result, text, _) = pane.read(observe, &cancel, receiver);
    assert!(
        text.contains("answered in the adopted session"),
        "the observer must read the adopted session's answer: {text:?}"
    );
    assert_eq!(result, Ok(()));
}

/// With no `/clear`, the submit's own receipt in the observer's session starts the read at once,
/// from the observer's offset, without waiting out the rebind grace.
#[test]
fn an_owner_observer_reads_its_own_session_once_the_submit_receipt_lands_there() {
    let pane = ClearedPane::new();
    let frame = "own session input";
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let observe = owner_observer(
        (&pane.session, frame, 7_896_205),
        (None, pane.dir.path()),
        sender,
        cancel.clone(),
    )
    .expect("the observer resolves the pane's session before the submit");
    let answered = ClearedPane::turn(&pane.before, frame, "answered in its own session");
    pane.tmux
        .on_enter(&pane.path(&pane.before), answered.trim_end());
    let sent = guarded_outcome::with_receipt_sink(Some(&cancel), || {
        crate::services::claude_tui::input::send_warm_followup_owned(&pane.session, frame, None)
    });
    assert!(sent.is_ok(), "{sent:?}");
    let (result, text, took) = pane.read(observe, &cancel, receiver);
    assert!(text.contains("answered in its own session"), "{text:?}");
    assert_eq!(result, Ok(()));
    assert!(took < REBIND_GRACE, "the receipt decides at once: {took:?}");
}

/// The rebind is asked again while the adoption is still pending; the old session growing with
/// metadata alone never decides for it.
#[test]
fn a_rebind_pending_at_the_first_poll_is_followed_once_adopted() {
    let pane = ClearedPane::new();
    let observed = Resolution {
        session_id: pane.before.clone(),
        transcript_path: pane.path(&pane.before),
        resume: true,
    };
    let mut old = std::fs::OpenOptions::new()
        .append(true)
        .open(&observed.transcript_path)
        .unwrap();
    std::io::Write::write_all(&mut old, clear_suffix().as_bytes()).unwrap();
    std::fs::write(pane.path(&pane.after), "").unwrap();
    let adopted = Resolution {
        session_id: pane.after.clone(),
        transcript_path: pane.path(&pane.after),
        resume: true,
    };
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let rebound = rebound_source(&pane.session, &observed, &CancelToken::new(), || {
        let n = asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (n > 0).then(|| adopted.clone())
    });
    assert_eq!(rebound.map(|now| now.session_id), Some(pane.after.clone()));
    assert!(asked.load(std::sync::atomic::Ordering::SeqCst) >= 2);
}

/// `/clear`'s session is adopted after the grace, while the observer reads the old session with
/// no receipt of this submit: the answer is read once from the adopted session and the turn ends.
#[test]
fn an_owner_observer_moves_once_to_a_session_adopted_after_the_grace() {
    let pane = ClearedPane::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let observe = owner_observer(
        (&pane.session, "late clear", 7_889_301),
        (None, pane.dir.path()),
        sender,
        cancel.clone(),
    )
    .expect("the observer resolves the pane's session before the submit");
    let mut old = std::fs::OpenOptions::new()
        .append(true)
        .open(pane.path(&pane.before))
        .unwrap();
    std::io::Write::write_all(&mut old, clear_suffix().as_bytes()).unwrap();
    let (finished, done) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let result = observe();
        let _ = finished.send(());
        result
    });
    // The old session's metadata is read only once the grace is over.
    let mut seen = Vec::new();
    let reading_old = std::time::Instant::now() + REBIND_GRACE + std::time::Duration::from_secs(10);
    loop {
        let left = reading_old.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(left) {
            Ok(StreamMessage::OutputOffset { .. }) => break,
            Ok(other) => seen.push(other),
            Err(error) => panic!("the observer never read the old session: {error}"),
        }
    }
    std::fs::write(
        pane.path(&pane.after),
        ClearedPane::turn(&pane.after, "late clear", "answered after the grace"),
    )
    .unwrap();
    pane.bind(&pane.after.clone());
    crate::services::tui_prompt_dedupe::record_hook_adopted_claude_session_id(
        &pane.session,
        &pane.after,
    );
    if done
        .recv_timeout(std::time::Duration::from_secs(30))
        .is_err()
    {
        cancel
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let result = reader.join().unwrap();
    let text: String = seen
        .into_iter()
        .chain(receiver.try_iter())
        .filter_map(|message| match message {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(
        text.matches("answered after the grace").count(),
        1,
        "the adopted session's answer is relayed once: {text:?}"
    );
    assert_eq!(result, Ok(()));
}

/// This submit's receipt in the observed session keeps the read there even when the pane is
/// later verified on another session, so one turn's answer is never read from two transcripts.
#[test]
fn a_receipt_in_the_observed_session_keeps_the_read_from_moving() {
    let pane = ClearedPane::new();
    let frame = "own session input";
    let (sender, _receiver) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let at = (pane.session.as_str(), frame, 7_889_302);
    let _observe = owner_observer(at, (None, pane.dir.path()), sender, cancel.clone());
    let answered = ClearedPane::turn(&pane.before, frame, "answered in its own session");
    pane.tmux
        .on_enter(&pane.path(&pane.before), answered.trim_end());
    let sent = guarded_outcome::with_receipt_sink(Some(&cancel), || {
        crate::services::claude_tui::input::send_warm_followup_owned(&pane.session, frame, None)
    });
    assert!(sent.is_ok(), "{sent:?}");
    std::fs::write(pane.path(&pane.after), "").unwrap();
    let observed = Resolution {
        session_id: pane.before.clone(),
        transcript_path: pane.path(&pane.before),
        resume: true,
    };
    let adopted = Resolution {
        session_id: pane.after.clone(),
        transcript_path: pane.path(&pane.after),
        resume: true,
    };
    let (leave, moved) = late_rebind(&observed, &cancel, move || Some(adopted.clone()));
    assert!(
        !leave(),
        "a receipt in the observed session must keep the read"
    );
    assert!(moved.get().is_none());
}

/// A cleared pane puts the scripted tmux's PATH back before its root guard unlocks the environment,
/// so the next locked test never has its own tmux shim overwritten.
#[test]
fn a_cleared_pane_restores_path_before_the_root_unlocks() {
    crate::config::test_env::teardown_probe::assert_restores_after_return(
        concat!(
            module_path!(),
            "::a_cleared_pane_restores_path_before_the_root_unlocks"
        ),
        false,
        || {
            let baseline = std::env::var_os("PATH");
            let restored = std::rc::Rc::new(std::cell::Cell::new(false));
            let observed = restored.clone();
            let _hook = crate::config::test_env::teardown_probe::on_before_restore(move || {
                assert_eq!(
                    std::env::var_os("PATH"),
                    baseline,
                    "PATH restored inside the lock"
                );
                observed.set(true);
            });
            drop(ClearedPane::new());
            assert!(restored.get(), "root teardown hook must run");
        },
    );
}
