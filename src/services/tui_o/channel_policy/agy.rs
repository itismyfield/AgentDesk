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
    pub fn agy_channels(&self) -> &BTreeSet<u64> {
        &self.agy.channels
    }

    pub(crate) fn agy_selected(&self, channel: u64) -> bool {
        self.agy.channels.contains(&channel)
    }

    pub fn agy_target(&self, destination: u64) -> Option<AgyTarget> {
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

    /// Only a checked Discord thread can inherit selection. A conflicting read closes the child.
    pub fn record_agy_thread(&self, channel: &GuildChannel) -> bool {
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
