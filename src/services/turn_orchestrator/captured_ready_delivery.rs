use std::sync::Arc;

use super::{ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxState, MailboxUnreachable};
use crate::services::provider::CancelToken;

/// A captured recovery write with its own typed reply.
pub(crate) trait CapturedReadyDeliveryWrite: Send {
    fn expected_actor(&self) -> Option<&Arc<CancelToken>>;
    fn commit(self: Box<Self>, current: Option<&Arc<CancelToken>>);
}

impl ChannelMailboxHandle {
    pub(crate) fn enqueue_captured_ready_delivery(
        &self,
        commit: Box<dyn CapturedReadyDeliveryWrite>,
    ) -> Result<(), MailboxUnreachable> {
        self.sender
            .send(ChannelMailboxMsg::CommitCapturedReadyDelivery { commit })
            .map_err(|_| MailboxUnreachable)
    }
}

pub(super) fn commit(state: &ChannelMailboxState, write: Box<dyn CapturedReadyDeliveryWrite>) {
    if state.closed {
        return;
    }
    let current = state.cancel_token.as_ref();
    let matches = match (write.expected_actor(), current) {
        (Some(expected), Some(current)) => Arc::ptr_eq(expected, current),
        (None, None) => true,
        _ => false,
    };
    if matches {
        write.commit(current);
    }
}
