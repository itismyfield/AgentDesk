use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;

#[derive(Debug, Default)]
pub(super) struct PanelCacheInvalidations {
    next_epoch: AtomicU64,
    by_panel: DashMap<(ChannelId, u64), u64>,
}

impl PanelCacheInvalidations {
    pub(super) fn invalidate(&self, channel_id: ChannelId, message_id: u64) {
        if message_id == 0 {
            return;
        }
        let epoch = self
            .next_epoch
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.by_panel.insert((channel_id, message_id), epoch);
    }

    pub(super) fn current_epoch(&self, channel_id: ChannelId, message_id: u64) -> Option<u64> {
        if message_id == 0 {
            return None;
        }
        self.by_panel
            .get(&(channel_id, message_id))
            .map(|entry| *entry.value())
    }

    pub(super) fn is_pending(&self, channel_id: ChannelId, message_id: u64) -> bool {
        message_id != 0 && self.by_panel.contains_key(&(channel_id, message_id))
    }

    pub(super) fn clear_if_epoch(
        &self,
        channel_id: ChannelId,
        message_id: u64,
        epoch: u64,
    ) -> bool {
        if message_id == 0 || epoch == 0 {
            return false;
        }
        self.by_panel
            .remove_if(&(channel_id, message_id), |_, current| *current == epoch)
            .is_some()
    }

    pub(super) fn clear_channel(&self, channel_id: ChannelId) {
        self.by_panel
            .retain(|(entry_channel, _), _| *entry_channel != channel_id);
    }
}

impl super::PlaceholderLiveEvents {
    pub(in crate::services::discord) fn invalidate_panel_cache(
        &self,
        channel_id: ChannelId,
        message_id: u64,
    ) {
        self.panel_cache_invalidations
            .invalidate(channel_id, message_id);
    }

    pub(in crate::services::discord) fn panel_cache_invalidation_pending(
        &self,
        channel_id: ChannelId,
        message_id: u64,
    ) -> bool {
        self.panel_cache_invalidations
            .is_pending(channel_id, message_id)
    }

    pub(in crate::services::discord) fn panel_cache_invalidation_epoch(
        &self,
        channel_id: ChannelId,
        message_id: u64,
    ) -> Option<u64> {
        self.panel_cache_invalidations
            .current_epoch(channel_id, message_id)
    }

    pub(in crate::services::discord) fn clear_panel_cache_invalidation_if_epoch(
        &self,
        channel_id: ChannelId,
        message_id: u64,
        epoch: u64,
    ) -> bool {
        self.panel_cache_invalidations
            .clear_if_epoch(channel_id, message_id, epoch)
    }
}
