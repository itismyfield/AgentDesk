use super::super::*;
use super::*;
use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState};

const UPDATE_PANE: &str = "  Update available · 0.160.0 → 9.9.9\n\
  Release notes: https://github.com/openai/codex/releases/latest\n\
› 1. Update now (runs `npm install -g @openai/codex`)\n\
  2. Skip\n\
  3. Skip until next version\n\
  enter continue · esc skip";
const STATUS: &str = "gpt-6.1-sol · Fast off · Context 100% left";

#[test]
fn startup_update_modal_blocks_readiness_steering_and_submit_without_keys() {
    let without_composer = UPDATE_PANE.to_string();
    let above_composer = format!("{UPDATE_PANE}\n›\n{STATUS}");
    for pane in [&without_composer, &above_composer] {
        let guard = SpyGuard::install(SpyState {
            captures: vec![Some(pane.clone()); 4].into(),
            ..SpyState::default()
        });
        let snapshot = prompt_readiness_snapshot("startup-update");
        assert!(codex_snapshot_indicates_interactive_modal(&snapshot));
        assert!(!snapshot_allows_warm_followup_submit(&snapshot));
        assert!(steering_snapshot_decision(&snapshot).is_err());
        assert!(inject_steering_prompt("startup-update", "follow-up").is_err());
        let outcome = submit_codex_followup_prompt("startup-update", "follow-up", None);
        assert!(matches!(
            outcome,
            CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
        ));
        assert_eq!(guard.0.borrow().sends, 0, "{:?}", guard.calls());
    }
    for header in [
        "Update available · 0.160.0 → 9.9.9",
        "Update now (runs `npm install -g @openai/codex`)",
        "Skip until next version",
    ] {
        let pane = format!("{header}\n›\n{STATUS}");
        assert!(pane_has_codex_interactive_modal_in_pane(&pane));
        assert!(!pane_looks_ready_for_codex_prompt(&pane));
        assert!(!pane_looks_ready_for_codex_prompt_with_ansi(&pane));
    }
    let mut stale_composer = PromptReadinessSnapshot {
        composer_marker_detected: true,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: UPDATE_PANE.to_string(),
    };
    assert_eq!(
        steering_snapshot_decision(&stale_composer),
        Err("interactive modal")
    );
    assert!(!snapshot_allows_warm_followup_submit(&stale_composer));
    stale_composer.pane_tail.clear();
    assert!(snapshot_allows_warm_followup_submit(&stale_composer));
}

#[test]
fn startup_update_unknown_screen_without_composer_gets_no_keys() {
    let guard = SpyGuard::install(SpyState {
        captures: vec![
            Some("Unknown startup screen\n› 1. Continue\nenter continue".to_string());
            3
        ]
        .into(),
        ..SpyState::default()
    });
    assert!(inject_steering_prompt("startup-unknown", "follow-up").is_err());
    assert!(matches!(
        submit_codex_followup_prompt("startup-unknown", "follow-up", None),
        CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
    ));
    assert_eq!(guard.0.borrow().sends, 0, "{:?}", guard.calls());
}

#[test]
fn steering_snapshot_requires_composer_and_rejects_modal() {
    let missing_composer = submit_snapshot(true, true, false, false);
    assert_eq!(
        steering_snapshot_decision(&missing_composer),
        Err("composer not present")
    );

    let mut modal = submit_snapshot(true, true, true, false);
    modal.pane_tail = "Approval required: allow command?".to_string();
    assert_eq!(steering_snapshot_decision(&modal), Err("interactive modal"));
}

#[test]
fn final_submit_gate_requires_a_live_empty_canonical_snapshot() {
    let ready = submit_snapshot(true, true, true, false);
    assert!(snapshot_allows_warm_followup_submit(&ready));

    for mutate in [
        |snapshot: &mut PromptReadinessSnapshot| snapshot.tmux_pane_alive = false,
        |snapshot: &mut PromptReadinessSnapshot| snapshot.capture_available = false,
        |snapshot: &mut PromptReadinessSnapshot| snapshot.composer_marker_detected = false,
        |snapshot: &mut PromptReadinessSnapshot| snapshot.prompt_draft_detected = true,
    ] {
        let mut rejected = ready.clone();
        mutate(&mut rejected);
        assert!(
            !snapshot_allows_warm_followup_submit(&rejected),
            "final submit must reject every mutated readiness guard"
        );
    }
}
