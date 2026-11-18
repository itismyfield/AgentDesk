use super::*;
use crate::services::tui_turn_state::idle_compaction_tests::{end_turn, tail};

#[cfg(unix)]
#[path = "idle_compaction_safety_tests.rs"]
mod safety_tests;

#[cfg(unix)]
fn send_with_budget(r: &Rig) -> Result<(), super::super::super::guarded_outcome::SubmissionError> {
    let previous = super::super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS
        .with(|budget| budget.replace(Some(Duration::ZERO)));
    let result = r.send();
    super::super::super::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|budget| budget.set(previous));
    result
}

#[cfg(unix)]
fn assert_evidence_pending(
    result: &Result<(), super::super::super::guarded_outcome::SubmissionError>,
    tmux: &super::super::super::input::submission::tests::ScriptedTmux,
) {
    use super::super::super::guarded_outcome::SubmissionError;
    let Err(SubmissionError::Held(hold)) = result else {
        panic!("expected the actual preparation hold: {result:?}");
    };
    assert_eq!(hold.reason, "idle_evidence_pending");
    assert!(hold.detail.starts_with("timeout waiting for claude tui"));
    assert!(
        tmux.sends().is_empty(),
        "a preparation hold sends no payload or key"
    );
    assert!(tmux.body().is_empty());
    eprintln!("preparation hold reason={} payloads=0 keys=0", hold.reason);
}

#[cfg(unix)]
#[test]
fn idle_compaction_actual_preparation_submits_one_original() {
    use super::super::super::input::submission::tests::{ScriptedTmux, view};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    r.append(end_turn());
    for record in tail() {
        r.append(record);
    }
    let result = send_with_budget(&r);
    if result.is_err() {
        assert_evidence_pending(&result, &tmux);
    }
    assert_eq!(result, Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    assert_eq!(tmux.body(), "one original");
}

#[test]
fn idle_compaction_proves_readiness_without_a_later_stop_hook() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.append(end_turn());
    for record in tail() {
        r.append(record);
    }
    let proof = observe(&r.session, Some(&r.path)).expect("completed reply stays ready");
    let reservation = reserve(proof).expect("same canonical tail can be reserved");
    assert!(reservation.current());
    reservation.not_sent();
}

#[test]
fn idle_compaction_readiness_refuses_activity_after_the_completed_reply() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for active in [
        json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"working"}]}}),
        json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"tool","name":"Read","input":{}}]}}),
        json!({"type":"progress","data":{"type":"hook_progress"}}),
        json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}),
        json!({"type":"user","isCompactSummary":true,"message":{"content":"summary"}}),
    ] {
        let r = Rig::new();
        r.append(end_turn());
        r.append(active.clone());
        for record in tail() {
            r.append(record);
        }
        assert!(!r.ready(), "{active}");
    }
}
