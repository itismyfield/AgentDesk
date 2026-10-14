use super::*;
use crate::services::discord::inflight::watcher_state::*;

fn stream_patch() -> WatcherStreamProgressPatch {
    WatcherStreamProgressPatch {
        current_msg_id: Some(8888),
        full_response: "FROZEN BODY TAIL".into(),
        response_sent_offset: 11,
        current_tool_line: None,
        prev_tool_status: None,
        task_notification_kind: None,
        any_tool_used: false,
        has_post_tool_text: false,
        streaming_rollover_frozen_msg_ids: vec![9999],
    }
}

#[test]
fn s1_old_source_stream_rmw_keeps_installed_bytes() {
    let root = tempfile::tempdir().unwrap();
    let legacy = seed(root.path(), &legacy_row(CHANNEL + 30));
    install(root.path(), legacy.channel_id);
    let path = row_path(root.path(), legacy.channel_id);
    let before = fs::read(&path).unwrap();
    let identity = InflightTurnIdentity::from_state(&legacy);
    let outcome = persist_watcher_stream_progress_locked_in_root(
        root.path(),
        &ProviderKind::Codex,
        legacy.channel_id,
        Some(&identity),
        "tmux-5925",
        stream_patch(),
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "old source stream patch changed installed bytes"
    );
    assert_ne!(outcome, WatcherProgressOutcome::Saved);
}

#[test]
fn s1_old_source_terminal_rmw_keeps_installed_bytes() {
    let root = tempfile::tempdir().unwrap();
    let legacy = seed(root.path(), &legacy_row(CHANNEL + 31));
    install(root.path(), legacy.channel_id);
    let path = row_path(root.path(), legacy.channel_id);
    let before = fs::read(&path).unwrap();
    let identity = InflightTurnIdentity::from_state(&legacy);
    let patch = WatcherTerminalCommitPatch {
        full_response: "FROZEN BODY TAIL".into(),
        last_offset: 9999,
        last_watcher_relayed_offset: Some(9999),
        last_watcher_relayed_generation_mtime_ns: Some(11),
    };
    let outcome = commit_watcher_terminal_delivery_locked_in_root(
        root.path(),
        &ProviderKind::Codex,
        legacy.channel_id,
        &identity,
        "tmux-5925",
        patch,
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "old source terminal patch changed installed bytes"
    );
    assert_ne!(outcome, WatcherTerminalCommitOutcome::Committed);
}

#[test]
fn s1_old_source_watermark_rmw_keeps_installed_bytes() {
    let root = tempfile::tempdir().unwrap();
    let legacy = seed(root.path(), &legacy_row(CHANNEL + 32));
    install(root.path(), legacy.channel_id);
    let path = row_path(root.path(), legacy.channel_id);
    let before = fs::read(&path).unwrap();
    let identity = InflightTurnIdentity::from_state(&legacy);
    let patch = WatcherRelayWatermarkPatch {
        last_watcher_relayed_offset: Some(9999),
        last_watcher_relayed_generation_mtime_ns: Some(11),
    };
    let outcome = persist_watcher_relay_watermark_locked_in_root(
        root.path(),
        &ProviderKind::Codex,
        legacy.channel_id,
        &identity,
        "tmux-5925",
        patch,
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "old source watermark patch changed installed bytes"
    );
    assert_ne!(outcome, WatcherRelayWatermarkOutcome::Saved);
}

#[test]
fn s1_observed_witness_reaches_all_three_rmws_with_positive_progress() {
    for managed in [false, true] {
        for kind in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let legacy = seed(root.path(), &legacy_row(CHANNEL + 33));
            let old = CoordinateSourceWitness::of(&legacy);
            let current = if managed {
                install(root.path(), legacy.channel_id)
            } else {
                legacy.clone()
            };
            let witness = CoordinateSourceWitness::of(&current);
            let identity = InflightTurnIdentity::from_state(&current);
            let path = row_path(root.path(), legacy.channel_id);
            if managed {
                let before = fs::read(&path).unwrap();
                match kind {
                    0 => assert_ne!(
                        persist_watcher_stream_progress_locked_in_root(
                            root.path(),
                            &ProviderKind::Codex,
                            legacy.channel_id,
                            (&identity, &old),
                            "tmux-5925",
                            stream_patch()
                        ),
                        WatcherProgressOutcome::Saved
                    ),
                    1 => assert_ne!(
                        commit_watcher_terminal_delivery_locked_in_root(
                            root.path(),
                            &ProviderKind::Codex,
                            legacy.channel_id,
                            (&identity, &old),
                            "tmux-5925",
                            WatcherTerminalCommitPatch {
                                full_response: "FROZEN BODY TAIL".into(),
                                last_offset: 512,
                                last_watcher_relayed_offset: Some(512),
                                last_watcher_relayed_generation_mtime_ns: Some(77)
                            }
                        ),
                        WatcherTerminalCommitOutcome::Committed
                    ),
                    _ => assert_ne!(
                        persist_watcher_relay_watermark_locked_in_root(
                            root.path(),
                            &ProviderKind::Codex,
                            legacy.channel_id,
                            (&identity, &old),
                            "tmux-5925",
                            WatcherRelayWatermarkPatch {
                                last_watcher_relayed_offset: Some(512),
                                last_watcher_relayed_generation_mtime_ns: Some(77)
                            }
                        ),
                        WatcherRelayWatermarkOutcome::Saved
                    ),
                }
                assert_eq!(fs::read(&path).unwrap(), before);
            }
            match kind {
                0 => assert_eq!(
                    persist_watcher_stream_progress_locked_in_root(
                        root.path(),
                        &ProviderKind::Codex,
                        legacy.channel_id,
                        (&identity, &witness),
                        "tmux-5925",
                        stream_patch()
                    ),
                    WatcherProgressOutcome::Saved
                ),
                1 => assert_eq!(
                    commit_watcher_terminal_delivery_locked_in_root(
                        root.path(),
                        &ProviderKind::Codex,
                        legacy.channel_id,
                        (&identity, &witness),
                        "tmux-5925",
                        WatcherTerminalCommitPatch {
                            full_response: "FROZEN BODY TAIL".into(),
                            last_offset: 512,
                            last_watcher_relayed_offset: Some(512),
                            last_watcher_relayed_generation_mtime_ns: Some(77)
                        }
                    ),
                    WatcherTerminalCommitOutcome::Committed
                ),
                _ => assert_eq!(
                    persist_watcher_relay_watermark_locked_in_root(
                        root.path(),
                        &ProviderKind::Codex,
                        legacy.channel_id,
                        (&identity, &witness),
                        "tmux-5925",
                        WatcherRelayWatermarkPatch {
                            last_watcher_relayed_offset: Some(512),
                            last_watcher_relayed_generation_mtime_ns: Some(77)
                        }
                    ),
                    WatcherRelayWatermarkOutcome::Saved
                ),
            }
            let durable = load(root.path(), legacy.channel_id);
            assert_eq!(
                durable.legacy_replay_coordinate,
                current.legacy_replay_coordinate
            );
            match kind {
                0 => assert_eq!(durable.full_response, "FROZEN BODY TAIL"),
                1 => assert!(durable.terminal_delivery_committed),
                _ => assert_eq!(durable.last_watcher_relayed_offset, Some(512)),
            }
            assert_eq!(durable.output_path, current.output_path);
            if kind != 1 {
                assert_eq!(durable.last_offset, current.last_offset);
            }
        }
    }
}
