use super::*;

/// Direct TUI bodies follow destination membership; uncertain selected identities are held.
pub(in crate::services::discord::turn_bridge) fn bridge_o_body_cut_decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
) -> Result<bool, crate::services::tui_o::cutover::IdentityError> {
    let kind = (inflight.channel_id == channel_id.get())
        .then_some(inflight.runtime_kind)
        .flatten();
    let owned =
        crate::services::tui_o::cutover::o_owns_tui_output_for_channel(channel_id.get(), kind)?;
    if owned && !can_deliver_directly {
        return Err(
            crate::services::tui_o::cutover::IdentityError::NonDirectGateway.hold(channel_id.get()),
        );
    }
    Ok(owned)
}
