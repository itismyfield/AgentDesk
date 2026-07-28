use super::*;
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::provider::ProviderKind;

const BRIDGE_ENTRY_CALLER: &str = "turn_bridge::spawn_turn_bridge::bridge_entry_test";

fn bridge_entry_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("adk-4259-r4".to_string()),
        343_742_347_365_974_026,
        user_msg_id,
        18,
        "user prompt".to_string(),
        Some("session".to_string()),
        Some(format!("AgentDesk-codex-4259-r4-{user_msg_id}")),
        Some(format!("/tmp/AgentDesk-codex-4259-r4-{user_msg_id}.jsonl")),
        Some(format!("/tmp/AgentDesk-codex-4259-r4-{user_msg_id}.input")),
        512,
    )
}

fn state_path(root: &Path, channel_id: u64) -> PathBuf {
    inflight_state_path(root, &ProviderKind::Codex, channel_id)
}

fn read_state(root: &Path, channel_id: u64) -> InflightTurnState {
    serde_json::from_slice(
        &fs::read(state_path(root, channel_id)).expect("read durable inflight row"),
    )
    .expect("parse durable inflight row")
}

#[test]
fn bridge_entry_patch_persists_owned_fields_and_preserves_same_owner_watcher_progress() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let before = bridge_entry_state(4_259_401, 77_010);
    save_inflight_state_in_root(temp.path(), &before).expect("seed owner row");

    let mut durable = before.clone();
    durable.full_response = "newer watcher answer bytes".to_string();
    durable.response_sent_offset = durable.full_response.len();
    durable.last_offset = 8_192;
    durable.last_watcher_relayed_offset = Some(8_000);
    durable.last_watcher_relayed_generation_mtime_ns = Some(123_456);
    durable.streaming_rollover_frozen_msg_ids = vec![700_001, 700_002];
    durable.current_tool_line = Some("🔧 watcher tool".to_string());
    durable.last_tool_name = Some("exec_command".to_string());
    durable.last_tool_summary = Some("watcher summary".to_string());
    durable.prev_tool_status = Some("watcher complete".to_string());
    durable.task_notification_kind = Some(TaskNotificationKind::Background);
    durable.any_tool_used = true;
    durable.has_post_tool_text = true;
    durable.terminal_delivery_committed = true;
    durable.status_message_id = Some(800_001);
    durable.status_panel_generation = 9;
    save_inflight_state_in_root(temp.path(), &durable)
        .expect("persist same-owner watcher progress");

    let mut after = before.clone();
    after.current_msg_id = 42;
    after.current_msg_len = 17;
    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut after,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::Saved
    );

    let persisted = read_state(temp.path(), before.channel_id);
    assert_eq!(persisted.user_msg_id, 77_010);
    assert_eq!(persisted.current_msg_id, 42);
    assert_eq!(persisted.current_msg_len, 17);
    assert_eq!(persisted.full_response, durable.full_response);
    assert_eq!(persisted.response_sent_offset, durable.response_sent_offset);
    assert_eq!(persisted.last_offset, durable.last_offset);
    assert_eq!(
        persisted.last_watcher_relayed_offset,
        durable.last_watcher_relayed_offset
    );
    assert_eq!(
        persisted.last_watcher_relayed_generation_mtime_ns,
        durable.last_watcher_relayed_generation_mtime_ns
    );
    assert_eq!(
        persisted.streaming_rollover_frozen_msg_ids,
        durable.streaming_rollover_frozen_msg_ids
    );
    assert_eq!(persisted.current_tool_line, durable.current_tool_line);
    assert_eq!(persisted.last_tool_name, durable.last_tool_name);
    assert_eq!(persisted.last_tool_summary, durable.last_tool_summary);
    assert_eq!(persisted.prev_tool_status, durable.prev_tool_status);
    assert_eq!(
        persisted.task_notification_kind,
        durable.task_notification_kind
    );
    assert_eq!(persisted.any_tool_used, durable.any_tool_used);
    assert_eq!(persisted.has_post_tool_text, durable.has_post_tool_text);
    assert_eq!(
        persisted.terminal_delivery_committed,
        durable.terminal_delivery_committed
    );
    assert_eq!(persisted.status_message_id, durable.status_message_id);
    assert_eq!(
        persisted.status_panel_generation,
        durable.status_panel_generation
    );
    assert_eq!(after.full_response, durable.full_response);
    assert_eq!(after.current_tool_line, durable.current_tool_line);
    assert_eq!(after.status_message_id, durable.status_message_id);
}

#[test]
fn bridge_entry_guarded_save_preserves_newer_owner_bytes_on_identity_mismatch() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut stale_owner = bridge_entry_state(4_259_402, 77_010);
    save_inflight_state_in_root(temp.path(), &stale_owner).expect("seed original owner row");
    let before = stale_owner.clone();

    let mut newer_owner = bridge_entry_state(stale_owner.channel_id, 99_999);
    newer_owner.full_response = "newer owner bytes".to_string();
    newer_owner.last_offset = 8_192;
    save_inflight_state_in_root(temp.path(), &newer_owner)
        .expect("replace disk row with newer owner");
    let path = state_path(temp.path(), stale_owner.channel_id);
    let newer_owner_bytes = fs::read(&path).expect("read newer owner bytes");

    stale_owner.current_msg_id = 42;
    stale_owner.full_response = "stale owner overwrite".to_string();
    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut stale_owner,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::IdentityMismatch
    );
    assert_eq!(
        fs::read(&path).expect("read row after declined stale save"),
        newer_owner_bytes,
        "identity mismatch must leave the newer owner's serialized row byte-for-byte unchanged"
    );
}

#[test]
fn bridge_entry_guarded_save_does_not_resurrect_a_deleted_row() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut stale_owner = bridge_entry_state(4_259_403, 77_010);
    save_inflight_state_in_root(temp.path(), &stale_owner).expect("seed owner row");
    let before = stale_owner.clone();
    let path = state_path(temp.path(), stale_owner.channel_id);
    fs::remove_file(&path).expect("delete durable owner row");

    stale_owner.current_msg_id = 42;
    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut stale_owner,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::Missing
    );
    assert!(
        !path.exists(),
        "missing guarded save must not recreate the deleted inflight row"
    );
}

#[test]
fn bridge_entry_patch_preserves_contended_same_owner_ui_group() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let before = bridge_entry_state(4_259_406, 77_010);
    save_inflight_state_in_root(temp.path(), &before).expect("seed owner row");

    let mut after = before.clone();
    after.status_message_id = Some(810_001);
    after.status_panel_generation = 2;
    after.full_response = "stale bridge response".to_string();
    after.response_sent_offset = after.full_response.len();

    let mut durable = before.clone();
    durable.status_message_id = Some(820_001);
    durable.status_panel_generation = 7;
    durable.full_response = "newer watcher response".to_string();
    durable.response_sent_offset = durable.full_response.len();
    save_inflight_state_in_root(temp.path(), &durable).expect("persist newer watcher UI group");

    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut after,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::Saved
    );
    let persisted = read_state(temp.path(), before.channel_id);
    assert_eq!(persisted.status_message_id, durable.status_message_id);
    assert_eq!(
        persisted.status_panel_generation,
        durable.status_panel_generation
    );
    assert_eq!(persisted.full_response, durable.full_response);
    assert_eq!(persisted.response_sent_offset, durable.response_sent_offset);
    assert_eq!(after.status_message_id, durable.status_message_id);
    assert_eq!(
        after.status_panel_generation,
        durable.status_panel_generation
    );
    assert_eq!(after.full_response, durable.full_response);
    assert_eq!(after.response_sent_offset, durable.response_sent_offset);
}

#[test]
fn bridge_entry_patch_accepts_same_value_prior_write_within_ui_group() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let before = bridge_entry_state(4_259_407, 77_010);
    save_inflight_state_in_root(temp.path(), &before).expect("seed owner row");

    let mut after = before.clone();
    after.status_message_id = Some(810_007);
    after.status_panel_generation = 2;
    after.current_msg_id = 42;
    after.current_msg_len = 17;

    let mut durable = before.clone();
    durable.status_message_id = after.status_message_id;
    durable.status_panel_generation = after.status_panel_generation;
    save_inflight_state_in_root(temp.path(), &durable)
        .expect("persist bridge's prior status-panel bind");

    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut after,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::Saved
    );
    let persisted = read_state(temp.path(), before.channel_id);
    assert_eq!(persisted.status_message_id, after.status_message_id);
    assert_eq!(
        persisted.status_panel_generation,
        after.status_panel_generation
    );
    assert_eq!(persisted.current_msg_id, 42);
    assert_eq!(persisted.current_msg_len, 17);
}

#[test]
fn bridge_entry_patch_adopts_same_turn_external_relay_owner_advancement() {
    for (case, owner_kind) in [
        RelayOwnerKind::Watcher,
        RelayOwnerKind::StandbyRelay,
        RelayOwnerKind::SessionBoundRelay,
    ]
    .into_iter()
    .enumerate()
    {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let before = bridge_entry_state(4_259_410 + case as u64, 77_010);
        save_inflight_state_in_root(temp.path(), &before).expect("seed owner row");

        let mut durable = before.clone();
        durable.relay_owner_kind = owner_kind;
        save_inflight_state_in_root(temp.path(), &durable)
            .expect("persist same-turn external relay owner");

        let mut after = before.clone();
        after.current_msg_id = 42;
        assert_eq!(
            patch_bridge_entry_state_if_identity_unchanged_in_root(
                temp.path(),
                &before,
                &mut after,
                BRIDGE_ENTRY_CALLER,
            ),
            GuardedSaveOutcome::Saved
        );
        assert_eq!(after.effective_relay_owner_kind(), owner_kind);
        assert_eq!(
            read_state(temp.path(), before.channel_id).effective_relay_owner_kind(),
            owner_kind
        );
    }
}

#[test]
fn bridge_entry_patch_adopts_same_turn_current_message_clear() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let mut before = bridge_entry_state(4_259_413, 77_010);
    before.current_msg_id = 900_001;
    save_inflight_state_in_root(temp.path(), &before).expect("seed anchored owner row");

    let mut durable = before.clone();
    durable.current_msg_id = 0;
    save_inflight_state_in_root(temp.path(), &durable)
        .expect("persist same-turn orphan placeholder clear");

    let mut after = before.clone();
    after.current_msg_id = 900_002;
    after.long_running_placeholder_active = true;
    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut after,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::Saved
    );
    assert_eq!(after.current_msg_id, 0);
    assert_eq!(read_state(temp.path(), before.channel_id).current_msg_id, 0);
}

#[test]
fn bridge_entry_patch_reports_malformed_json_as_io_error() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let before = bridge_entry_state(4_259_404, 77_010);
    let mut after = before.clone();
    after.current_msg_id = 42;
    let path = state_path(temp.path(), before.channel_id);
    fs::create_dir_all(path.parent().expect("provider directory")).expect("create provider dir");
    fs::write(&path, b"{not valid inflight json").expect("write malformed row");
    let malformed_bytes = fs::read(&path).expect("read malformed row");

    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut after,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::IoError
    );
    assert_eq!(
        fs::read(&path).expect("malformed row remains present"),
        malformed_bytes
    );
}

#[test]
fn bridge_entry_patch_reports_non_not_found_read_failure_as_io_error() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let before = bridge_entry_state(4_259_405, 77_010);
    let mut after = before.clone();
    after.current_msg_id = 42;
    let path = state_path(temp.path(), before.channel_id);
    fs::create_dir_all(&path).expect("replace row path with directory");

    assert_eq!(
        patch_bridge_entry_state_if_identity_unchanged_in_root(
            temp.path(),
            &before,
            &mut after,
            BRIDGE_ENTRY_CALLER,
        ),
        GuardedSaveOutcome::IoError
    );
    assert!(
        path.is_dir(),
        "failed guarded read must not replace the directory"
    );
}
