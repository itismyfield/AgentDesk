//! Real completion and stamp writers preserve coordinate authority and reject stale sources.
//! Rows without a coordinate group keep their existing serialization and progress semantics.

use super::coordinate_replay::{CoordinateChange, CoordinateRmwOutcome};
use super::coordinate_replay_tests::{CHANNEL, change, install, legacy_row, load, row_path, seed};
use super::identity_gate::runtime_stamp::stamp_runtime_handoff_if_matches_identity_in_root;
use super::*;
use crate::services::discord::inflight::episode_guard::lock_inflight_episode_in_root;
use crate::services::discord::inflight::model::{CoordinateInstallPhase, RawTurnWitness};

fn json(root: &Path, channel_id: u64) -> serde_json::Map<String, serde_json::Value> {
    let text = fs::read_to_string(row_path(root, channel_id)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn without(
    mut row: serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> serde_json::Map<String, serde_json::Value> {
    for key in keys {
        row.remove(*key);
    }
    row
}

#[test]
fn stale_completion_keeps_a_released_hold_released_and_the_new_source_cursor() {
    let root = tempfile::tempdir().unwrap();
    let channel = CHANNEL + 10;
    seed(root.path(), &legacy_row(channel));
    let prepare = CoordinateChange::Prepare {
        install_id: "install-1".to_string(),
        raw: RawTurnWitness {
            path: "/raw/rollout.jsonl".to_string(),
            file_identity: (1, 2),
            session_id: "session-5925".to_string(),
            native_turn_id: "turn-5925".to_string(),
            turn_start: 100,
            prefix_record_end: 300,
            prefix_len: 11,
        },
    };
    assert_eq!(
        change(root.path(), channel, prepare),
        CoordinateRmwOutcome::Saved
    );
    // A completion snapshot from the prior source: pending hold, old cursor, old terminal flag.
    let mut stale = load(root.path(), channel);
    stale.full_response.push_str(" TAIL");
    stale.last_offset = 90;
    stale.terminal_delivery_committed = true;
    let install = CoordinateChange::Install {
        install_id: "install-1".to_string(),
        installed: super::coordinate_replay_tests::installed_witness(),
    };
    assert_eq!(
        change(root.path(), channel, install),
        CoordinateRmwOutcome::Saved
    );
    assert_eq!(
        change(root.path(), channel, CoordinateChange::Release),
        CoordinateRmwOutcome::Saved
    );

    let identity = InflightTurnIdentity::from_state(&stale);
    assert_eq!(
        save_inflight_state_if_matches_identity_in_root(root.path(), &stale, &identity, None),
        GuardedSaveOutcome::Saved
    );
    let durable = load(root.path(), channel);
    assert_eq!(
        durable.replay_hold_reasons,
        ["receipt 9 held"],
        "no coordinate hold returns"
    );
    assert_eq!(
        durable
            .legacy_replay_coordinate
            .as_ref()
            .map(|marker| marker.phase),
        Some(CoordinateInstallPhase::Installed)
    );
    assert_eq!(
        durable.full_response, "FROZEN BODY TAIL",
        "prefix-compatible body progress stays"
    );
    assert_eq!(
        durable.last_offset, 0,
        "the prior source cursor is not merged into the new namespace"
    );
    assert!(
        !durable.terminal_delivery_committed,
        "the prior source terminal flag is not merged"
    );
}

#[test]
fn a_stale_runtime_stamp_cannot_move_the_installed_source() {
    let root = tempfile::tempdir().unwrap();
    let channel = CHANNEL + 11;
    let stale = seed(root.path(), &legacy_row(channel));
    let installed = install(root.path(), channel);
    let path = row_path(root.path(), channel);
    let before = fs::read(&path).unwrap();

    let identity = InflightTurnIdentity::from_state(&stale);
    assert_eq!(
        stamp_runtime_handoff_if_matches_identity_in_root(root.path(), &stale, &identity, "test"),
        GuardedSaveOutcome::AuthorityPinned
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "the prior source's output path and cursor must not be stamped over the install"
    );

    let mut current = installed.clone();
    current.current_tool_line = Some("tool".to_string());
    assert_eq!(
        stamp_runtime_handoff_if_matches_identity_in_root(
            root.path(),
            &current,
            &InflightTurnIdentity::from_state(&current),
            "test"
        ),
        GuardedSaveOutcome::Saved
    );
    let durable = load(root.path(), channel);
    assert_eq!(durable.current_tool_line.as_deref(), Some("tool"));
    assert_eq!(durable.output_path, installed.output_path);
}

#[test]
fn rows_without_a_coordinate_group_are_written_as_before() {
    let root = tempfile::tempdir().unwrap();
    let channel = CHANNEL + 12;
    let original = seed(root.path(), &legacy_row(channel));
    let volatile = ["updated_at", "save_generation"];
    let before = json(root.path(), channel);
    assert!(!before.contains_key("legacy_replay_coordinate"));

    let mut completion = original.clone();
    completion.full_response.push_str(" TAIL");
    completion.last_offset = 60;
    let identity = InflightTurnIdentity::from_state(&completion);
    assert_eq!(
        save_inflight_state_if_matches_identity_in_root(root.path(), &completion, &identity, None),
        GuardedSaveOutcome::Saved
    );
    let after_completion = json(root.path(), channel);
    let mut expected = without(before, &volatile);
    expected.insert("full_response".into(), "FROZEN BODY TAIL".into());
    expected.insert("last_offset".into(), 60.into());
    assert_eq!(without(after_completion.clone(), &volatile), expected);
    let durable = load(root.path(), channel);
    completion.updated_at = durable.updated_at;
    completion.save_generation = durable.save_generation;
    assert_eq!(
        fs::read_to_string(row_path(root.path(), channel)).unwrap(),
        serde_json::to_string_pretty(&completion).unwrap()
    );

    let mut stamp = load(root.path(), channel);
    stamp.current_tool_line = Some("tool".to_string());
    stamp.last_offset = 70;
    let identity = InflightTurnIdentity::from_state(&stamp);
    assert_eq!(
        stamp_runtime_handoff_if_matches_identity_in_root(root.path(), &stamp, &identity, "test"),
        GuardedSaveOutcome::Saved
    );
    let mut expected = without(after_completion, &volatile);
    expected.insert("current_tool_line".into(), "tool".into());
    expected.insert("last_offset".into(), 70.into());
    assert_eq!(without(json(root.path(), channel), &volatile), expected);
    let durable = load(root.path(), channel);
    stamp.updated_at = durable.updated_at;
    stamp.save_generation = durable.save_generation;
    assert_eq!(
        fs::read_to_string(row_path(root.path(), channel)).unwrap(),
        serde_json::to_string_pretty(&stamp).unwrap()
    );
}

#[test]
fn a_general_write_cannot_replace_an_unreadable_coordinate_group() {
    let root = tempfile::tempdir().unwrap();
    let channel = CHANNEL + 13;
    let original = seed(root.path(), &legacy_row(channel));
    let path = row_path(root.path(), channel);
    let mut locked = lock_inflight_episode_in_root(
        root.path(),
        &ProviderKind::Codex,
        channel,
        &InflightEpisodePin::from_state(&original),
    )
    .unwrap();
    let corrupt = b"{\"legacy_replay_coordinate\": {";
    fs::write(&path, corrupt).unwrap();
    assert_eq!(
        locked.persist_verified_under_guard(&original, "test corrupt coordinate group"),
        GuardedSaveOutcome::IoError,
        "an unreadable tagged row cannot authorize an unmarked general write"
    );
    assert_eq!(fs::read(&path).unwrap(), corrupt);
}
