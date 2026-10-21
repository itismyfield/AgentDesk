use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn wrapper_followup_publishes_then_writes_then_marks_pending() {
    let session = "AgentDesk-claude-wrapper-followup-submit-order";
    let stale = CancelToken::new();
    let current = CancelToken::new();
    stale.bind_claude_tmux_session(session);
    let write_observed = AtomicUsize::new(0);

    submit_claude_wrapper_followup(Some(&current), session, || {
        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none(),
            "current generation must publish before FIFO write"
        );
        assert!(
            !current.claude_interrupt_submit_pending(),
            "submit-pending must remain false until FIFO flush succeeds"
        );
        write_observed.store(1, Ordering::Release);
        Ok(())
    })
    .unwrap();

    assert_eq!(write_observed.load(Ordering::Acquire), 1);
    assert!(current.claude_interrupt_submit_pending());
}

#[test]
fn wrapper_followup_write_failure_does_not_mark_submit_pending() {
    let token = CancelToken::new();
    assert!(
        submit_claude_wrapper_followup(
            Some(&token),
            "AgentDesk-claude-wrapper-write-failure",
            || { Err("write failed".to_string()) }
        )
        .is_err()
    );
    assert!(!token.claude_interrupt_submit_pending());
}

#[test]
fn wrapper_followup_read_end_clears_submit_pending() {
    for expected_ok in [true, false] {
        let token = CancelToken::new();
        token.mark_claude_interrupt_submit_pending();

        let observed = observe_claude_wrapper_followup(Some(&token), || {
            if expected_ok {
                Ok("completed")
            } else {
                Err("read failed".to_string())
            }
        });

        assert_eq!(observed.is_ok(), expected_ok);
        assert!(
            !token.claude_interrupt_submit_pending(),
            "terminal and error read exits must close the submitted window"
        );
    }
}

#[test]
fn session_generation_advance_blocks_stale_stop_operation() {
    let session = "AgentDesk-claude-session-generation-advance";
    let stale = CancelToken::new();
    let current = CancelToken::new();
    stale.bind_claude_tmux_session(session);
    assert!(stale.claim_claude_interrupt());

    current.bind_claude_tmux_session(session);
    let writes = AtomicUsize::new(0);
    let guard = stale.lock_current_claude_interrupt_session(session);
    if let Some(guard) = guard {
        let outcome = guard.commit_success({
            writes.fetch_add(1, Ordering::Relaxed);
            Ok::<(), ()>(())
        });
        assert_eq!(outcome, Ok(()));
    }

    assert!(
        stale
            .lock_current_claude_interrupt_session(session)
            .is_none()
    );
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    assert!(stale.release_claude_interrupt_claim());
}

#[test]
fn stale_rebind_cannot_replace_a_newer_session_generation() {
    let session = "AgentDesk-claude-session-stale-rebind";
    let stale = CancelToken::new();
    let current = CancelToken::new();
    stale.bind_claude_tmux_session(session);
    current.bind_claude_tmux_session(session);

    stale.bind_claude_tmux_session(session);

    assert!(
        stale
            .lock_current_claude_interrupt_session(session)
            .is_none()
    );
    assert!(
        current
            .lock_current_claude_interrupt_session(session)
            .is_some()
    );
}

#[test]
fn stale_pending_stop_is_rejected_after_next_generation_publishes() {
    let session = "AgentDesk-claude-session-pending-generation-advance";
    let stale = CancelToken::new();
    let current = CancelToken::new();
    stale.bind_claude_tmux_session(session);
    stale.mark_claude_interrupt_submit_pending();

    current.bind_claude_tmux_session(session);

    assert!(stale.claude_interrupt_submit_pending());
    assert!(
        stale
            .lock_current_claude_interrupt_session(session)
            .is_none(),
        "pending state must never bypass a newer generation publication"
    );
    assert!(
        current
            .lock_current_claude_interrupt_session(session)
            .is_some()
    );
}

#[test]
fn unmanaged_binding_stores_name_without_registry_authority() {
    let token = CancelToken::new();
    token.bind_unmanaged_session_name("AgentDesk-codex-name-only-binding");

    assert_eq!(
        token.tmux_session_name().as_deref(),
        Some("AgentDesk-codex-name-only-binding")
    );
    let binding = token
        .tmux_binding
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    assert!(matches!(
        authority::authorize(binding.as_ref()),
        KillAuthorization::Unregistered
    ));
}

#[test]
fn submitted_window_clears_when_delivery_commits() {
    let session = "AgentDesk-claude-session-pending-commit";
    let token = CancelToken::new();
    token.bind_claude_tmux_session(session);
    token.mark_claude_interrupt_submit_pending();
    assert!(token.claim_claude_interrupt());

    token
        .lock_current_claude_interrupt_session(session)
        .unwrap()
        .commit_success(Ok::<(), ()>(()))
        .unwrap();

    assert!(!token.claude_interrupt_submit_pending());
}

#[test]
fn successful_operation_commits_before_returning() {
    let session = "AgentDesk-claude-session-atomic-commit";
    let token = CancelToken::new();
    token.bind_claude_tmux_session(session);
    assert!(token.claim_claude_interrupt());

    token
        .lock_current_claude_interrupt_session(session)
        .expect("current generation must acquire delivery guard")
        .commit_success(Ok::<(), ()>(()))
        .expect("current generation must deliver");

    assert!(!token.claim_claude_interrupt());
    assert!(!token.release_claude_interrupt_claim());
}

fn herdr_owner(
    channel: &str,
    logical: &str,
) -> crate::db::dispatched_sessions::hosted_execution::HostedOwner {
    crate::db::dispatched_sessions::hosted_execution::HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "hash".into(),
        channel_id: channel.into(),
        logical_key: logical.into(),
        owner_node: "node".into(),
        runtime_root: "/tmp".into(),
    }
}

/// A live Herdr turn stays found by its own nonce whichever other turns, of this or another
/// channel, its session registers or drops.
#[test]
fn a_live_herdr_turn_is_found_by_its_own_nonce_beside_other_turns() {
    let logical = format!("AgentDesk-codex-turn-index-{}", std::process::id());
    let older = CancelToken::new();
    let a = older.prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("1", &logical));
    let newer = CancelToken::new();
    let b = newer.prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("2", &logical));
    let (a_nonce, b_nonce) = (older.turn_nonce().unwrap(), newer.turn_nonce().unwrap());
    let found = |nonce: &str, state: &Arc<HerdrInterruptState>| {
        herdr_turn(&logical, nonce).is_some_and(|found| Arc::ptr_eq(&found, state))
    };
    assert!(
        found(a_nonce, &a),
        "a newer turn never evicts a live older one"
    );
    assert!(found(b_nonce, &b));
    assert!(herdr_turn("AgentDesk-codex-turn-index-other", a_nonce).is_none());
    let b_nonce = b_nonce.to_owned();
    drop((b, newer));
    assert!(
        herdr_turn(&logical, &b_nonce).is_none(),
        "a dropped turn is gone"
    );
    assert!(
        found(a_nonce, &a),
        "dropping the newer turn keeps the older one"
    );
    let a_nonce = a_nonce.to_owned();
    drop((a, older));
    assert!(herdr_turn(&logical, &a_nonce).is_none());
}

/// A nonce two live turns share names neither; the survivor is found once the other drops.
#[test]
fn a_nonce_two_live_herdr_turns_share_names_neither() {
    let logical = format!("AgentDesk-codex-turn-collision-{}", std::process::id());
    let nonce = format!("collision-{}", std::process::id());
    let owner = herdr_owner("1", &logical);
    let first = CancelToken::from_persisted_turn_nonce(Some(nonce.clone()));
    let a = first.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
    let second = CancelToken::from_persisted_turn_nonce(Some(nonce.clone()));
    let b = second.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
    assert!(
        herdr_turn(&logical, &nonce).is_none(),
        "a shared nonce is unknown"
    );
    drop((b, second));
    let survivor = herdr_turn(&logical, &nonce);
    assert!(survivor.is_some_and(|found| Arc::ptr_eq(&found, &a)));
}

/// A token's Herdr state is taken once, by its own owner: another owner's prepare gets none
/// and changes nothing.
#[test]
fn a_herdr_stop_state_is_prepared_again_only_by_its_owner() {
    let logical = format!("AgentDesk-codex-prepare-owner-{}", std::process::id());
    let token = CancelToken::new();
    let state = token.try_prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("1", &logical));
    let state = state.expect("a fresh token takes its state");
    let again = token.try_prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("1", &logical));
    assert!(again.is_some_and(|again| Arc::ptr_eq(&again, &state)));
    let other = herdr_owner("2", "AgentDesk-codex-prepare-owner-other");
    assert!(
        token
            .try_prepare_herdr_interrupt(ProviderKind::Codex, &other)
            .is_none()
    );
    assert_eq!(token.tmux_session_name().as_deref(), Some(logical.as_str()));
}

/// Under the Herdr slot a stop before a prepare publishes and the prepare takes nothing; after
/// one it leaves the token to its intent path; without settlement it is the existing cancel.
#[cfg(unix)]
#[test]
fn a_stop_and_a_herdr_prepare_are_ordered_by_the_slot() {
    let owner = herdr_owner("1", "AgentDesk-codex-stop-order");
    let reason = || "mailbox_cancel_active_turn".to_string();
    let first = Arc::new(CancelToken::new());
    let decided = StopCancel::decide(Some(first.clone()), reason());
    assert!(matches!(decided, StopCancel::Published(_)));
    assert!(first.cancelled.load(Ordering::SeqCst));
    assert!(
        first
            .try_prepare_herdr_interrupt(ProviderKind::Codex, &owner)
            .is_none()
    );
    assert!(first.herdr_interrupt_state().is_none());
    assert!(
        first.tmux_session_name().is_none(),
        "a refused prepare binds nothing"
    );

    let prepared = Arc::new(CancelToken::new());
    prepared.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
    let decided = StopCancel::decide(Some(prepared.clone()), reason());
    assert!(matches!(decided, StopCancel::Herdr(_)));
    assert!(!prepared.cancelled.load(Ordering::SeqCst));
    assert!(prepared.cancel_source().is_none());

    prepared.publish_cancel("earlier");
    let decided = StopCancel::decide(Some(prepared.clone()), reason());
    assert!(matches!(decided, StopCancel::AlreadyStopping(_)));
    assert_eq!(prepared.cancel_source().as_deref(), Some("earlier"));
    assert!(matches!(
        StopCancel::decide(None, reason()),
        StopCancel::NotCurrent
    ));

    let unsettled = Arc::new(CancelToken::new());
    unsettled.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let decided = StopCancel::decide(Some(unsettled.clone()), reason());
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    assert!(
        matches!(decided, StopCancel::Published(_)),
        "without settlement it cancels"
    );
    assert!(unsettled.cancelled.load(Ordering::SeqCst));
}

/// With settlement a prepare racing into a stop's judgement-to-publish window waits on the
/// slot, then sees the cancel and installs nothing; without settlement the stop takes no slot.
#[cfg(unix)]
#[test]
fn p2b_decide_holds_slot_lock_until_publish() {
    use std::sync::mpsc;
    use std::time::Duration;

    struct HookReset;
    impl Drop for HookReset {
        fn drop(&mut self) {
            DECIDE_HOOK.with(|hook| hook.borrow_mut().take());
            HERDR_SETTLEMENT_OVERRIDE.set(true);
        }
    }
    let _reset = HookReset;
    let owner = herdr_owner("1", "AgentDesk-codex-stop-lock-window");
    let reason = || "mailbox_cancel_active_turn".to_string();

    let token = Arc::new(CancelToken::new());
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    let done_rx = std::rc::Rc::new(done_rx);
    let hook_runs = Arc::new(AtomicUsize::new(0));
    {
        let token = token.clone();
        let owner = owner.clone();
        let hook_runs = hook_runs.clone();
        let done_rx = done_rx.clone();
        DECIDE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                hook_runs.fetch_add(1, Ordering::SeqCst);
                let (started_tx, started_rx) = mpsc::channel::<()>();
                let racer = token.clone();
                let owner = owner.clone();
                let done_tx = done_tx.clone();
                std::thread::spawn(move || {
                    started_tx.send(()).unwrap();
                    let prepared = racer.try_prepare_herdr_interrupt(ProviderKind::Codex, &owner);
                    done_tx.send(prepared.is_some()).unwrap();
                });
                started_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the racing prepare starts");
                let raced = done_rx.recv_timeout(Duration::from_millis(500));
                assert!(
                    matches!(raced, Err(mpsc::RecvTimeoutError::Timeout)),
                    "the racing prepare is not past the slot while the stop holds it: {raced:?}"
                );
                assert!(
                    token.herdr_interrupt.try_lock().is_err(),
                    "the stop holds the slot between its judgement and its publish"
                );
                assert!(!token.cancelled.load(Ordering::SeqCst));
            }));
        });
    }
    let decided = StopCancel::decide(Some(token.clone()), reason());
    assert_eq!(
        hook_runs.load(Ordering::SeqCst),
        1,
        "the window was observed"
    );
    assert!(matches!(decided, StopCancel::Published(_)));
    let installed = done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the racing prepare finishes once the slot is released");
    assert!(
        !installed,
        "the prepare sees the published cancel and takes nothing"
    );
    assert!(token.cancelled.load(Ordering::SeqCst));
    assert!(token.herdr_interrupt_state().is_none());
    assert!(
        token.tmux_session_name().is_none(),
        "a refused prepare binds nothing"
    );

    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let unsettled = Arc::new(CancelToken::new());
    let unsettled_runs = Arc::new(AtomicUsize::new(0));
    {
        let unsettled = unsettled.clone();
        let unsettled_runs = unsettled_runs.clone();
        DECIDE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                unsettled_runs.fetch_add(1, Ordering::SeqCst);
                assert!(
                    unsettled.herdr_interrupt.try_lock().is_ok(),
                    "without settlement the stop takes no slot"
                );
            }));
        });
    }
    let decided = StopCancel::decide(Some(unsettled.clone()), reason());
    assert_eq!(
        unsettled_runs.load(Ordering::SeqCst),
        1,
        "the window was observed"
    );
    assert!(matches!(decided, StopCancel::Published(_)));
    assert!(unsettled.cancelled.load(Ordering::SeqCst));
}

/// With settlement the stop still holds the slot after its publish: the published cancel and
/// the held slot are seen together, with no racing thread.
#[cfg(unix)]
#[test]
fn p2b_decide_publishes_before_releasing_the_slot() {
    struct HookReset;
    impl Drop for HookReset {
        fn drop(&mut self) {
            PUBLISH_HOOK.with(|hook| hook.borrow_mut().take());
            HERDR_SETTLEMENT_OVERRIDE.set(true);
        }
    }
    let _reset = HookReset;
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    let token = Arc::new(CancelToken::new());
    let hook_runs = Arc::new(AtomicUsize::new(0));
    {
        let token = token.clone();
        let hook_runs = hook_runs.clone();
        PUBLISH_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                hook_runs.fetch_add(1, Ordering::SeqCst);
                assert!(
                    token.cancelled.load(Ordering::SeqCst),
                    "the cancel is published"
                );
                assert!(
                    token.herdr_interrupt.try_lock().is_err(),
                    "the stop holds the slot through its publish"
                );
            }));
        });
    }
    let decided = StopCancel::decide(
        Some(token.clone()),
        "mailbox_cancel_active_turn".to_string(),
    );
    assert_eq!(
        hook_runs.load(Ordering::SeqCst),
        1,
        "the publish was observed"
    );
    assert!(matches!(decided, StopCancel::Published(_)));
    assert!(
        token.herdr_interrupt.try_lock().is_ok(),
        "the slot is released"
    );
}
