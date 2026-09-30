use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
pub(in crate::services::discord::turn_bridge) use crate::services::tui_o::cutover::BodyClaim;
use crate::services::tui_o::cutover::{self, IdentityError};

type Gate = fn(u64, Option<RuntimeHandoffKind>) -> Result<bool, IdentityError>;

/// Direct TUI bodies follow destination membership; uncertain selected identities are held.
/// Only read: a pending adoption stays pending, and a body claims at its transport instead.
pub(in crate::services::discord::turn_bridge) fn bridge_o_body_peek_decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
) -> Result<bool, IdentityError> {
    let gate: Gate = cutover::peek_o_owns_tui_output_for_channel;
    decision(channel_id, inflight, can_deliver_directly, gate)
}

/// The claim a bridge body sends under, with the identity these decisions resolve.
pub(in crate::services::discord::turn_bridge) fn bridge_body_claim(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
) -> BodyClaim<'static> {
    BodyClaim::new(channel_id.get(), kind(channel_id, inflight)).direct(can_deliver_directly)
}

fn kind(channel_id: ChannelId, inflight: &InflightTurnState) -> Option<RuntimeHandoffKind> {
    (inflight.channel_id == channel_id.get())
        .then_some(inflight.runtime_kind)
        .flatten()
}

fn decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
    o_owns: Gate,
) -> Result<bool, IdentityError> {
    let owned = o_owns(channel_id.get(), kind(channel_id, inflight))?;
    if owned && !can_deliver_directly {
        return Err(IdentityError::NonDirectGateway.hold(channel_id.get()));
    }
    Ok(owned)
}

/// A bridge body sent under `claim`; O owning the channel or a held identity sent nothing, which
/// reads as a failed send.
pub(super) async fn claimed_send<T, F: std::future::Future<Output = Result<T, String>>>(
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Result<T, String> {
    cutover::BodySend::flatten(cutover::claim_then_send(claim, send).await)
}

/// The transport's own result, or `None` when O owns the channel or its identity is held and
/// nothing was sent.
pub(in crate::services::discord::turn_bridge) async fn sent_under<T, F>(
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    match cutover::claim_then_send(claim, send).await {
        Ok(cutover::BodySend::Sent(sent)) => Some(sent),
        Ok(cutover::BodySend::OwnedByO) | Err(_) => None,
    }
}
