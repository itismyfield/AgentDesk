//! Verified destination/parent evidence; raw request metadata cannot construct it.

use crate::services::agent_protocol::RuntimeHandoffKind;

#[derive(Clone, Copy, Debug)]
pub(crate) struct VerifiedSurface {
    destination: u64,
    parent: u64,
    kind: RuntimeHandoffKind,
}

impl VerifiedSurface {
    pub(crate) fn matches(
        &self,
        destination: u64,
        parent: u64,
        kind: Option<RuntimeHandoffKind>,
    ) -> bool {
        self.destination == destination && self.parent == parent && kind == Some(self.kind)
    }

    #[cfg(test)]
    pub(crate) fn relationship(&self) -> (u64, u64, RuntimeHandoffKind) {
        (self.destination, self.parent, self.kind)
    }

    // The producer is dormant until child admission is enabled.
    #[cfg(test)]
    pub(crate) fn from_discord(
        channel: &poise::serenity_prelude::GuildChannel,
        kind: RuntimeHandoffKind,
    ) -> Option<Self> {
        use poise::serenity_prelude::ChannelType;
        if !matches!(
            channel.kind,
            ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
        ) {
            return None;
        }
        let (destination, parent) = (channel.id.get(), channel.parent_id?.get());
        (destination != 0 && parent != 0 && destination != parent).then_some(Self {
            destination,
            parent,
            kind,
        })
    }
}
