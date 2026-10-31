use super::*;

#[test]
fn headless_status_and_turn_identity_pass_through_the_handoff_response() {
    for (outcome, status) in [
        (
            HeadlessTurnStartOutcome::accepted("discord:111:222".into()),
            "accepted",
        ),
        (
            HeadlessTurnStartOutcome::started("discord:111:222".into(), None),
            "started",
        ),
        (
            HeadlessTurnStartOutcome::consumed("discord:111:222".into()),
            "consumed",
        ),
    ] {
        let target = AgentHandoffTurnTarget {
            to_agent_id: "adk-dashboard".into(),
            channel_id: "111".into(),
            channel_id_num: 111,
            channel_kind: AgentHandoffChannelKind::Cc,
            provider: ProviderKind::Claude,
            content: "envelope".into(),
        };
        let response = map_turn_start_result(&target, Ok(outcome))
            .unwrap()
            .to_value();
        assert_eq!(
            response,
            serde_json::json!({
                "ok": true,
                "to_agent_id": "adk-dashboard",
                "channel_id": "111",
                "channel_kind": "cc",
                "turn_id": "discord:111:222",
                "status": status,
            })
        );
    }
}
