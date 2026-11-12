use super::*;

#[test]
fn i8117_real_headless_start_pins_only_routine_process_policy_pg() {
    let name = concat!(
        module_path!(),
        "::i8117_real_headless_start_pins_only_routine_process_policy_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channels = [ChannelId::new(8_117_101), ChannelId::new(8_117_102), ChannelId::new(8_117_103)];
        let host = Host::new(&channels).await;
        for (channel, source, metadata, policy) in [
            (channels[0], "routine", serde_json::json!({"routine_id":"fixture","routine_run_id":"fixture-run","execution_strategy":"fresh","execution_mode":"process"}), "claude_process_c1"),
            (channels[1], "api", serde_json::json!({"execution_strategy":"fresh","execution_mode":"process"}), "legacy"),
            (channels[2], "scheduled_message", serde_json::json!({"scheduled_message_id":"fixture-schedule"}), "legacy"),
        ] {
            let reservation = health::reserve_headless_agent_turn(channel);
            host.start(channel, source, Some(metadata), reservation).await.unwrap();
            let row = crate::services::discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
            assert_eq!(serde_json::to_value(row).unwrap()["routine_presentation"]["policy"], policy);
        }
        host.close().await;
    });
}
