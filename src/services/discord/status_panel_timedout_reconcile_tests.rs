use super::{
    reconcile_same_turn_identity, register_finalized_panel_terminal_anchor,
    timed_out_panel_should_reconcile_to_done, turn_jsonl_deterministically_terminal,
};
use crate::services::agent_protocol::StatusEvent;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::placeholder_cleanup::{
    PlaceholderCleanupRegistry, committed_terminal_panel_anchor_skip,
};
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
    // The current turn advanced its own output past the turn-start anchor. The
    // single-line fixtures these warm states point at ARE the current turn, so the
    // anchor is at the file start (offset 0) and the terminator the slice scan
    // looks for is within `[turn_start_offset, EOF)`. Multi-turn fixtures (a PRIOR
    // terminator below the anchor) set their own offsets explicitly.
    state.turn_start_offset = Some(0);
    state.last_offset = 50;
    state.rebind_origin = false;
    state
}

fn write_terminal_jsonl() -> tempfile::NamedTempFile {
    write_jsonl(r#"{"type":"result","result":"done","session_id":"s"}"#)
}

fn write_jsonl(content: &str) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("temp jsonl");
    std::fs::write(file.path(), content).expect("write jsonl");
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

// DEFECT 1 (codex #3951): the finalize-safe turn-END-only probe must NOT treat
// any NON-terminator Idle-class envelope as a turn end. The lenient idle-queue-
// drain observer reads each of these as at-rest, which would FALSE-FINALIZE a
// quiet still-running turn (the #2161 premature-completion class). None of these
// IS the authoritative per-provider turn terminator, so the reconcile must leave
// the turn running.
#[test]
fn idle_class_non_terminator_envelopes_are_not_deterministically_terminal() {
    let cases: &[(ProviderKind, &str, &str)] = &[
        (
            ProviderKind::Codex,
            "session_meta",
            r#"{"type":"session_meta","payload":{}}"#,
        ),
        (
            ProviderKind::Codex,
            "thread.started",
            r#"{"type":"thread.started"}"#,
        ),
        (
            ProviderKind::Codex,
            "task_complete",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ),
        (
            ProviderKind::Codex,
            "completed agent_message",
            r#"{"type":"item.completed","item":{"type":"agent_message"}}"#,
        ),
        (
            ProviderKind::Claude,
            "system:init",
            r#"{"type":"system","subtype":"init"}"#,
        ),
    ];
    for (provider, label, jsonl) in cases {
        let file = write_jsonl(jsonl);
        let state = warm_tui_state("AgentDesk-warm-1", &file.path().display().to_string());
        assert!(
            !turn_jsonl_deterministically_terminal(provider, &state),
            "{label}: a non-terminator Idle-class envelope must NOT finalize a quiet running turn"
        );
    }

    // Positive control: the authoritative per-provider TURN terminator DOES
    // finalize, so the probe is not vacuously false.
    let codex_done = write_jsonl(r#"{"type":"turn.completed","payload":{}}"#);
    let codex_state = warm_tui_state("AgentDesk-warm-1", &codex_done.path().display().to_string());
    assert!(
        turn_jsonl_deterministically_terminal(&ProviderKind::Codex, &codex_state),
        "the Codex turn.completed terminator IS deterministically terminal"
    );
}

// DEFECT 1 (codex #3951) fail-safe: a torn/short trailing write (the writer was
// mid-flush when the sweep read the JSONL) that is NOT recognized post-turn
// housekeeping keeps the turn Busy — even when a real terminator sits beneath it
// — so the reconcile never finalizes on an ambiguous tail.
#[test]
fn torn_or_short_trailing_jsonl_line_is_not_deterministically_terminal() {
    // A complete `result` terminator shadowed by an active-looking torn trailing
    // line (a new assistant stream just started): NOT terminal.
    let active_torn = write_jsonl(
        "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n{\"type\":\"assistant\",\"message\":{",
    );
    let state = warm_tui_state(
        "AgentDesk-warm-1",
        &active_torn.path().display().to_string(),
    );
    assert!(
        !turn_jsonl_deterministically_terminal(&ProviderKind::Claude, &state),
        "an active-looking torn trailing line shadows the terminator beneath → not terminal"
    );

    // An unrecoverable short fragment (could be the start of a fresh `user`
    // envelope) is likewise not terminal.
    let short_torn = write_jsonl("{\"ty");
    let short_state = warm_tui_state("AgentDesk-warm-1", &short_torn.path().display().to_string());
    assert!(
        !turn_jsonl_deterministically_terminal(&ProviderKind::Claude, &short_state),
        "a too-short torn fragment cannot prove the turn ended → not terminal"
    );
}

// DEFECT 2 (codex #3951): a stale sweep snapshot must NOT finalize a NEXT turn's
// freshly-restarted panel. The reconcile re-reads the fresh on-disk row and only
// finalizes when EVERY turn/panel identity field still matches the snapshot.
#[test]
fn reconcile_no_ops_on_stale_snapshot_vs_next_turn_identity() {
    let file = write_terminal_jsonl();
    let mut snapshot = warm_tui_state("AgentDesk-warm-1", &file.path().display().to_string());
    snapshot.status_message_id = Some(555);

    // Same turn still on disk → finalize is allowed.
    let same = snapshot.clone();
    assert!(
        reconcile_same_turn_identity(&snapshot, &same),
        "the unchanged on-disk row is the same turn → finalize allowed"
    );

    // NEXT turn: a fresh user message + restarted panel → NO-OP.
    let mut next_turn = snapshot.clone();
    next_turn.user_msg_id = snapshot.user_msg_id + 1;
    next_turn.current_msg_id = snapshot.current_msg_id + 1;
    next_turn.status_message_id = Some(777);
    assert!(
        !reconcile_same_turn_identity(&snapshot, &next_turn),
        "a different turn / restarted panel must not be finalized off the stale snapshot"
    );

    // A session/output rebind onto the channel is likewise a different turn.
    let mut rebound = snapshot.clone();
    rebound.output_path = Some("/tmp/agentdesk-other-turn.jsonl".to_string());
    assert!(
        !reconcile_same_turn_identity(&snapshot, &rebound),
        "an output-path rebind is a different turn → NO-OP"
    );
    let mut resession = snapshot.clone();
    resession.tmux_session_name = Some("AgentDesk-warm-2".to_string());
    assert!(
        !reconcile_same_turn_identity(&snapshot, &resession),
        "a tmux-session rebind is a different turn → NO-OP"
    );
}

// DEFECT 3 (codex #3951): after the reconcile finalizes a panel to `응답 완료`,
// the orphan-panel reclaim STILL runs in the same pass and would delete the aged
// panel. Registering the #3607 committed-terminal anchor makes that reclaim SKIP
// the just-finalized panel.
#[test]
fn finalized_panel_is_skipped_by_same_pass_orphan_reclaim() {
    let registry = PlaceholderCleanupRegistry::default();
    let provider = ProviderKind::Claude;
    let channel = serenity::ChannelId::new(42);
    let panel_msg = serenity::MessageId::new(555);
    let file = write_terminal_jsonl();
    let mut state = warm_tui_state("AgentDesk-warm-1", &file.path().display().to_string());
    state.status_message_id = Some(panel_msg.get());

    // Before finalize: no anchor → the orphan reclaim would DELETE the panel.
    assert!(
        !committed_terminal_panel_anchor_skip(&registry, &provider, channel, panel_msg, &state),
        "with no committed-terminal anchor the orphan reclaim would delete the panel"
    );

    // The reconcile registers the anchor on a committed finalize.
    register_finalized_panel_terminal_anchor(
        &registry,
        &provider,
        channel,
        panel_msg,
        state.tmux_session_name.as_deref(),
    );

    // Same pass: the orphan reclaim now SKIPS the just-finalized panel.
    assert!(
        committed_terminal_panel_anchor_skip(&registry, &provider, channel, panel_msg, &state),
        "the #3607 anchor makes the same-pass orphan reclaim skip the finalized panel"
    );
}

// DEFECT 1b (codex #3951 round-2): a PRIOR Codex turn's `turn.completed` sits
// BELOW turn_start_offset; the CURRENT turn has only written a non-terminator
// completed `agent_message` (item.completed) so far. The unbounded turn-END scan
// would walk past the current turn's non-terminator and latch the prior
// terminator — the offset-scoped guard must reject it (turn still running).
#[test]
fn prior_turn_terminator_below_offset_does_not_finalize_running_codex_turn() {
    let prior = "{\"type\":\"turn.completed\",\"payload\":{}}\n";
    let current = "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\"}}";
    let file = write_jsonl(&format!("{prior}{current}"));
    let mut state = warm_tui_state("AgentDesk-warm-1", &file.path().display().to_string());
    state.turn_start_offset = Some(prior.len() as u64);
    state.last_offset = (prior.len() + current.len()) as u64;
    assert!(
        !turn_jsonl_deterministically_terminal(&ProviderKind::Codex, &state),
        "a prior turn.completed below the anchor must not finalize the running current turn"
    );

    // Control: once the CURRENT turn writes its OWN turn.completed (after the
    // anchor) the reconcile may finalize.
    let current_done = format!("{current}\n{{\"type\":\"turn.completed\",\"payload\":{{}}}}");
    let file2 = write_jsonl(&format!("{prior}{current_done}"));
    let mut done = warm_tui_state("AgentDesk-warm-1", &file2.path().display().to_string());
    done.turn_start_offset = Some(prior.len() as u64);
    done.last_offset = (prior.len() + current_done.len()) as u64;
    assert!(
        turn_jsonl_deterministically_terminal(&ProviderKind::Codex, &done),
        "the current turn's own turn.completed (after the anchor) IS deterministically terminal"
    );
}

// DEFECT 1b (codex #3951 round-2): a PRIOR Claude turn's `result` sits BELOW
// turn_start_offset; the CURRENT turn has only written a `system{init}`
// (session-start, NON-terminator) so far. The offset-scoped guard must reject
// finalize even though the unbounded scan would walk past system:init to the
// prior result.
#[test]
fn prior_turn_terminator_below_offset_does_not_finalize_running_claude_turn() {
    let prior = "{\"type\":\"result\",\"result\":\"prior\",\"session_id\":\"s\"}\n";
    let current = "{\"type\":\"system\",\"subtype\":\"init\"}";
    let file = write_jsonl(&format!("{prior}{current}"));
    let mut state = warm_tui_state("AgentDesk-warm-1", &file.path().display().to_string());
    state.turn_start_offset = Some(prior.len() as u64);
    state.last_offset = (prior.len() + current.len()) as u64;
    assert!(
        !turn_jsonl_deterministically_terminal(&ProviderKind::Claude, &state),
        "a prior result below the anchor must not finalize the running current turn (system:init is not a terminator)"
    );

    // Control: the CURRENT turn's own result (after the anchor) IS terminal.
    let current_done = "{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"result\",\"result\":\"now\",\"session_id\":\"s\"}";
    let file2 = write_jsonl(&format!("{prior}{current_done}"));
    let mut done = warm_tui_state("AgentDesk-warm-1", &file2.path().display().to_string());
    done.turn_start_offset = Some(prior.len() as u64);
    done.last_offset = (prior.len() + current_done.len()) as u64;
    assert!(
        turn_jsonl_deterministically_terminal(&ProviderKind::Claude, &done),
        "the current turn's own result (after the anchor) IS deterministically terminal"
    );
}
