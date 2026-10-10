//! A dcserver restart between two turns of a launched Herdr pane: the restart pass reconnects the
//! execution only through a matched read of its pane, and the next turn takes its prompt once.

use super::*;
use crate::services::discord::recovery_engine::herdr_reader::{
    ReconnectCounts, reconnect_counts, reconnect_restarted_herdr_panes,
};

/// A first turn launches the pane and binds it, as T-E1 does; returns the launched nonce and
/// session.
fn launch(fx: &Fixture, launcher: &Arc<Launcher>) -> (String, Started) {
    let started = Mutex::new(None);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(launcher), || {
        if let Some(launched) = fx.start_provider(launcher, true) {
            fx.answer(&launched);
            *started.lock().unwrap() = Some(launched);
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    (nonce, started.into_inner().unwrap().unwrap())
}

thread_local! {
    /// Whether each row the latest pass read had its source bound, as adoption sees it.
    static BOUND: std::cell::RefCell<Result<Vec<bool>, String>> = const { std::cell::RefCell::new(Ok(Vec::new())) };
}

fn bound() -> Result<Vec<bool>, String> {
    BOUND.with_borrow(Clone::clone)
}

/// What a dcserver restart forgets, then its rehydrate pass on this node's endpoint.
fn restart_and_reconnect(fx: &Fixture) -> ReconnectCounts {
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    crate::services::tui_prompt_dedupe::binding_events::forget_channel_for_tests(CHANNEL);
    let _runtime = fx.rt.enter();
    let _registry = fx.rig.registry_on_this_thread();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[]);
    fx.rig.show_panes(&[PANE]);
    let pass = reconnect_restarted_herdr_panes(
        Some(&fx.pool),
        &crate::services::provider::ProviderKind::Claude,
    );
    let channel = Some(CHANNEL);
    BOUND.set(pass.map(|rows| {
        rows.iter()
            .map(|r| r.bound && r.channel == channel)
            .collect()
    }));
    reconnect_counts()
}

/// The second turn's provider answers its prompt once paste and Enter arrived.
fn second_turn(fx: &Fixture, launcher: &Arc<Launcher>, started: &Started) -> Result<(), String> {
    let row = fx.record();
    let (result, _) = fx.turn(&row, &fx.ports(launcher), || {
        if wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 4) {
            let session = &started.session;
            let user = json!({"type": "user", "sessionId": session,
                "message": {"role": "user", "content": "질문"}});
            let answer = json!({"type": "assistant", "sessionId": session, "message": {
                "role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "답"}]}});
            let done = json!({"type": "system", "subtype": "turn_duration", "sessionId": session});
            append(&started.path, &[user, answer, done]);
        }
    });
    result
}

// T-R1: the restart forgets the attached source; the pass reads the pane as the Bound execution,
// restores the logged source, and the next turn takes exactly one paste and Enter.
#[test]
fn t_r1_a_restart_reconnects_a_matched_bound_pane_and_its_next_turn_prompts_once_pg() {
    let fx = Fixture::new("reconnect", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = launch(&fx, &launcher);
    let counts = restart_and_reconnect(&fx);
    let reconnected = ReconnectCounts {
        channels: 1,
        published: 1,
        ..ReconnectCounts::default()
    };
    assert_eq!(counts, reconnected);
    assert_eq!(bound(), Ok(vec![true]), "the restored source is bound");
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(
        binding.and_then(|binding| binding.session_id),
        Some(started.session.clone())
    );
    assert_eq!(second_turn(&fx, &launcher, &started), Ok(()));
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), 4, "{sends:?}");
    assert_eq!(sends[2..], prompt_sends()[..]);
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        1,
        "nothing relaunched"
    );
}

// T-R2: the pane's root shell was started again after the launch recorded it: the pass restores
// nothing, and the next turn writes nothing and launches nothing.
#[test]
fn t_r2_a_replaced_root_shell_reconnects_nothing_and_takes_no_input_pg() {
    let fx = Fixture::new("root-replaced", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = launch(&fx, &launcher);
    fx.rig.restart_shell(&context_of(&nonce));
    let counts = restart_and_reconnect(&fx);
    let withheld = ReconnectCounts {
        channels: 1,
        withheld: 1,
        ..ReconnectCounts::default()
    };
    assert_eq!(counts, withheld);
    assert_eq!(bound(), Ok(vec![false]));
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(binding, None);
    assert!(second_turn(&fx, &launcher, &started).is_err());
    assert_eq!(
        fx.rig.sends().len(),
        2,
        "no input after the launch's own prompt"
    );
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        1,
        "nothing relaunched"
    );
}

/// The launched execution's own SessionStart(clear) Pending of `cleared`, as its hook logs it
/// before the cleared session's transcript exists; returns where that transcript goes.
fn clear_pending(fx: &Fixture, cleared: &str) -> std::path::PathBuf {
    use crate::services::tui_prompt_dedupe::binding_events::{
        self, BindingCause, CauseSource, HookSignal, Proposal,
    };
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        cleared,
        None,
    )
    .unwrap();
    let text = path.display().to_string();
    let payload = json!({"source": "clear", "session_id": cleared, "transcript_path": text});
    let hook = HookSignal::from_payload("session_start", &payload);
    let proposal = Proposal {
        channel_id: CHANNEL,
        provider: "claude",
        tmux_session: fx.logical(),
        session_id: Some(cleared),
        path: &text,
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Clear),
        hook: Some(&hook),
    };
    crate::services::tmux_common::with_tmux_source_authority(fx.logical(), |_| {
        binding_events::record_pending(&proposal).unwrap();
    });
    path
}

/// The next turn after the clear: its provider writes the cleared session's transcript once paste
/// and Enter arrived.
fn cleared_turn(
    fx: &Fixture,
    launcher: &Arc<Launcher>,
    cleared: &str,
    path: &Path,
) -> Result<(), String> {
    let row = fx.record();
    let (result, _) = fx.turn(&row, &fx.ports(launcher), || {
        if wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 4) {
            let user = json!({"type": "user", "sessionId": cleared,
                "message": {"role": "user", "content": "질문"}});
            let answer = json!({"type": "assistant", "sessionId": cleared, "message": {
                "role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "답"}]}});
            let done = json!({"type": "system", "subtype": "turn_duration", "sessionId": cleared});
            append(path, &[user, answer, done]);
        }
    });
    result
}

/// The pane's latest logged record resolves the clear's Pending to the cleared session.
fn resolved_to(cleared: &str, logical: &str) -> bool {
    use crate::services::tui_prompt_dedupe::binding_events::{BindingTarget, binding_events_since};
    let events = binding_events_since(CHANNEL, 0).unwrap();
    let pane: Vec<_> = events
        .iter()
        .filter(|e| e.tmux_session == logical)
        .collect();
    let pending = pane.iter().rev().find_map(|e| match &e.new {
        BindingTarget::Pending {
            payload_session_id, ..
        } if payload_session_id == cleared => Some(e.seq),
        _ => None,
    });
    matches!(pane.last().map(|e| &e.new), Some(BindingTarget::Resolved { pending_seq, source })
        if Some(*pending_seq) == pending && source.session_id == cleared)
}

// A restart while a clear's own Pending waits admits the execution without a restored source;
// the next turn prompts the cleared session once and its transcript resolves the Pending.
#[test]
fn a_restart_after_a_pending_clear_prompts_the_cleared_session_once_pg() {
    let fx = Fixture::new("clear-restart", None);
    let launcher = Arc::new(Launcher::default());
    let (_, _) = launch(&fx, &launcher);
    let cleared = uuid();
    let path = clear_pending(&fx, &cleared);
    let counts = restart_and_reconnect(&fx);
    let admitted = ReconnectCounts {
        channels: 1,
        published: 1,
        ..ReconnectCounts::default()
    };
    assert_eq!(counts, admitted);
    // Published for health, yet AwaitingClear restores no source: never adoption's bound source.
    assert_eq!(
        bound(),
        Ok(vec![false]),
        "herdr_awaiting_clear_is_not_bound_source_success"
    );
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(binding, None, "no source is restored for a waiting clear");
    let result = cleared_turn(&fx, &launcher, &cleared, &path);
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), 4, "{sends:?} {result:?}");
    assert_eq!(sends[2..], prompt_sends()[..]);
    assert!(
        resolved_to(&cleared, fx.logical()),
        "the Pending resolved: {result:?}"
    );
    assert_eq!(result, Ok(()));
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(binding.and_then(|b| b.session_id), Some(cleared));
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        1,
        "nothing relaunched"
    );
}

// Without a restart the old session's source is still attached: the next turn after the clear
// still takes its prompt on the cleared session, once.
#[test]
fn the_turn_after_a_pending_clear_prompts_the_cleared_session_once_pg() {
    let fx = Fixture::new("clear-live", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = launch(&fx, &launcher);
    let cleared = uuid();
    let path = clear_pending(&fx, &cleared);
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(binding.and_then(|b| b.session_id), Some(started.session));
    let result = cleared_turn(&fx, &launcher, &cleared, &path);
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), 4, "{sends:?} {result:?}");
    assert_eq!(sends[2..], prompt_sends()[..]);
    assert!(
        resolved_to(&cleared, fx.logical()),
        "the Pending resolved: {result:?}"
    );
    assert_eq!(result, Ok(()));
}

/// Two clears with no prompt between them; returns the second's session and transcript.
fn two_clears(fx: &Fixture) -> (String, std::path::PathBuf) {
    clear_pending(fx, &uuid());
    let latest = uuid();
    let path = clear_pending(fx, &latest);
    (latest, path)
}

/// The next turn prompts `cleared` once, reads its transcript and resolves its Pending.
fn assert_prompts_once(fx: &Fixture, launcher: &Arc<Launcher>, cleared: &str, path: &Path) {
    let result = cleared_turn(fx, launcher, cleared, path);
    let sends = fx.rig.sends();
    assert_eq!(sends.len(), 4, "{sends:?} {result:?}");
    assert_eq!(sends[2..], prompt_sends()[..], "one paste and Enter");
    assert!(
        resolved_to(cleared, fx.logical()),
        "the latest Pending resolved: {result:?}"
    );
    assert_eq!(result, Ok(()));
}

/// Rewrites the pane's logged record `back` lines from the end, as a hand other than the writer.
fn forge(back: usize, edit: impl Fn(&mut Value)) {
    use crate::services::tui_prompt_dedupe::binding_events::{self, BINDING_EVENTS_DIR};
    let log = (binding_events::test_root().unwrap())
        .join(BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    let text = std::fs::read_to_string(&log).unwrap();
    let mut lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let at = lines.len() - 1 - back;
    edit(&mut lines[at]);
    let lines: Vec<String> = lines.iter().map(Value::to_string).collect();
    std::fs::write(&log, lines.join("\n") + "\n").unwrap();
    binding_events::forget_channel_for_tests(CHANNEL);
}

// Two clears with no prompt between them: the next turn prompts the latest cleared session once,
// alone, and reads that session's transcript.
#[test]
fn the_turn_after_two_pending_clears_prompts_the_latest_cleared_session_once_pg() {
    let fx = Fixture::new("clear-twice-live", None);
    let launcher = Arc::new(Launcher::default());
    let _ = launch(&fx, &launcher);
    let (cleared, path) = two_clears(&fx);
    assert_prompts_once(&fx, &launcher, &cleared, &path);
}

// A restart while the second of two clears waits admits the execution without a restored source;
// the next turn prompts the latest cleared session once.
#[test]
fn a_restart_after_two_pending_clears_prompts_the_latest_cleared_session_once_pg() {
    let fx = Fixture::new("clear-twice-restart", None);
    let launcher = Arc::new(Launcher::default());
    let _ = launch(&fx, &launcher);
    let (cleared, path) = two_clears(&fx);
    let counts = restart_and_reconnect(&fx);
    let admitted = ReconnectCounts {
        channels: 1,
        published: 1,
        ..ReconnectCounts::default()
    };
    assert_eq!(counts, admitted);
    // Published for health, yet AwaitingClear restores no source: never adoption's bound source.
    assert_eq!(
        bound(),
        Ok(vec![false]),
        "herdr_awaiting_clear_is_not_bound_source_success"
    );
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(binding, None, "no source is restored for a waiting clear");
    assert_prompts_once(&fx, &launcher, &cleared, &path);
}

/// Two clears, then `edit` on the record `back` lines from the end: a restart restores and admits
/// nothing, and the next turn takes no prompt.
fn assert_forged_chain_refused(tag: &str, back: usize, edit: impl Fn(&mut Value)) {
    let fx = Fixture::new(tag, None);
    let launcher = Arc::new(Launcher::default());
    let _ = launch(&fx, &launcher);
    let (cleared, path) = two_clears(&fx);
    forge(back, edit);
    let counts = restart_and_reconnect(&fx);
    let withheld = ReconnectCounts {
        channels: 1,
        withheld: 1,
        ..ReconnectCounts::default()
    };
    assert_eq!(counts, withheld);
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(binding, None);
    let result = cleared_turn(&fx, &launcher, &cleared, &path);
    assert!(result.is_err(), "{result:?}");
    assert_eq!(fx.rig.sends().len(), 2, "no prompt after the launch's");
    assert!(!resolved_to(&cleared, fx.logical()));
}

// Another execution's Pending inside a clear chain breaks it: it is not this execution's clear.
#[test]
fn a_clear_chain_through_another_executions_pending_is_not_restored_pg() {
    assert_forged_chain_refused("chain-foreign", 1, |line| {
        line["execution_nonce"] = json!("f".repeat(32))
    });
}

// A clear Pending taken from another source than the one its execution logged is not its clear.
#[test]
fn a_clear_pending_taken_from_another_source_is_not_restored_pg() {
    assert_forged_chain_refused("chain-old", 0, |line| {
        line["old"]["session_id"] = json!("elsewhere")
    });
}

// A clear Pending whose hook named another transcript than the cleared session's own is refused
// before any prompt, so the turn never waits on a file nothing writes.
#[test]
fn a_clear_pending_naming_another_transcript_takes_no_prompt_pg() {
    let fx = Fixture::new("clear-elsewhere", None);
    let launcher = Arc::new(Launcher::default());
    let _ = launch(&fx, &launcher);
    let cleared = uuid();
    let path = clear_pending(&fx, &cleared);
    forge(0, |line| {
        line["new"]["pending"]["payload_transcript_path"] = json!("/tmp/elsewhere.jsonl")
    });
    let result = cleared_turn(&fx, &launcher, &cleared, &path);
    let sends = fx.rig.sends().len();
    assert_eq!(sends, 2, "no prompt after the launch's: {result:?}");
    let error = result.unwrap_err();
    assert!(error.contains("names another transcript"), "{error}");
}
