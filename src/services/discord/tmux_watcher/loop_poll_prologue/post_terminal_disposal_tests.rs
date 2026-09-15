use super::*;
use crate::services::discord::make_shared_data_for_tests;

// PATH/root isolation follows continuation_marker_tests; no live tmux or transport.
#[cfg(unix)]
#[tokio::test]
async fn suppressed_poll_keeps_shared_delivery_frontier() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "ADK_5755_DISPOSAL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let tmux = root.path().join("tmux");
        std::fs::write(&tmux, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
        let exact = format!(
            "{}::suppressed_poll_keeps_shared_delivery_frontier",
            module_path!().split_once("::").unwrap().1
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .env("PATH", root.path())
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed; 0 failed"));
        return;
    }
    let payload = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"늦은 결과\"}\n";
    let end = payload.len() as u64;
    for (index, (case, range, stale, carried, pending_utf8)) in [
        ("no receipt", None, false, "", false),
        ("covered", Some((0, end)), false, "", false),
        ("stale generation", Some((0, end)), true, "", false),
        ("uncovered end", Some((0, end - 1)), false, "", false),
        ("uncovered start", Some((1, end)), false, "", false),
        ("empty range", Some((end, end)), false, "", false),
        ("reversed range", Some((end, 0)), false, "", false),
        (
            "carried fragment",
            Some((0, end)),
            false,
            "{\"type\":\"result\",\"result\":\"",
            false,
        ),
        ("pending UTF8", Some((0, end)), false, "", true),
    ]
    .into_iter()
    .enumerate()
    {
        let disk_payload = if pending_utf8 {
            "한".to_owned()
        } else if !carried.is_empty() {
            format!("{carried}다음 결과\"}}\n")
        } else {
            payload.to_owned()
        };
        let start = if pending_utf8 {
            1
        } else {
            carried.len() as u64
        };
        let end = disk_payload.len() as u64;
        let range = if pending_utf8 || !carried.is_empty() {
            range.map(|_| (0, end))
        } else {
            range
        };
        let shared = make_shared_data_for_tests();
        let channel =
            ChannelId::new(57_550_000 + u64::from(std::process::id()) * 10 + index as u64);
        let session = format!("disposal-{}", channel.get());
        let generation = crate::services::tmux_common::session_temp_path(&session, "generation");
        std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
        std::fs::write(&generation, "fixture-generation").unwrap();
        let generation_mtime = read_generation_file_mtime_ns(&session);
        assert_ne!(generation_mtime, 0);
        let path = crate::services::tmux_common::session_temp_path(&session, "jsonl");
        std::fs::write(&path, &disk_payload).unwrap();
        use crate::services::discord::outbound::delivery_record::{
            DeliveredCommit, DeliveryRecord, delivery_record_path,
        };
        if let Some(range) = range {
            let record = DeliveryRecord {
                delivered_frontier: Some(DeliveredCommit {
                    range,
                    generation_mtime_ns: if stale { 11 } else { generation_mtime },
                    attempts: 1,
                    panel_msg_id: Some(5755),
                    panel_channel_id: Some(channel.get()),
                }),
                ..Default::default()
            };
            let record_path = delivery_record_path(&ProviderKind::Claude, channel.get()).unwrap();
            std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
            std::fs::write(record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        }
        let coord = shared.tmux_relay_coord(channel);
        coord.confirmed_end_offset.store(7, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(11, Ordering::Release);
        let (mut offset, mut local_end, mut local_generation) = (start, Some(7), Some(11));
        let mut suppressed = None;
        let carried = carried.to_owned();
        let mut decoder = Utf8ChunkDecoder::default();
        if pending_utf8 {
            let decoded = decoder.decode_source(
                &"한".as_bytes()[..1],
                0,
                WatcherSourceAuthority {
                    source_file:
                        crate::services::cluster::stream_relay::SourceFileIdentity::Unavailable,
                    generation_mtime_ns: generation_mtime,
                    reset_incarnation: 0,
                    source_stamp: None,
                },
            );
            assert!(decoded.text.is_empty());
            assert!(decoder.has_pending());
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            poll_watcher_output_or_continue(
                &PollWatcherContext {
                    http: &Arc::new(serenity::Http::new("fixture-no-network")),
                    shared: &shared,
                    channel_id: channel,
                    watcher_provider: &ProviderKind::Claude,
                    tmux_session_name: &session,
                    output_path: &path,
                    watcher_thread_channel_id: None,
                    watcher_instance_id: 5755,
                },
                &PollWatcherControls {
                    cancel: &Arc::new(AtomicBool::new(false)),
                    paused: &Arc::new(AtomicBool::new(false)),
                    resume_offset: &Arc::new(std::sync::Mutex::new(None)),
                    pause_epoch: &Arc::new(AtomicU64::new(0)),
                    turn_delivered: &Arc::new(AtomicBool::new(false)),
                    last_heartbeat_ts_ms: &Arc::new(AtomicI64::new(0)),
                    jsonl_notify: &Arc::new(tokio::sync::Notify::new()),
                    dead_marker_notify: &Arc::new(tokio::sync::Notify::new()),
                },
                &mut RelayOffsetState {
                    current_offset: &mut offset,
                    terminal_delivery_observed: &mut true,
                    last_relayed_offset: &mut local_end,
                    last_observed_generation_mtime_ns: &mut local_generation,
                    rotation_tick: &mut 0,
                    watcher_turn_identity: &mut None,
                    watcher_turn_nonce: &mut None,
                },
                &mut LoopPollState {
                    retained_source: &mut None,
                    prompt_too_long_killed: false,
                    all_data: &carried,
                    utf8_decoder: &mut decoder,
                    completion_footer_idle: &mut WatcherCompletionFooterIdleState::default(),
                    last_activity_heartbeat_at: &mut None,
                },
                &mut PostTerminalState {
                    turn_result_relayed: true,
                    post_terminal_continuation_logged: &mut false,
                    last_post_terminal_suppressed_range: &mut suppressed,
                    active_stream_inflight_reacquire_logged: &mut false,
                    restored_turn: &None,
                    restored_injected_prompt_message_id: None,
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(offset, end, "{case}: read position advances without rewind");
        if case == "covered" {
            assert_eq!(result, PollOutcome::ContinueWatcherLoop, "{case}");
            assert_eq!(
                (local_end, local_generation),
                (Some(end), Some(generation_mtime))
            );
            assert_eq!(
                suppressed,
                Some((0, end)),
                "receipt-backed disposal reached"
            );
        } else {
            let PollOutcome::OutputReady {
                data,
                data_start_offset,
                source_authority,
                ..
            } = result
            else {
                panic!("{case}: unproven output was discarded: {result:?}");
            };
            assert_eq!(
                data,
                &disk_payload.as_bytes()[start as usize..],
                "{case}: all unread bytes survive"
            );
            assert_eq!(data_start_offset, start);
            assert_eq!(source_authority.generation_mtime_ns, generation_mtime);
            assert_eq!((local_end, local_generation), (Some(7), Some(11)), "{case}");
            assert_eq!(suppressed, None, "{case}: unknown is not settled");
            if pending_utf8 {
                assert!(decoder.has_pending(), "poll retained the incomplete scalar");
                assert_eq!(
                    decoder
                        .decode_source(&data, data_start_offset, source_authority)
                        .text,
                    "한",
                    "the next decode recovers the original scalar without replacement bytes"
                );
            } else if !carried.is_empty() {
                let complete = format!("{carried}{}", String::from_utf8(data).unwrap());
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&complete).unwrap()["result"],
                    "다음 결과",
                    "carried JSON finishes on the ordinary decode path"
                );
            }
        }
        assert_eq!(
            (
                coord.confirmed_end_offset.load(Ordering::Acquire),
                coord
                    .confirmed_end_generation_mtime_ns
                    .load(Ordering::Acquire)
            ),
            (7, 11),
            "suppression must not manufacture delivery authority"
        );
    }
}

// Pure downstream carry-forward is separate from the runtime arm witness.
#[test]
fn carry_forward_uses_proven_frontier_not_local_consumption() {
    use crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward;
    assert_eq!(synthetic_start_offset_carry_forward(3, Some(7)), 7);
    assert_eq!(synthetic_start_offset_carry_forward(80, Some(7)), 80);
}
