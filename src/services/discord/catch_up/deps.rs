use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use poise::serenity_prelude::ChannelId;

use super::api::CatchUpDiscordApi;
use super::clock::{CatchUpClock, SYSTEM_CLOCK};
use super::{CatchUpRetryState, SharedData};
use crate::services::provider::ProviderKind;

// Retry inputs and the clock share the sweep's lifetime.
pub(super) struct CatchUpDeps<'a, A: CatchUpDiscordApi + ?Sized> {
    pub(super) api: &'a A,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) retry_checkpoints: &'a HashMap<ChannelId, CatchUpRetryState>,
    pub(super) pending_retry_channels: &'a HashSet<ChannelId>,
    pub(super) clock: &'a dyn CatchUpClock,
}

static EMPTY_RETRY_CHECKPOINTS: LazyLock<HashMap<ChannelId, CatchUpRetryState>> =
    LazyLock::new(HashMap::new);
static EMPTY_PENDING_RETRY_CHANNELS: LazyLock<HashSet<ChannelId>> = LazyLock::new(HashSet::new);

impl<'a, A: CatchUpDiscordApi + ?Sized> CatchUpDeps<'a, A> {
    pub(super) fn new(api: &'a A, shared: &'a Arc<SharedData>, provider: &'a ProviderKind) -> Self {
        Self {
            api,
            shared,
            provider,
            retry_checkpoints: &EMPTY_RETRY_CHECKPOINTS,
            pending_retry_channels: &EMPTY_PENDING_RETRY_CHANNELS,
            clock: &SYSTEM_CLOCK,
        }
    }

    pub(super) fn with_retry_checkpoints(
        mut self,
        retry_checkpoints: &'a HashMap<ChannelId, CatchUpRetryState>,
    ) -> Self {
        self.retry_checkpoints = retry_checkpoints;
        self
    }

    pub(super) fn with_pending_retry_channels(
        mut self,
        pending_retry_channels: &'a HashSet<ChannelId>,
    ) -> Self {
        self.pending_retry_channels = pending_retry_channels;
        self
    }

    #[cfg(test)]
    pub(super) fn with_clock(mut self, clock: &'a dyn CatchUpClock) -> Self {
        self.clock = clock;
        self
    }
}
