//! A shared, derived reader view. It does not add children to output membership.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use super::{Adoption, BootChannels, Candidate};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::store::{Initialized, OStore};

#[derive(Clone, Debug)]
struct ChildEntry {
    parent: Option<u64>,
    kind: Option<RuntimeHandoffKind>,
    candidate: Option<Candidate>,
    held: bool,
    conflict: bool,
    record_unreadable: bool,
    boot_discovered: bool,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ChildEntries {
    entries: Arc<Mutex<BTreeMap<u64, ChildEntry>>>,
    refresh: Arc<Mutex<()>>,
}

impl ChildEntries {
    fn record(&self, boot: &BootChannels, init: &Initialized, adoption: Adoption) {
        let parent = init.policy_parent;
        let kind = parent.and_then(|parent| boot.kind(parent));
        let held = parent.is_none_or(|parent| {
            parent == 0 || parent == init.channel || !boot.configured_binding(parent)
        }) || kind.is_none();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        match entries.entry(init.channel) {
            std::collections::btree_map::Entry::Occupied(mut existing) => {
                let existing = existing.get_mut();
                // A conflict never replaces the candidate or opens a held entry.
                existing.conflict |= held || existing.parent != parent || existing.kind != kind;
                existing.held = existing.conflict;
                if existing.record_unreadable
                    && !existing.conflict
                    && let Some(candidate) = &existing.candidate
                {
                    *candidate.lock() = adoption;
                }
                existing.record_unreadable = false;
                if existing.held
                    && let Some(candidate) = &existing.candidate
                {
                    *candidate.lock() = Adoption::Held;
                }
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ChildEntry {
                    parent,
                    kind,
                    candidate: Some(Candidate::new(if held { Adoption::Held } else { adoption })),
                    held,
                    conflict: held,
                    record_unreadable: false,
                    boot_discovered: true,
                });
            }
        }
    }

    #[cfg(all(test, unix))]
    fn resolve_legacy(
        &self,
        boot: &BootChannels,
        mut checked: impl FnMut(
            u64,
        ) -> Result<
            crate::services::session_host::surface_policy::VerifiedSurface,
            (),
        >,
    ) {
        let Ok(_refresh) = self.refresh.try_lock() else {
            return;
        };
        let mut entries = self.entries.lock().unwrap();
        for (&channel, entry) in &mut *entries {
            if entry.parent.is_some() || entry.conflict {
                continue;
            }
            let Ok(surface) = checked(channel) else {
                continue;
            };
            let (destination, parent, kind) = surface.relationship();
            if destination != channel || boot.kind(parent).is_some_and(|expected| expected != kind)
            {
                entry.conflict = true;
                continue;
            }
            entry.parent = Some(parent);
            entry.kind = Some(kind);
            entry.held = false;
            entry.candidate = boot.kind(parent).map(|_| {
                if entry.boot_discovered {
                    Candidate::reserved_late()
                } else {
                    Candidate::new(Adoption::Pending)
                }
            });
        }
    }

    fn refresh_records(&self, boot: &BootChannels, root: Option<&Path>) {
        let Ok(_refresh) = self.refresh.try_lock() else {
            return;
        };
        let channels: Vec<_> = self
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, entry)| entry.boot_discovered)
            .map(|(&id, _)| id)
            .collect();
        if channels.is_empty() {
            return;
        }
        let Some(store) = root.and_then(OStore::existing) else {
            return;
        };
        for channel in channels {
            match store.read_init(channel) {
                Ok(Some(init)) if init.policy_parent.is_some() => {
                    self.record(boot, &init, Adoption::Committed)
                }
                _ => {
                    if let Some(entry) = self
                        .entries
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get_mut(&channel)
                    {
                        entry.held = true;
                        entry.record_unreadable = true;
                        if let Some(candidate) = &entry.candidate {
                            *candidate.lock() = Adoption::Held;
                        }
                    }
                }
            }
        }
    }
}

pub(super) fn seeded_from_store(
    boot: BootChannels,
    config: &crate::config::Config,
    root: Option<&Path>,
) -> anyhow::Result<BootChannels> {
    let states = |channels: &std::collections::BTreeSet<u64>, committed: bool| {
        if !committed {
            return Ok(super::adoption::stored(root, channels));
        }
        let mut states = super::adoption::stored_with_committed(root, channels)?;
        if let Some(store) = root.and_then(OStore::existing) {
            let mut children = Vec::new();
            for (&channel, &state) in &states {
                if boot.configured_binding(channel) {
                    continue;
                }
                if let Ok(Some(init)) = store.read_init(channel)
                    && init.policy_parent.is_some()
                {
                    boot.children.record(&boot, &init, state);
                    children.push(channel);
                }
            }
            for channel in children {
                states.remove(&channel);
            }
        }
        Ok(states)
    };
    boot.clone().seeded(config, states)
}

impl BootChannels {
    /// Called once per shared view before provider dispatch; recorded relationships need no REST.
    pub(crate) fn refresh_child_records(&self) {
        if self
            .children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
        {
            return;
        }
        self.children
            .refresh_records(self, crate::config::runtime_root().as_deref());
    }

    pub(crate) fn configured_binding(&self, channel: u64) -> bool {
        self.bindings.contains(&channel)
    }
}

#[cfg(all(test, unix))]
mod tests;
