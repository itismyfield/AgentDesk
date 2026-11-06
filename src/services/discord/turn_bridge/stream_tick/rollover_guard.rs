//! Provider-output guard on the bridge rollover freeze edit.

use super::super::*;
use crate::services::discord::outbound::body_census::{BodyOffer, BodySource};
use crate::services::tui_o::cutover::{BodyClaim, BodySend, BodySite, claim_then_send_at};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GuardedRolloverEditOutcome {
    Clean,
    Held,
    Blocked,
}

pub(super) async fn guarded_bridge_rollover_edit<G: TurnGateway + ?Sized>(
    gateway: &G,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    unsent_response: &str,
    frozen_chunk: &str,
    claim: Option<BodyClaim<'_>>,
) -> Result<GuardedRolloverEditOutcome, String> {
    use crate::services::provider_output_guard::{
        ProviderOutputVerdict, inspect_provider_streaming_rollover, safe_blocked_body,
    };

    match inspect_provider_streaming_rollover(provider, unsent_response, frozen_chunk) {
        ProviderOutputVerdict::Clean => {
            let freeze = || {
                BodySource::inherited().observe(
                    BodySite::BridgeRollover,
                    BodyOffer::Body,
                    TurnGateway::edit_message(gateway, channel_id, message_id, frozen_chunk),
                )
            };
            match BodySource::inherited()
                .observe(
                    BodySite::BridgeRollover,
                    BodyOffer::Body,
                    claim_then_send_at(BodySite::BridgeRollover, claim, freeze),
                )
                .await
            {
                Ok(BodySend::Sent(edit)) => edit.map(|()| GuardedRolloverEditOutcome::Clean),
                // O took the channel or its identity is held: keep the frame, like a held one.
                Ok(BodySend::OwnedByO) | Err(_) => Ok(GuardedRolloverEditOutcome::Held),
            }
        }
        ProviderOutputVerdict::Hold { kind } => {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                verdict = "hold",
                kind = kind.as_str(),
                output_bytes = frozen_chunk.len(),
                output_chars = frozen_chunk.chars().count(),
                "held turn-bridge streaming rollover frame"
            );
            Ok(GuardedRolloverEditOutcome::Held)
        }
        ProviderOutputVerdict::Blocked { kind } => {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                verdict = "blocked",
                kind = kind.as_str(),
                output_bytes = frozen_chunk.len(),
                output_chars = frozen_chunk.chars().count(),
                "blocked turn-bridge streaming rollover frame"
            );
            BodySource::inherited()
                .reason(crate::services::tui_o::channel_policy::ClaimReason::NoClaim)
                .observe(
                    BodySite::BridgeBlockedRollover,
                    BodyOffer::Body,
                    TurnGateway::edit_message(
                        gateway,
                        channel_id,
                        message_id,
                        safe_blocked_body(kind),
                    ),
                )
                .await
                .map(|()| GuardedRolloverEditOutcome::Blocked)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let gateway = crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway(
            std::sync::Arc::new(observation::client(&mock)), shared);
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
}
