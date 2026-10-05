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

/// What a dcserver restart forgets, then its rehydrate pass on this node's endpoint.
fn restart_and_reconnect(fx: &Fixture) -> ReconnectCounts {
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    crate::services::tui_prompt_dedupe::binding_events::forget_channel_for_tests(CHANNEL);
    let _runtime = fx.rt.enter();
    let _registry = fx.rig.registry_on_this_thread();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[]);
    fx.rig.show_panes(&[PANE]);
    reconnect_restarted_herdr_panes(Some(&fx.pool));
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

// A restart after a clear whose own Pending still waits on its new session admits the execution
// without restoring a source; the next turn takes one paste and Enter on the cleared session and
// its transcript resolves the Pending.
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
