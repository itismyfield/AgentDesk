//! The router functions every `SharedData` carries as its `RouterPorts`.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::discord::shared_state::{PromoteBlockedFuture, RouterPorts};
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) const ROUTER_PORTS: RouterPorts = RouterPorts {
    #[cfg(unix)]
    owner_wake: super::message_handler::owner_wake,
    checkpoint_waits: super::checkpoint_waits_for_admission,
    promote_readiness_blocked,
};

fn promote_readiness_blocked<'a>(
    shared: &'a Arc<SharedData>,
    provider: &'a ProviderKind,
    channel: ChannelId,
) -> PromoteBlockedFuture<'a> {
    Box::pin(super::hosted_tui_promote_readiness_blocked(
        shared, provider, channel,
    ))
}
