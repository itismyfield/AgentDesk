use super::{timed_out_panel_should_reconcile_to_done, turn_jsonl_deterministically_terminal};
use crate::services::agent_protocol::StatusEvent;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::placeholder_live_events::PlaceholderLiveEvents;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

fn warm_tui_state(tmux_session_name: &str, output_path: &str) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        Some("dm".to_string()),
        7,
        9001,
        9002,
        "run the skill".to_string(),
        Some("session-1".to_string()),
        Some(tmux_session_name.to_string()),
        Some(output_path.to_string()),
        None,
        50,
    );
    // The current turn advanced its own output past the turn-start anchor.
    state.turn_start_offset = Some(10);
    state.last_offset = 50;
    state.rebind_origin = false;
    state
}

fn write_terminal_jsonl() -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("temp jsonl");
    std::fs::write(
        file.path(),
        r#"{"type":"result","result":"done","session_id":"s"}"#,
    )
    .expect("write jsonl");
    file
}

#[test]
fn deterministic_terminal_true_for_advanced_turn_with_result_envelope() {
    let file = write_terminal_jsonl();
    let state = warm_tui_state("AgentDesk-claude-dm-1", &file.path().display().to_string());
    assert!(
        turn_jsonl_deterministically_terminal(&ProviderKind::Claude, &state),
        "an advanced warm-TUI turn whose JSONL holds a terminal result is deterministically terminal"
    );
}

#[test]
fn deterministic_terminal_false_when_turn_did_not_advance_output() {
    let file = write_terminal_jsonl();
    let mut state = warm_tui_state("AgentDesk-claude-dm-1", &file.path().display().to_string());
    // No advance past the anchor → a stale prior `result` must not unlock it.
    state.turn_start_offset = Some(50);
    state.last_offset = 50;
    assert!(!turn_jsonl_deterministically_terminal(
        &ProviderKind::Claude,
        &state
    ));

    // A missing anchor is treated as "not advanced" (conservative).
    state.turn_start_offset = None;
    assert!(!turn_jsonl_deterministically_terminal(
        &ProviderKind::Claude,
        &state
    ));
}

#[test]
fn deterministic_terminal_false_for_rebind_origin_or_missing_jsonl() {
    let file = write_terminal_jsonl();
    let mut state = warm_tui_state("AgentDesk-claude-dm-1", &file.path().display().to_string());
    // Operator-launched pane — never AgentDesk-gated, never reconciled.
    state.rebind_origin = true;
    assert!(!turn_jsonl_deterministically_terminal(
        &ProviderKind::Claude,
        &state
    ));

    // A turn whose JSONL is absent reads `Unknown` (not `Idle`) → not terminal.
    let mut missing = warm_tui_state("AgentDesk-claude-dm-1", "/nonexistent/turn.jsonl");
    missing.rebind_origin = false;
    assert!(!turn_jsonl_deterministically_terminal(
        &ProviderKind::Claude,
        &missing
    ));
}

// THE pin: a `TimedOut` completion gate (which suppresses `TurnCompleted`) leaves
// the live panel stuck at `진행 중`; once the turn is deterministically terminal
// the reconcile finalizes it to `응답 완료` — and the render stays byte-identical
// across no-op heartbeat ticks (#3477/#3812), before AND after the finalize.
#[test]
fn timed_out_panel_reconciles_to_done_and_preserves_heartbeat_stability() {
    let live = PlaceholderLiveEvents::default();
    let channel = serenity::ChannelId::new(42);
    let provider = ProviderKind::Claude;
    let started_at_unix = 1_700_000_000;

    // Mid/end of turn: a panel state exists and is non-terminal (`진행 중`).
    live.push_status_event(channel, StatusEvent::Heartbeat);
    assert!(
        live.status_panel_is_unfinished(channel),
        "a non-terminal panel is stuck at 진행 중 until something finalizes it"
    );

    // Heartbeat byte-stability BEFORE the reconcile: no state change → identical.
    let running_a = live.render_status_panel(channel, &provider, started_at_unix);
    let running_b = live.render_status_panel(channel, &provider, started_at_unix);
    assert_eq!(
        running_a, running_b,
        "render must be byte-identical across no-op heartbeat ticks (#3477/#3812)"
    );

    // The reconcile decision: fire ONLY when the turn is deterministically
    // terminal — never while the pane could still stream (#2161 guard), and never
    // for an already-finalized panel (idempotent → heartbeat-stable).
    assert!(timed_out_panel_should_reconcile_to_done(true, true));
    assert!(!timed_out_panel_should_reconcile_to_done(true, false));
    assert!(!timed_out_panel_should_reconcile_to_done(false, true));

    // The terminal event the suppressed gate withheld — what the reconcile pushes.
    live.push_status_event(channel, StatusEvent::TurnCompleted { background: false });
    assert!(
        !live.status_panel_is_unfinished(channel),
        "the reconcile transitions 진행 중 → 응답 완료 (no stuck panel)"
    );

    // Heartbeat byte-stability AFTER the reconcile, and the finalize was visible.
    let done_a = live.render_status_panel(channel, &provider, started_at_unix);
    let done_b = live.render_status_panel(channel, &provider, started_at_unix);
    assert_eq!(
        done_a, done_b,
        "the completed panel render is also byte-identical across ticks"
    );
    assert_ne!(
        running_a, done_a,
        "finalizing visibly changed the panel header (진행 중 → 완료)"
    );
}
