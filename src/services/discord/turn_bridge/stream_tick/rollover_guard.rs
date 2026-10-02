//! Provider-output guard on the bridge rollover freeze edit.

use super::super::*;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send};

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
            let freeze =
                || TurnGateway::edit_message(gateway, channel_id, message_id, frozen_chunk);
            match claim_then_send(claim, freeze).await {
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
            TurnGateway::edit_message(gateway, channel_id, message_id, safe_blocked_body(kind))
                .await
                .map(|()| GuardedRolloverEditOutcome::Blocked)
        }
    }
}
