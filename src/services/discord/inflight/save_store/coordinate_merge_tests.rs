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

#[test]
fn s1_projection_keeps_same_turn_progress_across_zero_and_nonzero_rebases() {
    let mut failures = Vec::new();
    let mut cases = 0;
    for start in [0, 128] {
        for stamp in [false, true] {
            cases += 1;
            let observed = std::panic::catch_unwind(|| {
                let root = tempfile::tempdir().unwrap();
                let mut initial = legacy_row(CHANNEL + 20);
                initial.turn_start_offset = Some(start);
                initial.last_offset = start + 512;
                initial.external_turn_id = Some("external-5925".into());
                initial.injected_prompt_message_id = Some(initial.user_msg_id);
                let mut stale = seed(root.path(), &initial);
                let installed = install(root.path(), initial.channel_id);
                stale.full_response.push_str(" TAIL");
                stale.response_sent_offset = 11;
                stale
                    .replay_hold_reasons
                    .push("other_namespace:held".into());
                stale.streaming_rollover_frozen_msg_ids.push(5925999);
                stale.last_offset = 99999;
                stale.last_watcher_relayed_offset = Some(99999);
                stale.last_watcher_relayed_generation_mtime_ns = Some(99999);
                stale.terminal_delivery_committed = true;
                stale.current_tool_line = Some("old-source tool".into());
                stale.output_path = Some("/old-source".into());
                stale.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
                let identity = InflightTurnIdentity::from_state(&stale);
                let outcome = if stamp {
                    stamp_runtime_handoff_if_matches_identity_in_root(
                        root.path(),
                        &mut stale,
                        &identity,
                        "s1 projection",
                    )
                } else {
                    save_inflight_state_if_matches_identity_in_root(
                        root.path(),
                        &mut stale,
                        &identity,
                        Some(start),
                    )
                };
                assert_eq!(
                    outcome,
                    if stamp {
                        GuardedSaveOutcome::AuthorityPinned
                    } else {
                        GuardedSaveOutcome::Saved
                    }
                );
                let durable = load(root.path(), initial.channel_id);
                assert_eq!(
                    durable.full_response, "FROZEN BODY TAIL",
                    "start={start}, stamp={stamp}"
                );
                assert_eq!(
                    durable.response_sent_offset, 11,
                    "renderer boundary is not body length"
                );
                assert!(
                    durable
                        .replay_hold_reasons
                        .iter()
                        .any(|r| r == "other_namespace:held")
                );
                assert_eq!(durable.streaming_rollover_frozen_msg_ids, [5925999]);
                assert_eq!(durable.current_msg_id, initial.current_msg_id);
                assert_eq!(durable.finalizer_turn_id, initial.finalizer_turn_id);
                assert_eq!(durable.turn_nonce, initial.turn_nonce);
                assert_eq!(durable.external_turn_id, initial.external_turn_id);
                assert_eq!(
                    durable.injected_prompt_message_id,
                    initial.injected_prompt_message_id
                );
                assert_eq!(durable.output_path, installed.output_path);
                assert_eq!(durable.runtime_kind, installed.runtime_kind);
                assert_eq!(
                    durable.effective_relay_owner_kind(),
                    installed.effective_relay_owner_kind()
                );
                assert_eq!(durable.last_offset, installed.last_offset);
                assert_eq!(durable.turn_start_offset, installed.turn_start_offset);
                assert_eq!(
                    durable.last_watcher_relayed_offset,
                    installed.last_watcher_relayed_offset
                );
                assert_eq!(
                    durable.last_watcher_relayed_generation_mtime_ns,
                    installed.last_watcher_relayed_generation_mtime_ns
                );
                assert_eq!(durable.current_tool_line, installed.current_tool_line);
                assert_eq!(
                    durable.terminal_delivery_committed,
                    installed.terminal_delivery_committed
                );
                assert_eq!(
                    serde_json::to_value(&stale).unwrap(),
                    serde_json::to_value(&durable).unwrap(),
                    "mutable target adopts durable projection despite runtime refusal"
                );
            });
            if observed.is_err() {
                failures.push((start, stamp));
            }
        }
    }
    assert_eq!(cases, 4);
    assert!(
        failures.is_empty(),
        "failed start/stamp cases: {failures:?}"
    );
}

#[test]
fn s1_projection_refuses_foreign_identity_or_unproven_transition() {
    for axis in 0..12 {
        for stamp in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let initial = legacy_row(CHANNEL + 21);
            let mut stale = seed(root.path(), &initial);
            let mut installed = install(root.path(), initial.channel_id);
            stale.full_response.push_str(" TAIL");
            match axis {
                0 => stale.turn_nonce = Some("successor".into()),
                1 => stale.native_turn_id = Some("other-turn".into()),
                2 => stale.session_id = Some("other-session".into()),
                3 => stale.external_turn_id = Some("other-external".into()),
                4 => stale.finalizer_turn_id += 1,
                5 => stale.injected_prompt_message_id = Some(9988),
                6 => {
                    installed.legacy_replay_coordinate.as_mut().unwrap().raw = None;
                    fs::write(
                        row_path(root.path(), initial.channel_id),
                        serde_json::to_vec_pretty(&installed).unwrap(),
                    )
                    .unwrap();
                }
                7 => stale.started_at.push_str("other-birth"),
                8 => stale.user_msg_id += 1,
                9 => {
                    stale.turn_nonce = Some("   ".into());
                    installed.turn_nonce = stale.turn_nonce.clone();
                    fs::write(
                        row_path(root.path(), initial.channel_id),
                        serde_json::to_vec_pretty(&installed).unwrap(),
                    )
                    .unwrap();
                }
                10 => {
                    installed.legacy_replay_coordinate.as_mut().unwrap().version = 2;
                    fs::write(
                        row_path(root.path(), initial.channel_id),
                        serde_json::to_vec_pretty(&installed).unwrap(),
                    )
                    .unwrap();
                }
                _ => {
                    installed
                        .legacy_replay_coordinate
                        .as_mut()
                        .unwrap()
                        .prior
                        .turn_start_offset = Some(128);
                    fs::write(
                        row_path(root.path(), initial.channel_id),
                        serde_json::to_vec_pretty(&installed).unwrap(),
                    )
                    .unwrap();
                }
            }
            let before = fs::read(row_path(root.path(), initial.channel_id)).unwrap();
            let identity = InflightTurnIdentity::from_state(&stale);
            if stamp {
                assert_ne!(
                    stamp_runtime_handoff_if_matches_identity_in_root(
                        root.path(),
                        &mut stale,
                        &identity,
                        "foreign"
                    ),
                    GuardedSaveOutcome::Saved
                );
            } else {
                assert_ne!(
                    save_inflight_state_if_matches_identity_in_root(
                        root.path(),
                        &mut stale,
                        &identity,
                        None
                    ),
                    GuardedSaveOutcome::Saved
                );
            }
            assert_eq!(
                fs::read(row_path(root.path(), initial.channel_id)).unwrap(),
                before,
                "axis={axis}, stamp={stamp}"
            );
        }
    }
}
