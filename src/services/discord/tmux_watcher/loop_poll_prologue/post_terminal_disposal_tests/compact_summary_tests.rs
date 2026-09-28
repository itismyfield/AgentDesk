use super::*;
use crate::services::discord::inflight::load_inflight_state;
use crate::services::discord::outbound::delivery_frontier_probe::delivered_frontier_current_generation;
use serde_json::json;

#[tokio::test]
async fn compact_summary_owned_tail_split_and_rewind_preserve_unreceipted_range() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "ADK_COMPACT_SUMMARY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let tmux = root.path().join("tmux");
        std::fs::write(&tmux, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
        let exact = format!(
            "{}::compact_summary_owned_tail_split_and_rewind_preserve_unreceipted_range",
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
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(63_000_100 + u64::from(std::process::id()));
    let session = format!("compact-{}", channel.get());
    let generation = crate::services::tmux_common::session_temp_path(&session, "generation");
    std::fs::create_dir_all(std::path::Path::new(&generation).parent().unwrap()).unwrap();
    std::fs::write(&generation, "fixture-generation").unwrap();
    let path = crate::services::tmux_common::session_temp_path(&session, "jsonl");
    let before = format!(
        "{}\n{}\n{}\n",
        json!({"type":"user","message":{"role":"user","content":"real prompt"}}),
        json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"HEAD_ONE"},{"type":"text","text":"HEAD_TWO"}]}}),
        json!({"type":"system","subtype":"compact_boundary"})
    );
    let summary = format!(
        "{}\n",
        json!({"type":"user","isCompactSummary":true,
        "isVisibleInTranscriptOnly":true,"isSidechain":false,"uuid":"summary",
        "message":{"role":"user","content":"가".repeat(6_000)}})
    );
    let after = format!(
        "{}\n",
        json!({"type":"assistant","message":{"content":[
        {"type":"text","text":"TAIL_ONE"},{"type":"text","text":"TAIL_TWO"}]}})
    );
    let terminal = format!(
        "{}\n",
        json!({"type":"system","subtype":"stop_hook_summary"})
    );
    let stop_offset = (before.len() + summary.len() + after.len()) as u64;
    let source = format!("{before}{summary}{after}{terminal}");
    assert!(before.len() < 16_384 && before.len() + summary.len() > 16_384);
    std::fs::write(&path, &source).unwrap();
    assert!(reacquire_watcher_inflight_for_active_stream(
        &ProviderKind::Claude,
        channel,
        &session,
        &path,
        0,
        None,
        None,
        None
    ));
    let owner = load_inflight_state(&ProviderKind::Claude, channel.get()).unwrap();
    assert_eq!(
        owner.effective_relay_owner_kind(),
        crate::services::discord::inflight::RelayOwnerKind::Watcher
    );
    assert!(owner.turn_nonce.is_some());
    let original_owner = serde_json::to_value(&owner).unwrap();
    let (mut offset, mut local_end, mut local_generation) = (0, None, None);
    let (mut identity, mut nonce) = (None, None);
    let resume = Arc::new(std::sync::Mutex::new(None));
    let turn_delivered = Arc::new(AtomicBool::new(false));
    let mut terminal_observed = false;
    let mut decoder = Utf8ChunkDecoder::default();
    let mut retained_source = None;
    let mut activity = None;
    let mut buffer = String::new();
    let mut body = String::new();
    let mut state = StreamLineState::new();
    let mut tool = WatcherToolState::new();
    let mut final_bodies = Vec::new();
    for pass in 0..2 {
        if pass == 1 {
            *resume.lock().unwrap() = Some(0);
            buffer.clear();
            body.clear();
            decoder = Utf8ChunkDecoder::default();
            state = StreamLineState::new();
            tool = WatcherToolState::new();
        }
        for chunk in 0..2 {
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
                        watcher_instance_id: 6300,
                    },
                    &PollWatcherControls {
                        cancel: &Arc::new(AtomicBool::new(false)),
                        paused: &Arc::new(AtomicBool::new(false)),
                        resume_offset: &resume,
                        pause_epoch: &Arc::new(AtomicU64::new(0)),
                        turn_delivered: &turn_delivered,
                        last_heartbeat_ts_ms: &Arc::new(AtomicI64::new(0)),
                        jsonl_notify: &Arc::new(tokio::sync::Notify::new()),
                        dead_marker_notify: &Arc::new(tokio::sync::Notify::new()),
                    },
                    &mut RelayOffsetState {
                        current_offset: &mut offset,
                        terminal_delivery_observed: &mut terminal_observed,
                        last_relayed_offset: &mut local_end,
                        last_observed_generation_mtime_ns: &mut local_generation,
                        rotation_tick: &mut 0,
                        watcher_turn_identity: &mut identity,
                        watcher_turn_nonce: &mut nonce,
                    },
                    &mut LoopPollState {
                        retained_source: &mut retained_source,
                        prompt_too_long_killed: false,
                        all_data: &buffer,
                        utf8_decoder: &mut decoder,
                        completion_footer_idle: &mut WatcherCompletionFooterIdleState::default(),
                        last_activity_heartbeat_at: &mut activity,
                    },
                    &mut PostTerminalState {
                        turn_result_relayed: false,
                        post_terminal_continuation_logged: &mut false,
                        last_post_terminal_suppressed_range: &mut None,
                        active_stream_inflight_reacquire_logged: &mut false,
                        restored_turn: &None,
                        restored_injected_prompt_message_id: None,
                    },
                ),
            )
            .await
            .unwrap();
            let PollOutcome::OutputReady {
                data,
                data_start_offset,
                source_authority,
                ..
            } = result
            else {
                panic!("tail must retain unreceipted bytes: {result:?}");
            };
            assert_eq!(data_start_offset, if chunk == 0 { 0 } else { 16_384 });
            assert_eq!(
                offset,
                if chunk == 0 {
                    16_384
                } else {
                    source.len() as u64
                }
            );
            let decoded = decoder.decode_source(&data, data_start_offset, source_authority);
            let buffer_start = decoded.start_offset.unwrap() - buffer.len() as u64;
            buffer.push_str(&decoded.text);
            let outcome = process_watcher_lines_for_turn(
                &mut buffer,
                &mut state,
                &mut body,
                &mut tool,
                Some(buffer_start),
                Some(0),
            );
            assert_eq!(
                outcome.terminal_kind,
                (chunk == 1).then_some(WatcherTerminalKind::SoftStopHookSummary)
            );
            assert_eq!(
                outcome.terminal_evidence_offset,
                (chunk == 1).then_some(stop_offset)
            );
            assert_eq!(local_end, None, "read/summary/rewind is not delivery");
            assert!(!terminal_observed);
            assert_eq!(shared.committed_relay_offset(channel), 0);
            assert!(
                delivered_frontier_current_generation(
                    &ProviderKind::Claude,
                    channel,
                    &session,
                    Some(source.len() as u64)
                )
                .is_none()
            );
            assert_eq!(
                serde_json::to_value(
                    load_inflight_state(&ProviderKind::Claude, channel.get()).unwrap()
                )
                .unwrap(),
                original_owner
            );
            assert_eq!(nonce, owner.turn_nonce);
            if chunk == 0 {
                assert_eq!(body.matches("HEAD_ONE").count(), 1);
                assert!(!body.contains("TAIL_ONE"));
                assert!(!buffer.is_empty());
            }
        }
        assert!(buffer.is_empty());
        for text in ["HEAD_ONE", "HEAD_TWO", "TAIL_ONE", "TAIL_TWO"] {
            assert_eq!(body.matches(text).count(), 1, "pass={pass}, body={body:?}");
        }
        final_bodies.push(body.clone());
    }
    assert_eq!(final_bodies[0], final_bodies[1]);
}
