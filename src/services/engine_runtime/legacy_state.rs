//! Transitional storage keeps SDK map keys at the adapter boundary.

use poise::serenity_prelude::ChannelId;

pub(crate) struct LegacyState {
    /// Weak entries are pruned by the existing transition-lock lookup.
    pub(in crate::services) session_transition_locks:
        dashmap::DashMap<ChannelId, std::sync::Weak<tokio::sync::Mutex<()>>>,
    pub(in crate::services) core: tokio::sync::Mutex<super::session::CoreState>,
    pub(in crate::services) turn_start_times: dashmap::DashMap<ChannelId, std::time::Instant>,
}
