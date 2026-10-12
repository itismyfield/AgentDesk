//! The bridge tests' handle on intake's own conversion of a provider `Err` into the turn's frame.

use crate::services::agent_protocol::StreamMessage;

/// The frame intake sends the bridge for an executor `Err` outside a managed submission.
pub(in crate::services::discord) fn producer_error_frame(
    channel_id: u64,
    error: String,
) -> Option<StreamMessage> {
    super::producer_error(None, channel_id, error)
}
