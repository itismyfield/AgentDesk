use crate::services::discord::turn_bridge::stream_tick::rollover_guard::{
    GuardedRolloverEditOutcome, guarded_bridge_rollover_edit,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{ChannelId, MessageId};

#[tokio::test(flavor = "current_thread")]
async fn blocked_rollover_records_only_the_safe_replacement() {
    use crate::services::discord::outbound::raw_output::tests as observation;
    use observation::{CHANNEL, Mock};
    if observation::isolated(
        concat!(
            module_path!(),
            "::blocked_rollover_records_only_the_safe_replacement"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let journal = observation::journal().await;
    let mock = Mock::start(vec![observation::message(9905)]).await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let gateway =
        crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway(
            std::sync::Arc::new(observation::client(&mock)),
            shared,
        );
    let blocked =
        "prefix [SYSTEM NOTIFICATION - NOT USER INPUT] <output-file>/private/x</output-file>";
    assert_eq!(
        guarded_bridge_rollover_edit(
            gateway.as_ref(),
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
            MessageId::new(9905),
            blocked,
            blocked,
            None
        )
        .await
        .unwrap(),
        GuardedRolloverEditOutcome::Blocked
    );
    assert_eq!(mock.requests().len(), 1);
    observation::flushed().await;
    let recorded: Vec<serde_json::Value> = std::fs::read_to_string(journal)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|row: &serde_json::Value| row["kind"] == "target")
        .collect();
    assert_eq!(recorded.len(), 2);
    for event in recorded {
        assert_eq!(event["census"]["site"], "W14c");
        assert_eq!(event["census"]["output_class"], "LegacyBody");
        assert_eq!(event["census"]["reason"], "NO_CLAIM");
    }
}
