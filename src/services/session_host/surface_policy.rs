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

    /// The parent channel whose policy the destination thread follows.
    pub(crate) fn parent(&self) -> u64 {
        self.parent
    }

    #[cfg(all(test, unix))]
    pub(crate) fn relationship(&self) -> (u64, u64, RuntimeHandoffKind) {
        (self.destination, self.parent, self.kind)
    }

    // The producer is dormant until child admission is enabled.
    #[cfg(all(test, unix))]
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

#[derive(Clone, Debug)]
pub(crate) struct InitScope {
    bindings: std::collections::BTreeSet<u64>,
    kinds: std::collections::BTreeMap<u64, RuntimeHandoffKind>,
}

impl InitScope {
    pub(crate) fn from_boot(
        bindings: &std::collections::BTreeSet<u64>,
        kinds: &std::collections::BTreeMap<u64, RuntimeHandoffKind>,
    ) -> Self {
        Self {
            bindings: bindings.clone(),
            kinds: kinds.clone(),
        }
    }

    pub(crate) fn configured(&self, channel: u64) -> bool {
        self.bindings.contains(&channel)
    }

    pub(crate) fn kind(&self, channel: u64) -> Option<RuntimeHandoffKind> {
        self.kinds.get(&channel).copied()
    }

    #[cfg(test)]
    pub(crate) fn fixture(channels: &[u64]) -> Self {
        Self {
            bindings: channels.iter().copied().collect(),
            kinds: Default::default(),
        }
    }
}
