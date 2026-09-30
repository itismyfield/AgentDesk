use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::cutover::{self, IdentityError};

type Gate = fn(u64, Option<RuntimeHandoffKind>) -> Result<bool, IdentityError>;

/// Direct TUI bodies follow destination membership; uncertain selected identities are held.
/// For a caller about to send a body: a pending adoption is released to Legacy first.
pub(in crate::services::discord::turn_bridge) fn bridge_o_body_cut_decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
) -> Result<bool, IdentityError> {
    let gate: Gate = cutover::o_owns_tui_output_for_channel;
    decision(channel_id, inflight, can_deliver_directly, gate)
}

/// The same decision for a caller with no body to send; a pending adoption stays pending.
pub(in crate::services::discord::turn_bridge) fn bridge_o_body_peek_decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
) -> Result<bool, IdentityError> {
    let gate: Gate = cutover::peek_o_owns_tui_output_for_channel;
    decision(channel_id, inflight, can_deliver_directly, gate)
}

fn decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
    o_owns: Gate,
) -> Result<bool, IdentityError> {
    let kind = (inflight.channel_id == channel_id.get())
        .then_some(inflight.runtime_kind)
        .flatten();
    let owned = o_owns(channel_id.get(), kind)?;
    if owned && !can_deliver_directly {
        return Err(IdentityError::NonDirectGateway.hold(channel_id.get()));
    }
    Ok(owned)
}
