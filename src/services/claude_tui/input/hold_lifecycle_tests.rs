use super::*;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};

fn empty() -> String {
    let border = "─".repeat(60);
    format!("⏺ Done.\n{border}\n❯ \n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n")
}

#[test]
#[cfg(unix)]
fn a2_legacy_confirmation_rechecks_person_ownership_before_enter() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let view = crate::services::claude_tui::host_input::guarded_submission::idle_stash::tests::view;
    for (prompt, changed) in [
        ("[User: test]\n원문 보존", true),
        ("첫째 줄\n원문 보존", false),
    ] {
        let session = format!("a2-confirm-{}", uuid::Uuid::new_v4());
        let initial = view(prompt, false);
        let person = view(
            if changed {
                "사람이 바꾼 초안"
            } else {
                prompt
            },
            false,
        );
        assert_eq!(
            crate::services::claude_tui::busy_inject::composer_owner_in_session(&session, &person),
            crate::services::claude_tui::busy_inject::ComposerOwner::Person
        );
        if changed {
            assert_eq!(
                crate::services::claude_tui::busy_inject::composer_owner_in_session(
                    &session, &initial
                ),
                crate::services::claude_tui::busy_inject::ComposerOwner::AgentDesk
            );
        } else {
            assert!(
                crate::services::claude_tui::prompt_readiness::composer_still_holds_prompt(
                    &person, prompt
                ),
                "the ownership-only control keeps exactly the submitted text"
            );
        }
        let spy = SpyGuard::install(SpyState {
            captures: std::iter::repeat_n(Some(empty()), 30).collect(),
            captures_after_send: Some([Some(initial), Some(person), Some(empty())].into()),
            ..SpyState::default()
        });
        let result = guarded_outcome::capture_preparation(|| {
            send_prompt_with_readiness(
                &session,
                prompt,
                PromptReadinessKind::FreshTurn,
                Some(&CancelToken::new()),
            )
        });
        let calls = spy.calls();
        assert_eq!(
            calls.iter().filter(|c| c.as_str() == "keys:Enter").count(),
            1,
            "same text is not retry permission without ownership: changed={changed} {calls:?}"
        );
        assert!(
            matches!(result, Err(SubmissionError::Held(ref hold)) if !hold.may_requeue()),
            "ownership loss after payload must be uncertain: {result:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("C-u") || c.contains("Escape") || c.contains("BSpace"))
        );
    }
}

#[test]
fn a2_legacy_send_errors_never_erase_uncertain_paste() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for fail_at in [Some(1), Some(2), None] {
        let session = format!("a2-in-send-{}", uuid::Uuid::new_v4());
        let spy = SpyGuard::install(SpyState {
            captures: std::iter::repeat_n(Some(empty()), 30).collect(),
            captures_after_send: Some(std::collections::VecDeque::new()),
            fail_send: fail_at.map(|at| (at, Err("uncertain transport ACK".into()))),
            ..SpyState::default()
        });
        let result = send_fresh_prompt(&session, "첫째 줄\n원문 보존", Some(&CancelToken::new()));
        assert!(result.is_err(), "{fail_at:?}");
        let calls = spy.calls();
        let cleanup: Vec<_> = calls
            .iter()
            .filter(|call| {
                call.starts_with("keys:")
                    && (call.contains("C-u") || call.contains("Escape") || call.contains("BSpace"))
            })
            .collect();
        assert!(
            cleanup.is_empty(),
            "uncertain submission must preserve raw payload: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("paste:"))
                .count(),
            1
        );
    }
}

#[test]
fn a2_last_cleanup_capture_must_still_prove_the_draft_is_ours() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for capture in [
        None,
        Some("unrecognized overlay".into()),
        Some(empty().replace("❯ \n", "❯ person's draft\n")),
    ] {
        let session = format!("a2-cleanup-owner-{}", uuid::Uuid::new_v4());
        let spy = SpyGuard::install(SpyState {
            captures: [capture].into(),
            ..SpyState::default()
        });
        clear_prompt_draft_before_error(&session);
        assert!(
            !spy.calls().iter().any(|c| c.starts_with("keys:")),
            "unknown and person drafts get no cleanup: {:?}",
            spy.calls()
        );
    }
}

#[test]
fn a2_cancel_before_payload_keeps_the_existing_cancellation_outcome() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let session = format!("a2-cancel-before-{}", uuid::Uuid::new_v4());
    let token = CancelToken::new();
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let spy = SpyGuard::install(SpyState::default());
    let result = guarded_outcome::capture_preparation(|| {
        run_actions_with_submission_confirmation(
            &session,
            &[
                TuiInputAction::PasteBuffer("raw\ninput".into()),
                TuiInputAction::Enter,
            ],
            Some("raw\ninput"),
            Some(&token),
        )
        .map_err(SubmissionError::from)
    });
    assert!(
        matches!(result, Err(SubmissionError::Legacy(_))),
        "cancellation before payload is not an uncertain submission: {result:?}"
    );
    assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
        &session
    ));
    assert!(
        !spy.calls()
            .iter()
            .any(|c| c.starts_with("keys:") || c.starts_with("paste:"))
    );
}

#[test]
#[cfg(unix)]
fn a2_fresh_empty_echo_waits_for_receipt_or_retries_owned_literal() {
    #[derive(Clone)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let _root = crate::config::TestRuntimeRootGuard::new();
    let view = crate::services::claude_tui::host_input::guarded_submission::idle_stash::tests::view;
    let prompt = format!("[User: test (ID: 123)] {}", "x".repeat(78));
    assert_eq!(prompt.chars().count(), 101);
    assert_eq!(prompt.matches('\n').count(), 0);
    for (stale, records) in [
        (None, 0),
        (None, 1),
        (None, 2),
        (Some(None), 0),
        (Some(None), 1),
        (Some(None), 2),
        (Some(Some("old-sid")), 0),
        (Some(Some("old-sid")), 1),
        (Some(Some("old-sid")), 2),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.jsonl");
        let session = format!("a2-empty-echo-{}", uuid::Uuid::new_v4());
        let sid = "fresh-receipt-sid";
        if let Some(old_sid) = stale {
            let old = dir.path().join("old.jsonl");
            std::fs::write(&old, "old transcript\n").unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                &session,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                    output_path: old.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: old_sid.map(str::to_owned),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
        }
        let prefix = view(&prompt[..60], false);
        assert_eq!(
            crate::services::claude_tui::busy_inject::composer_owner_in_session(&session, &prefix),
            crate::services::claude_tui::busy_inject::ComposerOwner::AgentDesk
        );
        let output = path.clone();
        let record = serde_json::json!({"type":"user", "sessionId":sid,
            "message":{"content":prompt}})
        .to_string()
            + "\n";
        let spy = SpyGuard::install(SpyState {
            captures: std::iter::repeat_n(Some(empty()), 30).collect(),
            captures_after_send: Some(
                [Some(view("", false)), Some(prefix.clone()), Some(prefix)].into(),
            ),
            on_send: Some(Box::new(move |call| {
                if records > 0 && call == "keys:Enter" {
                    std::fs::write(&output, record.repeat(records)).unwrap();
                }
            })),
            ..SpyState::default()
        });
        let owner = Arc::new(ReceiptOwner(std::sync::Mutex::new(None)));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = log.clone();
        crate::logging::test_capture::pin_callsite_interest();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || Capture(output.clone()))
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            guarded_outcome::with_owner(Some(owner), || {
                guarded_outcome::fresh_receipt(&session, &prompt, path.to_str().unwrap(), sid);
                send_fresh_prompt(&session, &prompt, Some(&CancelToken::new()))
            })
        });
        let calls = spy.calls();
        let enters = calls.iter().filter(|c| c.as_str() == "keys:Enter").count();
        assert!(calls.contains(&format!("literal:{prompt}")), "{calls:?}");
        if records > 0 {
            assert!(
                result.is_ok(),
                "exact user record proves the first Enter: {result:?}"
            );
            assert_eq!(enters, 1, "a native receipt vetoes every retransmission");
        } else {
            assert!(
                enters > 1,
                "empty echo without a user record must not silently accept Enter1+Ok: {result:?} {calls:?}"
            );
            assert!(enters <= PROMPT_SUBMIT_CONFIRM_RETRIES + 1);
        }
        let warnings = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        assert_eq!(
            warnings
                .matches("claude_tui prompt submit left a draft after Enter; retrying Enter")
                .count(),
            enters - 1,
            "every Enter retransmission retains its diagnostic warning: {warnings}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("BSpace") || c.contains("C-u") || c.contains("Escape"))
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}

#[cfg(unix)]
struct ReceiptOwner(std::sync::Mutex<Option<UserReceipt>>);
#[cfg(unix)]
impl guarded_outcome::Owner for ReceiptOwner {
    fn started(&self) {}
    fn finished(&self, _: &host_input::InputRun) {}
    fn submission_record_observed(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .as_mut()
            .is_some_and(UserReceipt::observed)
    }
    fn receipt(&self, receipt: UserReceipt) {
        *self.0.lock().unwrap() = Some(receipt);
    }
}

#[test]
#[cfg(unix)]
fn a2_fresh_busy_waits_for_a_user_record_after_five_seconds() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let session = format!("a2-busy-receipt-{}", uuid::Uuid::new_v4());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.jsonl");
    let prompt = "[User: test] delayed hook record";
    let busy = empty().replace("⏺ Done.", "✳ Architecting… (esc to interrupt)");
    assert!(crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&busy));
    let writer = Arc::new(std::sync::Mutex::new(None));
    let task = writer.clone();
    let mut output = Some(path.clone());
    let spy = SpyGuard::install(SpyState {
        captures_after_send: Some(
            std::iter::once(Some(empty()))
                .chain(std::iter::repeat_n(Some(busy), 256))
                .collect(),
        ),
        on_send: Some(Box::new(move |call| {
            if call == "keys:Enter"
                && let Some(path) = output.take()
            {
                *task.lock().unwrap() = Some(std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(5));
                    std::fs::write(path, serde_json::json!({"type":"user","sessionId":"sid","message":{"content":prompt}}).to_string()+"\n").unwrap();
                }));
            }
        })),
        ..SpyState::default()
    });
    let owner = Arc::new(ReceiptOwner(std::sync::Mutex::new(None)));
    let result = guarded_outcome::with_owner(Some(owner), || {
        guarded_outcome::fresh_receipt(&session, prompt, path.to_str().unwrap(), "sid");
        run_actions_with_submission_confirmation(
            &session,
            &[
                TuiInputAction::Literal(prompt.into()),
                TuiInputAction::Enter,
            ],
            Some(prompt),
            Some(&CancelToken::new()),
        )
    });
    writer.lock().unwrap().take().unwrap().join().unwrap();
    assert!(
        result.is_ok(),
        "busy native turn waits beyond four seconds without abandoning its receipt: {result:?}"
    );
    assert_eq!(
        spy.calls()
            .iter()
            .filter(|c| c.as_str() == "keys:Enter")
            .count(),
        1
    );
}

#[test]
#[cfg(unix)]
fn a2_fresh_empty_recheck_waits_and_accepts_the_last_receipt_veto() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct RecheckOwner {
        receipt: std::sync::Mutex<Option<UserReceipt>>,
        queries: AtomicUsize,
        path: std::path::PathBuf,
        record: String,
        immediate: bool,
        token: Arc<CancelToken>,
    }
    impl guarded_outcome::Owner for RecheckOwner {
        fn started(&self) {}
        fn finished(&self, _: &host_input::InputRun) {}
        fn receipt(&self, receipt: UserReceipt) {
            *self.receipt.lock().unwrap() = Some(receipt);
        }
        fn submission_record_observed(&self) -> bool {
            let query = self.queries.fetch_add(1, Ordering::SeqCst) + 1;
            let mut receipt = self.receipt.lock().unwrap();
            let before = receipt.as_mut().is_some_and(UserReceipt::observed);
            if query == 2 {
                // The second lookup follows the empty attributed recheck, after the prefix probe.
                std::fs::write(&self.path, &self.record).unwrap();
                if self.immediate {
                    // Another poll would observe cancellation; the existing receipt veto suffices.
                    self.token.cancelled.store(true, Ordering::SeqCst);
                    return receipt.as_mut().is_some_and(UserReceipt::observed);
                }
            }
            before
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let view = crate::services::claude_tui::host_input::guarded_submission::idle_stash::tests::view;
    let prompt = "[User: test] recheck race";
    for (immediate, styled) in [(true, false), (false, false), (false, true)] {
        let session = format!("a2-recheck-receipt-{}", uuid::Uuid::new_v4());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recheck.jsonl");
        let token = Arc::new(CancelToken::new());
        let owner = Arc::new(RecheckOwner {
            receipt: std::sync::Mutex::new(None),
            queries: AtomicUsize::new(0),
            path: path.clone(),
            record:
                serde_json::json!({"type":"user","sessionId":"sid","message":{"content":prompt}})
                    .to_string()
                    + "\n",
            immediate,
            token: token.clone(),
        });
        let recheck = if styled {
            empty().replace("❯", "\x1b[37m❯\x1b[0m")
        } else {
            empty()
        };
        let spy = SpyGuard::install(SpyState {
            captures_after_send: Some(
                std::iter::once(Some(view(prompt, false)))
                    .chain(std::iter::repeat_n(Some(empty()), 40))
                    .collect(),
            ),
            draft_capture_at: Some((1, Some(recheck))),
            ..SpyState::default()
        });
        let result = guarded_outcome::with_owner(Some(owner), || {
            guarded_outcome::fresh_receipt(&session, prompt, path.to_str().unwrap(), "sid");
            run_actions_with_submission_confirmation(
                &session,
                &[
                    TuiInputAction::Literal(prompt.into()),
                    TuiInputAction::Enter,
                ],
                Some(prompt),
                Some(&token),
            )
        });
        assert!(
            result.is_ok(),
            "empty recheck must await the native receipt and retain its immediate veto: immediate={immediate} styled={styled} {result:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls.iter().filter(|c| c.as_str() == "keys:Enter").count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.as_str() == "capture:draft")
                .count(),
            1
        );
    }
    let session = format!("a2-recheck-person-{}", uuid::Uuid::new_v4());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("person.jsonl");
    let person = view("person draft", false);
    assert_eq!(
        crate::services::claude_tui::busy_inject::composer_owner_in_session(&session, &person),
        crate::services::claude_tui::busy_inject::ComposerOwner::Person
    );
    let spy = SpyGuard::install(SpyState {
        captures_after_send: Some([Some(view(prompt, false))].into()),
        draft_capture_at: Some((1, Some(person))),
        ..SpyState::default()
    });
    let owner = Arc::new(ReceiptOwner(std::sync::Mutex::new(None)));
    let result = guarded_outcome::with_owner(Some(owner), || {
        guarded_outcome::fresh_receipt(&session, prompt, path.to_str().unwrap(), "sid");
        run_actions_with_submission_confirmation(
            &session,
            &[
                TuiInputAction::Literal(prompt.into()),
                TuiInputAction::Enter,
            ],
            Some(prompt),
            Some(&CancelToken::new()),
        )
    });
    assert!(result.is_err(), "Person ownership remains a fresh refusal");
    assert_eq!(
        spy.calls()
            .iter()
            .filter(|c| c.as_str() == "keys:Enter")
            .count(),
        1
    );
}

#[test]
#[cfg(unix)]
fn a2_ownerless_fresh_confirmation_keeps_the_legacy_empty_composer_result() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let session = format!("a2-ownerless-receipt-{}", uuid::Uuid::new_v4());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ownerless.jsonl");
    let prompt = "[User: test] ownerless";
    let spy = SpyGuard::install(SpyState {
        captures_after_send: Some(std::iter::repeat_n(Some(empty()), 80).collect()),
        ..SpyState::default()
    });
    let result = guarded_outcome::with_owner(None, || {
        guarded_outcome::fresh_receipt(&session, prompt, path.to_str().unwrap(), "sid");
        run_actions_with_submission_confirmation(
            &session,
            &[
                TuiInputAction::Literal(prompt.into()),
                TuiInputAction::Enter,
            ],
            Some(prompt),
            Some(&CancelToken::new()),
        )
    });
    assert!(
        result.is_ok(),
        "ownerless fresh scope must retain legacy submission confirmation: {result:?}"
    );
    assert_eq!(
        spy.calls()
            .iter()
            .filter(|c| c.as_str() == "keys:Enter")
            .count(),
        1
    );
}

#[test]
fn a2_fresh_receipt_wait_has_independent_ready_and_busy_boundaries() {
    let busy = empty().replace("⏺ Done.", "✳ Architecting… (esc to interrupt)");
    assert!(legacy_submission::fresh_receipt_may_wait(
        &empty(),
        Duration::from_millis(3999)
    ));
    assert!(!legacy_submission::fresh_receipt_may_wait(
        &empty(),
        Duration::from_secs(4)
    ));
    assert!(legacy_submission::fresh_receipt_may_wait(
        &busy,
        Duration::from_secs(5)
    ));
    assert!(!legacy_submission::fresh_receipt_may_wait(
        &busy,
        Duration::from_secs(30)
    ));
    assert!(legacy_submission::fresh_receipt_may_wait(
        "unrecognized overlay",
        Duration::from_secs(5)
    ));
    assert!(!legacy_submission::fresh_receipt_may_wait(
        "unrecognized overlay",
        Duration::from_secs(30)
    ));
}

#[test]
#[cfg(unix)]
fn bundle_fresh_folded_residue_requires_a_native_receipt_or_uncertain_hold() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let prompt = (0..15)
        .map(|n| format!("[User: bundle] line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let folded = empty().replace("❯ \n", "❯ [Pasted text #8 +14 lines]\n");
    for record_present in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launch.jsonl");
        let session = format!("bundle-fresh-folded-{}", uuid::Uuid::new_v4());
        let output = path.clone();
        let record = serde_json::json!({"type":"user", "sessionId":"sid",
            "message":{"content":prompt}})
        .to_string()
            + "\n";
        let spy = SpyGuard::install(SpyState {
            captures: std::iter::repeat_n(Some(empty()), 32).collect(),
            captures_after_send: Some(std::iter::repeat_n(Some(folded.clone()), 512).collect()),
            on_send: Some(Box::new(move |call| {
                if record_present && call == "keys:Enter" {
                    std::fs::write(&output, &record).unwrap();
                }
            })),
            ..SpyState::default()
        });
        let owner = Arc::new(ReceiptOwner(std::sync::Mutex::new(None)));
        let result = guarded_outcome::with_owner(Some(owner), || {
            guarded_outcome::fresh_receipt(&session, &prompt, path.to_str().unwrap(), "sid");
            send_prompt_with_readiness(
                &session,
                &prompt,
                PromptReadinessKind::FreshTurn,
                Some(&CancelToken::new()),
            )
        });
        let calls = spy.calls();
        assert_eq!(
            calls.iter().filter(|c| c.as_str() == "keys:Enter").count(),
            1,
            "folded residue never grants another Enter: {calls:?}"
        );
        assert_eq!(
            calls.iter().filter(|c| c.starts_with("paste:")).count(),
            1,
            "one payload only: {calls:?}"
        );
        assert_eq!(calls.iter().filter(|c| c.starts_with("load:")).count(), 1);
        if record_present {
            assert!(
                result.is_ok(),
                "exact launch receipt confirms folded input: {result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(SubmissionError::Held(ref hold))
                if hold.phase == preparation_hold::EffectPhase::PayloadUncertain
                    && hold.reason == "submission_uncertain"
                    && hold.detail.contains("no exact native user record")
                    && !hold.may_requeue()),
                "FOLDED_NO_RECEIPT_IS_UNCERTAIN: no record must never become Submitted: {result:?}"
            );
            assert!(crate::services::claude_tui::composer_lock::draft_guarded(
                &session
            ));
        }
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("BSpace") || c.contains("C-u") || c.contains("Escape"))
        );
    }
}

#[test]
#[cfg(unix)]
fn bundle_preparation_scope_carries_input_run_without_duplicate_owner_callbacks() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Observed(AtomicUsize);
    impl guarded_outcome::Owner for Observed {
        fn started(&self) {}
        fn finished(&self, _: &host_input::InputRun) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for run in [
        host_input::InputRun::Applied,
        host_input::InputRun::Cancelled { confirmed: 1 },
    ] {
        let owner = Arc::new(Observed(AtomicUsize::new(0)));
        let ((result, observed), _) = guarded_outcome::with_owner(Some(owner.clone()), || {
            (
                guarded_outcome::capture(|| {
                    guarded_outcome::capture_preparation(|| {
                        guarded_outcome::finish(&run);
                        if run == host_input::InputRun::Applied {
                            Ok(())
                        } else {
                            Err(SubmissionError::Held(PreparationHold::uncertain(
                                "scope-control",
                                "uncertain".into(),
                            )))
                        }
                    })
                }),
                (),
            )
        });
        assert_eq!(
            observed,
            Some(run.clone()),
            "the same owner's disposition reaches the outer capture"
        );
        assert_eq!(
            owner.0.load(Ordering::SeqCst),
            1,
            "a scope transfer never notifies twice"
        );
        if run != host_input::InputRun::Applied {
            assert!(matches!(result, Err(SubmissionError::Held(ref hold)) if !hold.may_requeue()));
        }
    }
}
