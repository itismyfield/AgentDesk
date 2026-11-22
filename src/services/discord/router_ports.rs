//! Router entry points the Discord runtime reaches through `SharedData`; the bootstrap installs
//! the router's own functions so this side never names the router module.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

use super::super::SharedData;

/// The hosted-TUI promote gate's future, borrowed from its arguments.
pub(in crate::services::discord) type PromoteBlockedFuture<'a> =
    Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

#[derive(Clone, Copy)]
pub(in crate::services::discord) struct RouterPorts {
    /// Wakes the channel's owner drive.
    #[cfg(unix)]
    pub owner_wake: fn(&Arc<SharedData>, ChannelId),
    /// Whether a late admission settles the channel's queued markers after dispatch.
    pub checkpoint_waits: fn(ChannelId) -> bool,
    /// Whether the hosted TUI must defer a queued turn's promotion.
    pub promote_readiness_blocked:
        for<'a> fn(&'a Arc<SharedData>, &'a ProviderKind, ChannelId) -> PromoteBlockedFuture<'a>,
}
