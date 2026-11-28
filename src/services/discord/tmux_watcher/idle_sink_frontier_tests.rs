use crate::services::tmux_common as tc;
#[test]
fn s2a1_sink_ordered_idle_entry_commits_durably_preserves_noop_and_refuses_old_generation() {
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::cluster::stream_relay::StreamFrame;
    use crate::services::cluster::stream_relay::source_read_proof::tests::seal_with_frontier;
    use crate::services::discord::{
        self,
        health::HealthRegistry,
        session_relay_sink::{SessionBoundDiscordRelaySink, turn_parser::SessionRelayParser},
    };
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;
    use std::sync::Arc;
    let _host = crate::config::pin_runtime_host_for_test();
    let channel = ChannelId::new(59_960_420);
    let provider = ProviderKind::Claude;
    let session = provider.build_tmux_session_name(&channel.get().to_string());
    let output = expected_rollout_path_for(&session);
    std::fs::write(&output, [b'x'; 512]).expect("source");
    let generation =
        crate::services::tmux_common::spawn_markers::stamp_session_generation_marker(&session)
            .expect("generation");
    let shared = discord::make_shared_data_for_tests();
    let coord = shared.tmux_relay_coord(channel);
    let (_source, seal) = seal_with_frontier(&session, 128, (generation, 0));
    assert_eq!(
        format!(
            "{:?}",
            coord.publish_confirmed_for_tests(128, generation, 0, Some(&seal))
        ),
        "Published"
    );
    let delivery = |frame_generation| {
        let frame = StreamFrame {
            session_name: session.clone(), binding: MatchedChannel { channel_id: channel.get().to_string(), agent_id: "s2a1".into(), provider: provider.clone(), expected_session_name: session.clone(), expected_rollout_path: output.clone() },
            payload: "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"idle answer\"}]}}\n{\"type\":\"result\",\"result\":\"idle answer\"}\n".into(),
            sequence: 1, terminal_consumed_end: None, turn_user_msg_id: 0, turn_started_at: String::new(), turn_start_offset: None, relay_range: Some((128,256)), relay_generation_mtime_ns: Some(frame_generation), relay_source_stamp: None, source_span: None, read_source: None,
        };
        SessionRelayParser::default()
            .ingest_frame(&frame)
            .pop()
            .expect("terminal parsed")
    };
    let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
    assert!(!sink.advance_idle_range_after_confirmed_post(
        &shared,
        &provider,
        channel.get(),
        &session,
        &delivery(generation + 1)
    ));
    assert_eq!(coord.frontier_tuple_for_tests(), (128, generation, 0, true));
    let candidate = delivery(generation);
    assert!(!sink.idle_range_already_committed_before_transport(
        &shared,
        &provider,
        channel.get(),
        &session,
        &candidate
    ));
    assert!(sink.advance_idle_range_after_confirmed_post(
        &shared,
        &provider,
        channel.get(),
        &session,
        &candidate
    ));
    assert_eq!(
        coord.frontier_tuple_for_tests(),
        (256, generation, 0, false)
    );
    assert_eq!(
        discord::outbound::delivery_record::delivered_frontier_end_current_generation(
            &provider,
            channel,
            &session,
            Some(512)
        ),
        256
    );
    assert!(sink.idle_range_already_committed_before_transport(
        &shared,
        &provider,
        channel.get(),
        &session,
        &candidate
    ));
    let (_same, seal) = seal_with_frontier(&session, 256, (generation, 0));
    assert_eq!(
        format!(
            "{:?}",
            coord.publish_confirmed_for_tests(256, generation, 0, Some(&seal))
        ),
        "Published"
    );
    assert!(sink.advance_idle_range_after_confirmed_post(
        &shared,
        &provider,
        channel.get(),
        &session,
        &candidate
    ));
    assert_eq!(coord.frontier_tuple_for_tests(), (256, generation, 0, true));
    tc::cleanup_session_temp_files(&session);
}
