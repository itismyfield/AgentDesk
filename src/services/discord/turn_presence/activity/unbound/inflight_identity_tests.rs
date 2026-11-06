use super::*;
use serde_json::json;

fn row() -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Claude,
        77_690_800,
        None,
        200,
        201,
        202,
        "body".into(),
        None,
        Some("session".into()),
        None,
        None,
        0,
    )
}

#[test]
fn every_included_axis_changes_the_projection_even_for_skipped_serde_flags() {
    let original = row();
    let before = original.presence_identity_fields().unwrap();
    let cases = [
        ("version", json!(42)),
        ("provider", json!("codex")),
        ("channel_id", json!(12)),
        ("channel_name", json!("other")),
        ("watcher_owner_channel_id", json!(12)),
        ("logical_channel_id", json!(12)),
        ("thread_id", json!(12)),
        ("request_owner_user_id", json!(12)),
        ("user_msg_id", json!(12)),
        (
            "terminal_receipt",
            json!({"outcome":"completed","confirmed_at":"2026-01-01T00:00:00Z"}),
        ),
        ("intake_outbox_id", json!(12)),
        ("finalizer_turn_id", json!(12)),
        ("session_id", json!("other")),
        ("tmux_session_name", json!("other")),
        ("output_path", json!("other")),
        ("input_fifo_path", json!("other")),
        ("runtime_kind", json!("claude_tui")),
        ("host_locator", json!({"future":"unknown"})),
        ("hosted_record_id", json!(12)),
        ("hosted_execution_nonce", json!("other")),
        ("turn_start_offset", json!(12)),
        ("terminal_delivery_committed", json!(true)),
        ("tui_terminal_source_file_identity", json!([1, 2])),
        ("tui_terminal_generation_mtime_ns", json!(12)),
        ("tui_terminal_kind", json!("Aborted")),
        ("herdr_before_start_stop", json!({"future":"unknown"})),
        ("started_at", json!("other")),
        ("born_generation", json!(12)),
        ("session_bound_delivered", json!(true)),
        ("session_key", json!("other")),
        ("external_turn_id", json!("other")),
        ("native_turn_id", json!("other")),
        ("dispatch_id", json!("other")),
        ("turn_nonce", json!("other")),
        ("last_watcher_relayed_generation_mtime_ns", json!(12)),
        ("restart_mode", json!("drain_restart")),
        ("restart_generation", json!(12)),
        ("rebind_origin", json!(true)),
        ("rebind_origin_birth_generation", json!(12)),
        ("relay_ownership_only", json!(true)),
        ("readopted_from_inflight", json!(true)),
        ("watcher_owns_live_relay", json!(true)),
        ("relay_owner_kind", json!("watcher")),
        ("turn_source", json!("external_input")),
        ("injected_prompt_message_id", json!(12)),
        ("replay_receipt_id", json!(12)),
        (
            "legacy_replay_coordinate",
            json!({"version":1,"install_id":"x","phase":"preparing",
            "prior":{"output_path":null,"last_offset":0,"turn_start_offset":0}}),
        ),
        ("scheduled", json!({"kind":"legacy"})),
    ];
    assert_eq!(cases.len() + 1, before.len());
    for (field, value) in cases {
        let mut wire = serde_json::to_value(&original).unwrap();
        wire[field] = value;
        let changed: InflightTurnState =
            serde_json::from_value(wire).unwrap_or_else(|error| panic!("{field}: {error}"));
        let after = changed.presence_identity_fields().unwrap();
        let label = format!("row.{field}");
        assert_ne!(
            before.iter().find(|(key, _)| *key == label).unwrap().1,
            after.iter().find(|(key, _)| *key == label).unwrap().1,
            "{field}"
        );
    }
    let mut changed = original.clone();
    changed.runtime_kind_unknown_on_disk = true;
    assert_ne!(before, changed.presence_identity_fields().unwrap());
}

#[test]
fn relay_progress_and_message_rollover_leave_presence_identity_unchanged() {
    let original = row();
    let mut changed = original.clone();
    changed.updated_at = "later".into();
    changed.save_generation += 1;
    changed.last_offset += 10;
    changed.full_response.push_str("more");
    changed.response_sent_offset += 1;
    changed.current_msg_id += 1;
    changed.current_msg_len += 1;
    changed.last_watcher_relayed_offset = Some(10);
    changed.streaming_rollover_frozen_msg_ids.push(12);
    changed.status_message_id = Some(12);
    changed.status_panel_generation += 1;
    changed.current_tool_line = Some("tool".into());
    changed.prev_tool_status = Some("running".into());
    changed.last_tool_name = Some("Read".into());
    changed.last_tool_summary = Some("summary".into());
    changed.any_tool_used = true;
    changed.has_post_tool_text = true;
    changed.long_running_placeholder_active = true;
    changed.source_message_ids.push(33);
    changed.input_origin_ids.push("input".into());
    assert_eq!(
        original.presence_identity_fields().unwrap(),
        changed.presence_identity_fields().unwrap()
    );
}

#[test]
fn episode_pin_axes_are_a_subset_of_presence_axes_plus_current_message_id() {
    let source = include_str!("../../../inflight/episode_guard.rs");
    let body = source
        .split("struct InflightEpisodePin {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    let fields = row().presence_identity_fields().unwrap();
    for line in body.lines().filter(|line| line.contains(':')) {
        let name = line.trim().split(':').next().unwrap();
        assert!(
            name == "current_msg_id" || fields.iter().any(|(key, _)| *key == format!("row.{name}")),
            "pin axis {name}"
        );
    }
}
