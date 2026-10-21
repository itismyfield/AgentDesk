//! Exact inputs discarded by a clear stay refused across actor reincarnations.
use std::sync::Arc;

use super::{ChannelMailboxState, Intervention, remint_fence::FenceCell};
use poise::serenity_prelude::MessageId;

#[derive(Debug, Default)]
pub(crate) struct DispatchLease {
    fence: Option<FenceCell>,
    sources: Vec<MessageId>,
}

impl DispatchLease {
    pub(super) fn new(fence: FenceCell, sources: Vec<MessageId>) -> Arc<Self> {
        Arc::new(Self {
            fence: Some(fence),
            sources,
        })
    }

    pub(crate) fn superseded_by_clear(&self) -> bool {
        self.fence
            .as_ref()
            .is_some_and(|fence| fence.refuses_discarded(&self.sources))
    }
}

pub(super) fn refuses(state: &ChannelMailboxState, item: &Intervention) -> bool {
    let sources: Vec<_> = item
        .source_message_ids
        .iter()
        .copied()
        .chain([item.message_id])
        .collect();
    state.remint_fence.refuses_discarded(&sources)
}

#[cfg(test)]
#[path = "clear_fence/clear_fence_tests.rs"]
mod tests;
