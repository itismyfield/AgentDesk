use super::super::*;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::cluster::stream_relay::{
    GenerationSourceIdentity, SourceEpoch, SourceFileIdentity, SourceStamp, SourceWitness,
};

pub(in crate::services::discord) fn frame_for(
    state: &InflightTurnState,
    payload: String,
) -> StreamFrame {
    let session = state.tmux_session_name.clone().unwrap();
    let path = state.output_path.clone().unwrap();
    let generation = dr::current_generation_mtime_ns(&session);
    let file = std::fs::File::open(&path).unwrap();
    let mut generation_identity = GenerationSourceIdentity::Unsupported {
        mtime_ns: generation,
    };
    #[cfg(unix)]
    if let Some(marker) =
        crate::services::tmux_common::resolve_session_temp_path(&session, "generation")
        && let Ok(marker_file) = std::fs::File::open(marker)
        && let SourceFileIdentity::Unix { dev, ino } =
            SourceFileIdentity::from_open_file(&marker_file)
    {
        generation_identity = GenerationSourceIdentity::Unix {
            mtime_ns: generation,
            dev,
            ino,
        };
    }
    let stamp = SourceStamp {
        epoch: SourceEpoch::from_observation(1),
        file: SourceFileIdentity::from_open_file(&file),
        witness: SourceWitness {
            generation: Some(generation_identity),
            spawn_nonce_hash: None,
        },
    };
    StreamFrame {
        session_name: session.clone(),
        binding: MatchedChannel {
            channel_id: state.channel_id.to_string(),
            agent_id: "fixture".into(),
            provider: ProviderKind::Codex,
            expected_session_name: session,
            expected_rollout_path: path,
        },
        payload,
        sequence: 1,
        terminal_consumed_end: None,
        turn_user_msg_id: state.user_msg_id,
        turn_started_at: state.started_at.clone(),
        turn_start_offset: state.turn_start_offset,
        relay_range: None,
        relay_generation_mtime_ns: Some(generation),
        relay_source_stamp: Some(stamp),
        source_span: None,
    }
}

pub(in crate::services::discord) async fn sink(
    shared: Arc<crate::services::discord::SharedData>,
) -> SessionBoundDiscordRelaySink {
    let registry = Arc::new(HealthRegistry::new());
    registry.register("codex".into(), shared).await;
    let sink = SessionBoundDiscordRelaySink::new(registry);
    sink.enable_delivery_for_test();
    sink
}

fn text(value: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":value}]}})
    )
}

#[test]
fn source_transition_folds_first_frame_and_ignores_observer_epoch_order() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let file = root.path().join("output");
        std::fs::write(&file, b"").unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            5925111,
            None,
            7,
            11,
            12,
            "fixture".into(),
            None,
            Some("source-fixture".into()),
            Some(file.to_string_lossy().into()),
            None,
            0,
        );
        state.turn_start_offset = Some(0);
        for replacement in [false, true] {
            let sink = sink(crate::services::discord::make_shared_data_for_tests()).await;
            let mut first = frame_for(&state, text("OLD"));
            first.relay_generation_mtime_ns = None;
            first.relay_source_stamp = None;
            assert_eq!(
                sink.deliver(&first).await.unwrap(),
                RelaySinkOutcome::FrameAccepted
            );
            let mut next = frame_for(&state, text("NEW"));
            next.relay_generation_mtime_ns = Some(9);
            if replacement {
                let mut old = next.clone();
                old.payload = text("OLDER");
                old.relay_source_stamp.as_mut().unwrap().epoch = SourceEpoch::from_observation(999);
                assert_eq!(
                    sink.deliver(&old).await.unwrap(),
                    RelaySinkOutcome::FrameAccepted
                );
                next.relay_source_stamp.as_mut().unwrap().file = SourceFileIdentity::Unavailable;
            }
            assert_eq!(
                sink.deliver(&next).await.unwrap(),
                RelaySinkOutcome::FrameAccepted
            );
            let mut terminal = next.clone();
            terminal.payload = "{\"type\":\"result\",\"result\":\"\"}\n".into();
            let deliveries = sink
                .by_session
                .lock()
                .unwrap()
                .get_mut("source-fixture")
                .unwrap()
                .ingest_frame(&terminal);
            assert_eq!(deliveries.len(), 1);
            assert_eq!(deliveries[0].response_text, "NEW");
        }
        let mut parser = SessionRelayParser::default();
        let mut first = frame_for(&state, text("A"));
        first.relay_source_stamp.as_mut().unwrap().epoch = SourceEpoch::from_observation(999);
        assert!(parser.ingest_frame(&first).is_empty());
        let mut next = first.clone();
        next.payload = text("B");
        next.relay_source_stamp.as_mut().unwrap().epoch = SourceEpoch::from_observation(1);
        assert!(parser.ingest_frame(&next).is_empty());
        next.payload = "{\"type\":\"result\",\"result\":\"\"}\n".into();
        assert_eq!(parser.ingest_frame(&next)[0].response_text, "AB");
    });
}

#[cfg(unix)]
#[test]
fn stale_marked_frame_is_refused_before_parser_and_committed_range_shortcut() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let session = "source-marked-fixture";
        let path = crate::services::tmux_common::session_temp_path(session, "jsonl");
        std::fs::write(&path, b"source\n").unwrap();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(session, "generation"),
            b"fixture",
        )
        .unwrap();
        let mtime = dr::current_generation_mtime_ns(session);
        let SourceFileIdentity::Unix { dev, ino } =
            SourceFileIdentity::from_open_file(&std::fs::File::open(&path).unwrap())
        else {
            panic!("regular source expected")
        };
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            5925112,
            None,
            7,
            11,
            12,
            "fixture".into(),
            Some("s1".into()),
            Some(session.into()),
            Some(path.clone()),
            None,
            0,
        );
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
        state.native_turn_id = Some("cur".into());
        state.turn_nonce = Some("nonce".into());
        state.full_response = "S".into();
        state.turn_start_offset = Some(0);
        inflight::save_inflight_state(&state).unwrap();
        state = inflight::load_inflight_state(&ProviderKind::Codex, state.channel_id).unwrap();
        let mut locked = inflight::lock_inflight_episode(
            &ProviderKind::Codex,
            state.channel_id,
            &inflight::InflightEpisodePin::from_state(&state),
        )
        .unwrap();
        assert_eq!(
            locked.begin_coordinate_preparation(&state, "install".into()),
            inflight::GuardedSaveOutcome::Saved
        );
        let preparing = locked.state().clone();
        let raw = inflight::RawTurnWitness {
            path: "fixture".into(),
            file_identity: (dev, ino),
            session_id: "s1".into(),
            native_turn_id: "cur".into(),
            turn_start: 0,
            prefix_record_end: 2,
            prefix_len: 1,
        };
        assert_eq!(
            locked.attach_coordinate_proof(&preparing, "install".into(), raw),
            inflight::GuardedSaveOutcome::Saved
        );
        let proven = locked.state().clone();
        assert_eq!(
            locked.install_coordinate_source(&proven, "install".into(), path, (dev, ino), mtime),
            inflight::GuardedSaveOutcome::Saved
        );
        state = locked.state().clone();
        drop(locked);
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared
            .tmux_relay_coord(ChannelId::new(state.channel_id))
            .confirmed_end_offset
            .store(7, Ordering::Release);
        shared.tmux_relay_coord(ChannelId::new(state.channel_id))
            .confirmed_end_generation_mtime_ns.store(mtime, Ordering::Release);
        let sink = sink(shared).await;
        let before = serde_json::to_vec(
            &inflight::load_inflight_state_read_only(&ProviderKind::Codex, state.channel_id)
                .unwrap(),
        )
        .unwrap();
        for ranged in [false, true] {
            let mut stale = frame_for(&state, text("STALE"));
            stale.relay_source_stamp.as_mut().unwrap().file = SourceFileIdentity::Unavailable;
            if ranged {
                stale.payload = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"cur\"}}\n".into();
                stale.relay_range = Some((0, 7));
            }
            assert_eq!(
                sink.deliver(&stale).await.unwrap(),
                RelaySinkOutcome::TerminalNotDelivered
            );
            assert_eq!(sink.frames_total.load(Ordering::Acquire), 0);
            assert!(sink.by_session.lock().unwrap().is_empty());
        }
        let current = frame_for(&state, text("CURRENT"));
        assert_eq!(
            sink.deliver(&current).await.unwrap(),
            RelaySinkOutcome::FrameAccepted
        );
        assert_eq!(sink.frames_total.load(Ordering::Acquire), 1);
        assert_eq!(
            serde_json::to_vec(
                &inflight::load_inflight_state_read_only(&ProviderKind::Codex, state.channel_id)
                    .unwrap()
            )
            .unwrap(),
            before
        );
    });
}
