//! Record-only selection never enters the pane writer's adoption or body gate.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use poise::serenity_prelude::GuildChannel;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::session_host::surface_policy::VerifiedSurface;
use crate::services::tui_o::store::agy_spool::replay::AgyTarget;

use super::{BootChannels, Site};

#[derive(Clone, Debug, Default)]
pub(super) struct AgySelection {
    pub(super) channels: BTreeSet<u64>,
    children: Arc<Mutex<BTreeMap<u64, Option<u64>>>>,
}

impl AgySelection {
    pub(super) fn project(kinds: &mut BTreeMap<u64, RuntimeHandoffKind>) -> Self {
        let mut selection = Self::default();
        kinds.retain(|id, kind| {
            if *kind == RuntimeHandoffKind::ProcessBackend {
                selection.channels.insert(*id);
                false
            } else {
                true
            }
        });
        selection
    }
}

impl BootChannels {
    pub(crate) fn agy_channels(&self) -> &BTreeSet<u64> {
        &self.agy.channels
    }

    pub(crate) fn agy_selected(&self, channel: u64) -> bool {
        self.agy.channels.contains(&channel)
    }

    pub(crate) fn agy_target(&self, destination: u64) -> Option<AgyTarget> {
        if self.site() != &Site::Home {
            return None;
        }
        let logical_channel = if self.agy_selected(destination) {
            destination
        } else {
            self.agy
                .children
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&destination)
                .copied()
                .flatten()?
        };
        Some(AgyTarget {
            destination,
            logical_channel,
        })
    }

    /// Recovery-only custody of an existing spool: destination and parent checked by REST, never a
    /// new-input selection. Pane custody wins, a held pane child included, though nothing hosts it.
    pub(crate) fn agy_recovery_target(
        &self,
        channel: &GuildChannel,
    ) -> Result<AgyTarget, &'static str> {
        use poise::serenity_prelude::ChannelType;
        let destination = channel.id.get();
        if self.site() != &Site::Home || destination == 0 {
            return Err("agy recovery runs only at home");
        }
        let thread = matches!(
            channel.kind,
            ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
        );
        let logical_channel = match (thread, VerifiedSurface::checked_parent(channel)) {
            (false, _) => destination,
            (true, Some(parent)) => parent,
            (true, None) => return Err("agy recovery thread has no checked parent"),
        };
        let pane_custody = |id: u64| self.hosts_output(id) || self.child(id).is_some();
        if pane_custody(destination) || pane_custody(logical_channel) {
            return Err("agy recovery conflicts with pane custody");
        }
        Ok(AgyTarget {
            destination,
            logical_channel,
        })
    }

    /// Only a checked Discord thread can inherit selection. A conflicting read closes the child.
    pub(crate) fn record_agy_thread(&self, channel: &GuildChannel) -> bool {
        let destination = channel.id.get();
        if self.site() != &Site::Home
            || self.configured_binding(destination)
            || self.child(destination).is_some()
        {
            return false;
        }
        let parent =
            VerifiedSurface::checked_parent(channel).filter(|parent| self.agy_selected(*parent));
        let mut children = self
            .agy
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = children.get_mut(&destination) {
            if *existing != parent {
                *existing = None;
            }
            existing.is_some()
        } else if parent.is_some() {
            children.insert(destination, parent);
            true
        } else {
            false
        }
    }
}
