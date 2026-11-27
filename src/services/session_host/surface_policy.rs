//! Verified destination/parent evidence; raw request metadata cannot construct it.

use crate::services::agent_protocol::RuntimeHandoffKind;

static POLICY_CHANNEL: std::sync::OnceLock<fn(u64) -> u64> = std::sync::OnceLock::new();

/// Installs the boot policy's read of a verified child's parent.
pub(crate) fn install_policy_channel(read: fn(u64) -> u64) {
    let _ = POLICY_CHANNEL.set(read);
}

pub(crate) fn policy_channel(channel: u64) -> u64 {
    // A thread forced as a child follows its forced parent, as the boot view's would.
    #[cfg(any(test, feature = "test-support"))]
    if let Some(Relation::Child { parent }) =
        FORCED_RELATIONS.with(|forced| forced.borrow().get(&channel).copied())
    {
        return parent;
    }
    POLICY_CHANNEL.get().map_or(channel, |read| read(channel))
}

/// What the boot view holds for a thread: its checked parent, or a child it could not resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Relation {
    NotChild,
    Child {
        parent: u64,
    },
    /// A held or conflicting child: its parent is not known, so it reads as neither host.
    Unresolved,
}

static RELATION: std::sync::OnceLock<fn(u64) -> Relation> = std::sync::OnceLock::new();

/// Installs the boot policy's read of a thread's relation, beside [`install_policy_channel`].
pub(crate) fn install_relation(read: fn(u64) -> Relation) {
    let _ = RELATION.set(read);
}

pub(crate) fn relation(channel: u64) -> Relation {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(forced) = FORCED_RELATIONS.with(|forced| forced.borrow().get(&channel).copied()) {
        return forced;
    }
    RELATION
        .get()
        .map_or(Relation::NotChild, |read| read(channel))
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static FORCED_RELATIONS: std::cell::RefCell<std::collections::BTreeMap<u64, Relation>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// This thread's relation of `channel` until dropped, for a judgement no boot view is built for.
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct ForcedRelation(u64);

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn force_relation_for_test(channel: u64, relation: Relation) -> ForcedRelation {
    FORCED_RELATIONS.with(|forced| forced.borrow_mut().insert(channel, relation));
    ForcedRelation(channel)
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for ForcedRelation {
    fn drop(&mut self) {
        FORCED_RELATIONS.with(|forced| forced.borrow_mut().remove(&self.0));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    pub(crate) fn relationship(&self) -> (u64, u64, RuntimeHandoffKind) {
        (self.destination, self.parent, self.kind)
    }

    /// Only a channel Discord itself returned, of an actual thread kind, under a nonzero parent.
    pub(crate) fn from_discord(
        channel: &poise::serenity_prelude::GuildChannel,
        kind: RuntimeHandoffKind,
    ) -> Option<Self> {
        Self::checked_parent(channel).map(|parent| Self {
            destination: channel.id.get(),
            parent,
            kind,
        })
    }

    pub(crate) fn checked_parent(channel: &poise::serenity_prelude::GuildChannel) -> Option<u64> {
        use poise::serenity_prelude::ChannelType;
        if !matches!(
            channel.kind,
            ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
        ) {
            return None;
        }
        let (destination, parent) = (channel.id.get(), channel.parent_id?.get());
        (destination != 0 && parent != 0 && destination != parent).then_some(parent)
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

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn fixture(channels: &[u64]) -> Self {
        Self {
            bindings: channels.iter().copied().collect(),
            kinds: Default::default(),
        }
    }
}
