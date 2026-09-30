#![cfg(unix)]

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::claude_tui::hosting::{
    ClaudeTuiWarmFollowupOutcome, try_claude_tui_warm_followup,
};
use crate::services::claude_tui::input::{
    is_prompt_ready_timeout_error, prompt_readiness_snapshot_from_capture,
};
use crate::services::session_host::HostKind;

const DRAFT: &str = "\u{276f} 남은 초안 한글";
const BAKED_DRAFT: &str = "\u{273b} Baked for 3m 2s\n\u{276f} 남은 초안 한글";
const EMPTY: &str = "Claude Code v2.1.141\n\n\u{276f} \nstatus";
const PROMPT: &str = "다음 입력";
const SESSION_ID: &str = "0b7c4f0e-5d0b-4e57-9d55-3f3a3c1f7a10";

/// Admits every mutation except the listed admissions, counted from 0.
struct RefuseAt(Cell<usize>, &'static [usize]);

impl MutationGate for RefuseAt {
    fn admit(&self, _session: &str) -> Result<(), InputRefusal> {
        let admission = self.0.replace(self.0.get() + 1);
        match self.1.contains(&admission) {
            true => Err(InputRefusal::IdentityMismatch),
            false => Ok(()),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Ended {
    Terminal(Result<(), String>),
    Recreate { session_id: String, resume: bool },
}

struct Scenario {
    target: Option<InputTarget>,
    refuse: &'static [usize],
    idle_transcript: bool,
    captures: Vec<&'static str>,
    fail_send: Option<(usize, Result<std::process::Output, String>)>,
    cancel_on: Option<(&'static str, usize)>,
}

impl Scenario {
    fn tmux(idle_transcript: bool, captures: Vec<&'static str>) -> Self {
        Self {
            target: None,
            refuse: &[],
            idle_transcript,
            captures,
            fail_send: None,
            cancel_on: None,
        }
    }
}

/// Runs the real warm follow-up entry on its own thread with the spy transport;
/// a composer lock taken twice deadlocks and fails the wait below.
fn follow_up(name: &str, scenario: Scenario) -> (Ended, Vec<String>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join(format!("{SESSION_ID}.jsonl"));
    let last = match scenario.idle_transcript {
        true => "result",
        false => "permission-mode",
    };
    std::fs::write(&transcript, format!("{{\"type\":\"{last}\"}}\n")).unwrap();
    let name = name.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let token = Arc::new(CancelToken::new());
        let guard = SpyGuard::install(SpyState {
            captures: scenario
                .captures
                .iter()
                .map(|c| Some(c.to_string()))
                .collect(),
            fail_send: scenario.fail_send,
            cancel_on: scenario
                .cancel_on
                .map(|(prefix, nth)| (prefix, nth, token.clone())),
            ..SpyState::default()
        });
        let gate = RefuseAt(Cell::new(0), scenario.refuse);
        let host = FollowupHost {
            target: scenario
                .target
                .unwrap_or_else(|| InputTarget::legacy_tmux(&name)),
            gate: &gate,
        };
        let (sender, streamed) = std::sync::mpsc::channel();
        let outcome = try_claude_tui_warm_followup(
            SESSION_ID.to_string(),
            transcript.clone(),
            transcript.display().to_string(),
            true,
            dir.path(),
            PROMPT,
            sender,
            Some(token),
            &host,
            None,
        );
        let ended = match outcome {
            ClaudeTuiWarmFollowupOutcome::Terminal(result) => Ended::Terminal(result),
            ClaudeTuiWarmFollowupOutcome::Recreate(state) => Ended::Recreate {
                session_id: state.resolved_session_id,
                resume: state.resume,
            },
        };
        let _ = tx.send((ended, guard.calls(), streamed.try_iter().count()));
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("warm follow-up never finished (composer lock taken twice?)")
}

fn keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| format!("keys:{name}")).collect()
}

fn read() -> Vec<String> {
    vec!["capture".to_string(), "alive".to_string()]
}

/// Keys and reads of one legacy clear attempt on a draft that stays.
fn attempt(clear: DraftClear) -> Vec<String> {
    let tail = prompt_readiness_snapshot_from_capture(Some(DRAFT), true).pane_tail;
    let budget = claude_prompt_draft_backspace_budget_from_tail(&tail).unwrap();
    let groups: &[&str] = match clear {
        DraftClear::Strong => &["C-e+C-u", "Escape", "C-e+C-u"],
        DraftClear::Gentle => &["C-e+C-u"],
    };
    let mut calls = Vec::new();
    for group in groups {
        calls.extend(keys(&[group]));
        calls.extend(read());
    }
    calls.extend(keys(&["C-e", &vec!["BSpace"; budget].join("+")]));
    calls.extend(read());
    calls
}

fn stopped_without_side_effects(label: &str, ended: &Ended, calls: &[String], streamed: usize) {
    if let Ended::Terminal(Err(error)) = ended {
        assert!(!is_prompt_ready_timeout_error(error), "{label}: {error}");
    }
    assert!(
        matches!(ended, Ended::Terminal(_)),
        "{label}: no fresh session: {ended:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("retire:")),
        "{label}: {calls:?}"
    );
    assert_eq!(streamed, 0, "{label}: nothing published");
}

#[test]
fn tf2_hosts_without_input_stop_before_any_host_call() {
    for refusal in [
        InputRefusal::Unsupported(HostKind::Herdr),
        InputRefusal::Unsupported(HostKind::Process),
        InputRefusal::Unknown,
        InputRefusal::Conflict,
    ] {
        let mut scenario = Scenario::tmux(true, vec![DRAFT; 12]);
        scenario.target = Some(InputTarget::Refused(refusal));
        let (ended, calls, streamed) = follow_up("p6a2-refused", scenario);
        assert_eq!(
            ended,
            Ended::Terminal(Err(stopped_error(&HostInputOutcome::Refused(refusal))))
        );
        assert!(calls.is_empty(), "{refusal:?}: {calls:?}");
        stopped_without_side_effects(&format!("{refusal:?}"), &ended, &calls, streamed);
    }
}

#[test]
fn tf2_legacy_tmux_clear_then_submits_once() {
    let mut scenario = Scenario::tmux(true, vec![DRAFT, DRAFT, EMPTY, EMPTY, EMPTY, EMPTY]);
    scenario.cancel_on = Some(("literal:", 1));
    let (ended, calls, streamed) = follow_up("p6a2-cleared", scenario);
    let cleared = [read(), read(), keys(&["C-e+C-u"]), read()].concat();
    let (clear, submit) = calls.split_at(cleared.len().min(calls.len()));
    assert_eq!(clear, &cleared[..], "{calls:?}");
    let literal = format!("literal:{PROMPT}");
    assert_eq!(
        submit.iter().filter(|c| **c == literal).count(),
        1,
        "{calls:?}"
    );
    assert!(!submit.iter().any(|c| c.starts_with("keys:")), "{calls:?}");
    assert_eq!(ended, Ended::Terminal(Ok(())));
    stopped_without_side_effects("cancelled at submit", &ended, &calls, streamed);
}

#[test]
fn tf2_legacy_tmux_recreates_only_after_retiring_the_session() {
    let fresh = |ended: &Ended| match ended {
        Ended::Recreate { session_id, resume } => session_id != SESSION_ID && !resume,
        Ended::Terminal(_) => false,
    };
    let retire = |code: &str, reason: &str| vec![format!("retire:{code}:{reason}")];
    let persisted = "stranded claude tui prompt draft persisted after clear attempts";

    let strong = Scenario::tmux(true, vec![DRAFT; 10]);
    let (ended, calls, _) = follow_up("p6a2-strong", strong);
    let expected = [
        read(),
        read(),
        attempt(DraftClear::Strong),
        attempt(DraftClear::Strong),
        retire("stranded_prompt_draft_recreate", persisted),
    ]
    .concat();
    assert!(fresh(&ended), "{ended:?}");
    assert_eq!(calls, expected);

    // An unknown transcript recreates only when the pane shows a finished turn.
    let gentle = Scenario::tmux(false, vec![BAKED_DRAFT; 6]);
    let (ended, calls, _) = follow_up("p6a2-gentle", gentle);
    let expected = [
        read(),
        read(),
        attempt(DraftClear::Gentle),
        attempt(DraftClear::Gentle),
        retire("stranded_prompt_draft_recreate", persisted),
    ]
    .concat();
    assert!(fresh(&ended), "{ended:?}");
    assert_eq!(calls, expected);

    let mut ack_lost = Scenario::tmux(true, vec![DRAFT; 4]);
    ack_lost.fail_send = Some((0, Err("ack lost".to_string())));
    let (ended, calls, _) = follow_up("p6a2-ack-lost", ack_lost);
    let expected = [
        read(),
        read(),
        keys(&["C-e+C-u"]),
        retire(
            "stranded_prompt_draft_clear_failed_recreate",
            "claude tui stranded prompt draft clear failed: ack lost",
        ),
    ]
    .concat();
    assert!(fresh(&ended), "{ended:?}");
    assert_eq!(calls, expected);
}

#[test]
fn tf2_swap_stop_or_lingering_draft_never_recreates() {
    // The host is swapped after the first clear key: nothing more is sent.
    let mut swapped = Scenario::tmux(true, vec![DRAFT; 10]);
    swapped.refuse = &[1];
    let (ended, calls, streamed) = follow_up("p6a2-swap", swapped);
    let stopped = HostInputOutcome::Indeterminate { confirmed: 1 };
    assert_eq!(ended, Ended::Terminal(Err(stopped_error(&stopped))));
    assert_eq!(calls, [read(), read(), keys(&["C-e+C-u"]), read()].concat());
    stopped_without_side_effects("swap", &ended, &calls, streamed);

    // The host is swapped before the first clear key.
    let mut refused_first = Scenario::tmux(true, vec![DRAFT; 10]);
    refused_first.refuse = &[0];
    let (ended, calls, streamed) = follow_up("p6a2-refused-first", refused_first);
    let refused = HostInputOutcome::Refused(InputRefusal::IdentityMismatch);
    assert_eq!(ended, Ended::Terminal(Err(stopped_error(&refused))));
    assert_eq!(calls, [read(), read()].concat());
    stopped_without_side_effects("refused first", &ended, &calls, streamed);

    // ACK lost, then the host is swapped before the retire: no kill, no fresh ID.
    let mut retire_refused = Scenario::tmux(true, vec![DRAFT; 4]);
    retire_refused.fail_send = Some((0, Err("ack lost".to_string())));
    retire_refused.refuse = &[1];
    let (ended, calls, streamed) = follow_up("p6a2-retire-refused", retire_refused);
    assert_eq!(ended, Ended::Terminal(Err(stopped_error(&refused))));
    assert_eq!(calls, [read(), read(), keys(&["C-e+C-u"])].concat());
    stopped_without_side_effects("retire refused", &ended, &calls, streamed);

    // A stop lands during the clear.
    let mut stopped = Scenario::tmux(true, vec![DRAFT; 10]);
    stopped.cancel_on = Some(("keys:", 1));
    let (ended, calls, streamed) = follow_up("p6a2-stop", stopped);
    assert_eq!(ended, Ended::Terminal(Ok(())));
    assert_eq!(calls, [read(), read(), keys(&["C-e+C-u"])].concat());
    stopped_without_side_effects("stop", &ended, &calls, streamed);

    // An unknown transcript keeps the draft and defers to the busy wait; a stop ends it.
    let mut lingering = Scenario::tmux(false, vec![DRAFT; 10]);
    lingering.cancel_on = Some(("capture", 7));
    let (ended, calls, streamed) = follow_up("p6a2-lingering", lingering);
    let expected = [
        read(),
        read(),
        attempt(DraftClear::Gentle),
        attempt(DraftClear::Gentle),
        read(),
    ]
    .concat();
    assert_eq!(ended, Ended::Terminal(Ok(())));
    assert_eq!(calls, expected);
    stopped_without_side_effects("lingering", &ended, &calls, streamed);
}
