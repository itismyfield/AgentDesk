//! #3479 Phase-1 rank-2: tests for the terminal-readiness predicates + the
//! pure buffer/message-id reconcilers. PURE MOVE from `tmux_watcher.rs`'s
//! `#[cfg(test)] mod tests` (zero logic change). Kept in a sibling `*_tests.rs`
//! so the production module stays within the
//! `src/services/discord/tmux_watcher/**` namespace LoC cap (test files are
//! excluded from the cap by the audit's `production_rust_files()` filter).

use super::*;

#[test]
fn bridge_suppressed_turn_discards_pending_buffer_before_direct_input() {
    let mut all_data = "{\"type\":\"assistant\",\"message\":\"old\"}\n".to_string();
    let mut all_data_start_offset = 10;
    let mut all_data_fully_mirrored_to_session_relay = false;
    let mut all_data_session_bound_relay_ack = None;

    discard_watcher_pending_buffer_after_suppressed_turn(
        &mut all_data,
        &mut all_data_start_offset,
        &mut all_data_fully_mirrored_to_session_relay,
        &mut all_data_session_bound_relay_ack,
        42,
    );

    assert!(all_data.is_empty());
    assert_eq!(all_data_start_offset, 42);
    assert!(all_data_fully_mirrored_to_session_relay);
    assert!(all_data_session_bound_relay_ack.is_none());
}

#[test]
fn terminal_relay_adopts_late_saved_inflight_message_ids() {
    let mut inflight = InflightTurnState::new(
        ProviderKind::Claude,
        123,
        Some("adk-cc".to_string()),
        42,
        1001,
        2002,
        "prompt".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    );
    inflight.status_message_id = Some(3003);

    let mut placeholder_msg_id = None;
    let mut placeholder_from_restored_inflight = false;
    let mut status_panel_msg_id = None;

    adopt_watcher_terminal_message_ids_from_inflight(
        &mut placeholder_msg_id,
        &mut placeholder_from_restored_inflight,
        &mut status_panel_msg_id,
        &inflight,
        "AgentDesk-claude-adk-cc",
    );

    assert_eq!(placeholder_msg_id, Some(MessageId::new(2002)));
    assert!(placeholder_from_restored_inflight);
    assert_eq!(status_panel_msg_id, Some(MessageId::new(3003)));
}

#[test]
fn terminal_relay_does_not_adopt_synthetic_status_panel_message_id() {
    let mut inflight = InflightTurnState::new(
        ProviderKind::Claude,
        123,
        Some("adk-cc".to_string()),
        42,
        1001,
        2002,
        "prompt".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    );
    inflight.status_message_id = Some(9_100_000_000_000_000_123);

    let mut placeholder_msg_id = None;
    let mut placeholder_from_restored_inflight = false;
    let mut status_panel_msg_id = None;

    adopt_watcher_terminal_message_ids_from_inflight(
        &mut placeholder_msg_id,
        &mut placeholder_from_restored_inflight,
        &mut status_panel_msg_id,
        &inflight,
        "AgentDesk-claude-adk-cc",
    );

    assert_eq!(placeholder_msg_id, Some(MessageId::new(2002)));
    assert!(placeholder_from_restored_inflight);
    assert_eq!(status_panel_msg_id, None);
}

#[test]
fn terminal_relay_does_not_adopt_inflight_for_other_tmux_session() {
    let mut inflight = InflightTurnState::new(
        ProviderKind::Claude,
        123,
        Some("adk-cc".to_string()),
        42,
        1001,
        2002,
        "prompt".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-claude-other".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    );
    inflight.status_message_id = Some(3003);

    let mut placeholder_msg_id = None;
    let mut placeholder_from_restored_inflight = false;
    let mut status_panel_msg_id = None;

    adopt_watcher_terminal_message_ids_from_inflight(
        &mut placeholder_msg_id,
        &mut placeholder_from_restored_inflight,
        &mut status_panel_msg_id,
        &inflight,
        "AgentDesk-claude-adk-cc",
    );

    assert_eq!(placeholder_msg_id, None);
    assert!(!placeholder_from_restored_inflight);
    assert_eq!(status_panel_msg_id, None);
}

#[test]
fn terminal_relay_does_not_adopt_placeholderless_user_message() {
    let inflight = InflightTurnState::new(
        ProviderKind::Claude,
        123,
        Some("adk-cc".to_string()),
        42,
        1001,
        1001,
        "prompt".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    );

    let mut placeholder_msg_id = None;
    let mut placeholder_from_restored_inflight = false;
    let mut status_panel_msg_id = None;

    adopt_watcher_terminal_message_ids_from_inflight(
        &mut placeholder_msg_id,
        &mut placeholder_from_restored_inflight,
        &mut status_panel_msg_id,
        &inflight,
        "AgentDesk-claude-adk-cc",
    );

    assert_eq!(placeholder_msg_id, None);
    assert!(!placeholder_from_restored_inflight);
    assert_eq!(status_panel_msg_id, None);
}

#[test]
fn external_input_lease_is_consumed_only_by_external_input_inflight() {
    let mut managed = InflightTurnState::new(
        ProviderKind::Claude,
        123,
        Some("adk-cc".to_string()),
        42,
        1001,
        2002,
        "prompt".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    );
    assert!(!watcher_inflight_represents_external_input(Some(&managed)));

    managed.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    assert!(watcher_inflight_represents_external_input(Some(&managed)));

    managed.turn_source = crate::services::discord::inflight::TurnSource::ExternalAdopted;
    assert!(watcher_inflight_represents_external_input(Some(&managed)));
}

#[test]
fn watcher_direct_terminal_idle_commit_requires_delivery_without_inflight() {
    assert!(watcher_direct_terminal_should_commit_session_idle(
        true, false, true, false, false, false
    ));
    assert!(watcher_direct_terminal_should_commit_session_idle(
        true, false, false, true, false, false
    ));
    assert!(watcher_direct_terminal_should_commit_session_idle(
        true, false, false, false, true, false
    ));
    assert!(watcher_direct_terminal_should_commit_session_idle(
        true, false, false, false, false, true
    ));
    assert!(!watcher_direct_terminal_should_commit_session_idle(
        false, false, true, true, true, true
    ));
    assert!(!watcher_direct_terminal_should_commit_session_idle(
        true, true, true, true, true, true
    ));
    assert!(watcher_direct_terminal_should_commit_session_idle(
        true, false, false, false, false, false
    ));
}

#[test]
fn watcher_direct_terminal_idle_commit_keeps_later_token_update_idle() {
    assert_eq!(watcher_terminal_token_update_status(true), "idle");
    assert_eq!(
        watcher_terminal_token_update_status(false),
        crate::db::session_status::TURN_ACTIVE
    );
}

#[test]
fn claude_watcher_ready_uses_transcript_turn_state_not_pane_prompt() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        concat!(
            r#"{"type":"user","message":{"content":"review"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            "\n"
        ),
    )
    .unwrap();
    let len = std::fs::metadata(file.path()).unwrap().len();

    assert_eq!(
        watcher_jsonl_turn_state_ready_for_input(
            &crate::services::provider::ProviderKind::Claude,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
            file.path().to_str().unwrap(),
            len,
        ),
        Some(false)
    );

    std::fs::write(
        file.path(),
        concat!(
            r#"{"type":"user","message":{"content":"review"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
            "\n"
        ),
    )
    .unwrap();
    let len = std::fs::metadata(file.path()).unwrap().len();

    assert_eq!(
        watcher_jsonl_turn_state_ready_for_input(
            &crate::services::provider::ProviderKind::Claude,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
            file.path().to_str().unwrap(),
            len,
        ),
        Some(true)
    );
}

// The transcript holds a fully written terminator envelope
// (`system/turn_duration`) and the watcher's `current_offset` lags the
// file size by one byte. Pre-fix the watcher would return Busy and the
// idle-queue drain would loop indefinitely (the production 9× recurrence
// observed on 2026-05-26: `hosted TUI structured turn state is busy`
// every 2s after #2789 froze the binding offset across quick-exit
// restarts). The strict-terminator override in `jsonl_ready_for_input`
// now classifies a fully-parsed terminator envelope as Ready regardless
// of the relay's last_offset; partial trailing fragments are still
// refused, so this is safe.
#[test]
fn claude_watcher_ready_treats_complete_terminator_envelope_as_ready() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
    )
    .unwrap();
    let len = std::fs::metadata(file.path()).unwrap().len();

    assert_eq!(
        watcher_jsonl_turn_state_ready_for_input(
            &crate::services::provider::ProviderKind::Claude,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
            file.path().to_str().unwrap(),
            len.saturating_sub(1),
        ),
        Some(true)
    );
}

// Race guard at the watcher boundary: a complete terminator envelope is
// followed by a partial `{"ty` fragment of the next turn's user line and
// the watcher's offset still lags. The strict-terminator predicate must
// refuse to fall through the partial line, keeping the watcher non-ready
// so we do not race a new turn that has just begun.
#[test]
fn claude_watcher_ready_keeps_busy_when_partial_user_follows_terminator() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        concat!(
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
            "\n",
            r#"{"ty"#,
        ),
    )
    .unwrap();
    let len = std::fs::metadata(file.path()).unwrap().len();

    assert_eq!(
        watcher_jsonl_turn_state_ready_for_input(
            &crate::services::provider::ProviderKind::Claude,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
            file.path().to_str().unwrap(),
            len.saturating_sub(5),
        ),
        Some(false)
    );
}
