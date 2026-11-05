//! A fresh writer's first key against a stop that took its token's gate first.
use super::*;
use crate::services::claude_tui::submission_fence::IdleStashContext;
use crate::services::provider::cancel_token_claude_interrupt::StopCancel;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

/// The managed attempt; its fence claim is the writer's last step before its first key.
struct Attempt {
    session: String,
    channel: u64,
    stop: Option<Arc<CancelToken>>,
    starts: AtomicUsize,
    outcomes: Mutex<Vec<InputRun>>,
}
impl SubmissionFence for Attempt {
    fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
        Ok(())
    }
    fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
        None
    }
    fn idle_stash_context(&self) -> Option<IdleStashContext> {
        Some(IdleStashContext {
            channel: self.channel,
            target: self.session.clone(),
            available: true,
        })
    }
    fn idle_stash_admits(&self, target: &str) -> bool {
        target == self.session
    }
    fn claim_idle_stash(&self, _: &str) -> Result<(), PreSubmitPersistenceRefused> {
        if let Some(token) = &self.stop {
            let stop = StopCancel::decide(Some(token.clone()), "!stop".into());
            assert!(matches!(stop, StopCancel::Published(_)));
        }
        Ok(())
    }
}
impl guarded_outcome::Owner for Attempt {
    fn started(&self) {
        self.starts.fetch_add(1, Ordering::SeqCst);
    }
    fn finished(&self, run: &InputRun) {
        self.outcomes.lock().unwrap().push(run.clone());
    }
}

/// Through the fresh entry, a stop at the writer's last step before its first key or while the
/// pane starts leaves no key; a writer that keyed first keeps its write and the gate.
#[test]
fn a_fresh_writer_sends_no_key_once_a_stop_took_its_gate() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let cases = [
        "stop-before-first-key",
        "stop-during-readiness",
        "writer-first",
    ];
    for (n, case) in cases.into_iter().enumerate() {
        let name = format!("gate-{case}");
        let before = match case {
            "stop-during-readiness" => "Claude is starting…\n".to_string(),
            _ => view("", false),
        };
        let idle = "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n";
        let root = renderer_with(&f, &name, "normal", before, idle, PROMPT);
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: root.join("transcript").display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let token = Arc::new(CancelToken::new());
        let sink = token.native_stop.receipt_sink();
        sink.arm_first_key();
        let attempt = Arc::new(Attempt {
            session: name.clone(),
            channel: 7_754_400 + n as u64,
            stop: (case == "stop-before-first-key").then(|| token.clone()),
            starts: AtomicUsize::new(0),
            outcomes: Mutex::new(Vec::new()),
        });
        // A stop while the pane is still starting reaches the same cancel decision.
        let stopper = (case == "stop-during-readiness").then(|| {
            let token = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                matches!(
                    StopCancel::decide(Some(token), "!stop".into()),
                    StopCancel::Published(_)
                )
            })
        });
        let result = guarded_outcome::with_receipt_sink(Some(&token), || {
            guarded_outcome::with_owner(Some(attempt.clone()), || {
                with_scope(Some(attempt.clone()), || {
                    crate::services::claude_tui::input::send_prompt_with_readiness(
                        &name,
                        PROMPT,
                        PromptReadinessKind::FreshTurn,
                        Some(&token),
                    )
                })
            })
        });
        if let Some(stopper) = stopper {
            assert!(
                stopper.join().unwrap(),
                "{case}: the stop published main's cancel"
            );
        }
        assert_eq!(
            attempt.starts.load(Ordering::SeqCst),
            usize::from(case == "writer-first"),
            "{case}: only the writer that takes the first-key gate starts its owner"
        );
        let keys = || {
            std::thread::sleep(Duration::from_millis(300));
            events(&root)
        };
        match case {
            "writer-first" => {
                assert!(result.is_ok(), "{case}: {result:?}");
                assert_eq!(keys(), ["paste", "Enter"], "{case}");
                let stop = StopCancel::decide(Some(token.clone()), "!stop".into());
                assert!(matches!(stop, StopCancel::Published(_)), "{case}");
                assert!(
                    sink.may_have_written() && !sink.stopped_before_key(),
                    "{case}"
                );
            }
            _ => {
                assert!(result.is_err(), "{case}: {result:?}");
                assert!(keys().is_empty(), "{case}: no key reached the pane");
                assert!(sink.stopped_before_key(), "{case}");
                assert!(
                    !sink.writer_first_key(),
                    "{case}: the gate refuses a later key"
                );
            }
        }
        if case == "stop-before-first-key" {
            let outcomes = attempt.outcomes.lock().unwrap().clone();
            assert_eq!(outcomes, [InputRun::Cancelled { confirmed: 0 }], "{case}");
        }
    }
}
