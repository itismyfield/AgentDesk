//! #8161 S0: a readiness wait that ends without a proof names its open cause; a source an older
//! launch lost settles as retained instead of holding for good.
use super::*;

/// An older launch whose finished turn this restarted daemon has not seen.
fn restarted() -> Rig {
    initialize();
    let r = Rig::launched(None, Utc::now() - chrono::Duration::days(10));
    r.assistant("old answer");
    r.terminal();
    daemon_restart(&r.session);
    r
}

fn idle() -> Option<String> {
    Some(crate::services::claude_tui::input::submission::tests::view(
        "", false,
    ))
}

#[test]
fn a_lost_source_settles_only_for_a_launch_older_than_this_run() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let older = restarted();
    tui_prompt_dedupe::clear_tmux_runtime_binding(&older.session);
    let mut seen = watched(&older.session, None);
    assert_eq!(
        seen.expired(idle),
        Expiry::Unproven(Unproven::Retained),
        "nothing in flight can publish an older launch's source"
    );
    initialize();
    let fresh = Rig::launched(None, Utc::now());
    tui_prompt_dedupe::clear_tmux_runtime_binding(&fresh.session);
    let mut seen = watched(&fresh.session, None);
    assert_eq!(
        seen.expired(idle),
        Expiry::Waits(WaitCause::NoSource),
        "a launch of this run may still publish its source"
    );
}

#[test]
fn an_open_fence_waits_and_names_it() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = restarted();
    let key = (r.path.clone(), Some(r.ctx.execution_nonce.clone()));
    ledger().unread_continuations.insert(key.clone());
    let mut seen = watched(&r.session, None);
    assert_eq!(seen.expired(idle), Expiry::Waits(WaitCause::Fenced));
    ledger().unread_continuations.remove(&key);
}

#[test]
fn our_pending_or_compact_control_waits_and_names_it() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = restarted();
    assert!(!r.ready(), "the restart floor stands");
    let source = Source::current(&r.session, None).unwrap();
    let len = fs::metadata(&r.path).unwrap().len();
    ledger().entries.get_mut(&source).unwrap().pending = Some((len, 1));
    let mut seen = watched(&r.session, None);
    assert_eq!(seen.expired(idle), Expiry::Waits(WaitCause::Pending));
    {
        let mut state = ledger();
        let entry = state.entries.get_mut(&source).unwrap();
        entry.pending = None;
        entry.compact_ticket = Some(1);
    }
    let mut seen = watched(&r.session, None);
    assert_eq!(seen.expired(idle), Expiry::Waits(WaitCause::Compact));
}

#[test]
fn a_proven_source_waits_as_observed_and_no_entry_opens_nothing() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = restarted();
    r.ready();
    r.assistant("internal turn after the restart");
    r.terminal();
    assert!(r.ready(), "a turn after the floor proves the source");
    let mut seen = watched(&r.session, None);
    assert_eq!(seen.expired(idle), Expiry::Waits(WaitCause::Observed));
    assert_eq!(watched::open_cause(None), None, "no entry is no open cause");
    let open = Entry {
        pending: Some((0, 1)),
        ..Entry::default()
    };
    assert_eq!(watched::open_cause(Some(&open)), Some(WaitCause::Pending));
    assert_eq!(watched::open_cause(Some(&Entry::default())), None);
}
