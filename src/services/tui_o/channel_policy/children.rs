//! A shared, derived view of thread children: each one's own adoption under its parent's policy.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use super::{Adoption, BootChannels, Candidate};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::session_host::surface_policy::VerifiedSurface;
use crate::services::tui_o::store::{Initialized, OStore};

mod lifecycle;
pub(crate) use lifecycle::RetirementTicket;

#[derive(Clone, Debug)]
struct ChildEntry {
    parent: Option<u64>,
    kind: Option<RuntimeHandoffKind>,
    candidate: Option<Candidate>,
    held: bool,
    conflict: bool,
    record_unreadable: bool,
    boot_discovered: bool,
    /// The checked relationship a placement resolved; only a child holding it can first init.
    surface: Option<VerifiedSurface>,
    life: lifecycle::Lifecycle,
}

/// A resolved child of an O parent, as every snapshot clone reads it.
#[derive(Clone, Debug)]
pub(crate) struct Child {
    pub(crate) parent: u64,
    pub(crate) kind: RuntimeHandoffKind,
    pub(crate) candidate: Candidate,
}

impl Child {
    /// Who has the child's body now: O (`true`), or Legacy while a late reservation waits for its
    /// open turn. `None` is an undecided adoption, which is held and never handed to Legacy.
    pub(crate) fn body_owner(&self) -> Option<bool> {
        match self.candidate.peek() {
            state if state.owned() => Some(true),
            Adoption::Deferred if self.candidate.late_adopt_origin() => Some(false),
            _ => None,
        }
    }
}

impl ChildEntry {
    /// A boot reservation whose parent is still unknown: Legacy's, with its sends counted.
    fn unresolved_late(&self) -> Option<&Candidate> {
        let open = self.parent.is_none() && !self.held && !self.conflict;
        self.candidate
            .as_ref()
            .filter(|c| open && c.late_adopt_origin())
    }
}

/// A known child whose relationship is unresolved, conflicting or unreadable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChildHeld;

impl ChildEntry {
    /// `None` once the child resolved under a parent O does not select.
    fn read(&self) -> Option<Result<Child, ChildHeld>> {
        if self.unresolved_late().is_some() {
            return None;
        }
        let (Some(parent), false, false) = (self.parent, self.held, self.conflict) else {
            return Some(Err(ChildHeld));
        };
        let candidate = self.candidate.clone()?;
        // An adoption already bound under another parent never initializes under this one.
        if let Some(surface) = self.surface
            && (surface.parent() != parent || !candidate.bind_child(surface))
        {
            return Some(Err(ChildHeld));
        }
        Some(self.kind.map_or(Err(ChildHeld), |kind| {
            Ok(Child {
                parent,
                kind,
                candidate,
            })
        }))
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct ChildEntries {
    entries: Arc<Mutex<BTreeMap<u64, ChildEntry>>>,
    refresh: Arc<Mutex<()>>,
}

impl ChildEntries {
    fn record(&self, boot: &BootChannels, init: &Initialized, adoption: Adoption) {
        let parent = init.policy_parent;
        let kind = parent.and_then(|parent| boot.kinds.get(&parent).copied());
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
                    surface: None,
                    life: lifecycle::Lifecycle::default(),
                });
            }
        }
    }

    /// Applies one checked relationship, keeping any known adoption so the Legacy sends it counted
    /// still hold off the first init; returns whether O now hosts the child.
    fn resolve(
        &self,
        boot: &BootChannels,
        channel: u64,
        surface: VerifiedSurface,
        fresh: bool,
    ) -> bool {
        // A configured binding keeps its own member adoption; a second one would split its writer.
        if boot.configured_binding(channel) {
            return false;
        }
        let (destination, parent, kind) = surface.relationship();
        let expected = boot
            .configured_binding(parent)
            .then(|| boot.kinds.get(&parent).copied())
            .flatten();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if expected.is_none() && !entries.contains_key(&channel) {
            return false;
        }
        let entry = entries.entry(channel).or_insert_with(|| ChildEntry {
            parent: None,
            kind: None,
            candidate: None,
            held: false,
            conflict: false,
            record_unreadable: false,
            boot_discovered: false,
            surface: None,
            life: lifecycle::Lifecycle::default(),
        });
        if entry.conflict {
            return false;
        }
        if destination != channel || expected.is_some_and(|expected| expected != kind) {
            entry.conflict = true;
            entry.held = true;
            return false;
        }
        if let Some(known) = entry.parent {
            // A resolved child is never moved to another parent.
            if known != parent {
                entry.conflict = true;
                entry.held = true;
            }
            return !entry.conflict && entry.candidate.is_some();
        }
        entry.parent = Some(parent);
        entry.kind = Some(kind);
        entry.held = false;
        entry.surface = Some(surface);
        let late = entry.boot_discovered || !fresh;
        entry.candidate = expected.map(|_| {
            entry.candidate.take().unwrap_or_else(|| {
                if late {
                    Candidate::reserved_late()
                } else {
                    Candidate::new(Adoption::Pending)
                }
            })
        });
        entry.candidate.is_some()
    }

    /// A known thread left over from before boot: Legacy keeps its body, counted on its own late
    /// adoption, until a placement reads its parent.
    fn reserve(&self, channel: u64) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.entry(channel).or_insert_with(|| ChildEntry {
            parent: None,
            kind: None,
            candidate: Some(Candidate::reserved_late()),
            held: false,
            conflict: false,
            record_unreadable: false,
            boot_discovered: true,
            surface: None,
            life: lifecycle::Lifecycle::default(),
        });
    }

    #[cfg(all(test, unix))]
    fn resolve_legacy(
        &self,
        boot: &BootChannels,
        mut checked: impl FnMut(u64) -> Result<VerifiedSurface, ()>,
    ) {
        let Ok(_refresh) = self.refresh.try_lock() else {
            return;
        };
        let unresolved = |entry: &ChildEntry| entry.parent.is_none() && !entry.conflict;
        // The checked read runs with no entry lock held, so the gate never waits on REST.
        let pending: Vec<(u64, bool)> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, entry)| unresolved(entry))
            .map(|(&channel, entry)| (channel, entry.boot_discovered))
            .collect();
        for (channel, boot_discovered) in pending {
            let Ok(surface) = checked(channel) else {
                continue;
            };
            // The entry may have been resolved or replaced while the read was in flight.
            let still = self
                .entries
                .lock()
                .unwrap()
                .get(&channel)
                .is_some_and(|entry| unresolved(entry) && entry.boot_discovered == boot_discovered);
            if still {
                self.resolve(boot, channel, surface, !boot_discovered);
            }
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
            // Only children boot read from the store; a checked one has no store record to lose yet.
            .filter(|(_, entry)| {
                entry.boot_discovered && entry.surface.is_none() && entry.parent.is_some()
            })
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
                    boot.children.rehydrate(channel, &store);
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
    pub(crate) fn init_scope_view(
        &self,
    ) -> Arc<crate::services::session_host::surface_policy::InitScope> {
        self.init_scope
            .get_or_init(|| {
                Arc::new(
                    crate::services::session_host::surface_policy::InitScope::from_boot(
                        &self.bindings,
                        &self.kinds,
                    ),
                )
            })
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn record_child_fixture(&self, init: &Initialized) {
        self.children.record(self, init, Adoption::Committed);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn child_fixture_held(&self, channel: u64) -> Option<bool> {
        self.children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&channel)
            .and_then(|entry| entry.candidate.as_ref())
            .map(|candidate| candidate.peek() == Adoption::Held)
    }

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

    /// What the shared view holds for `channel`: `None` leaves it to boot membership.
    pub(crate) fn child(&self, channel: u64) -> Option<Result<Child, ChildHeld>> {
        let entries = self
            .children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        entries.get(&channel)?.read()
    }

    /// A boot reservation whose parent no placement has read yet; Legacy's, its sends counted.
    pub(crate) fn unresolved_child(&self, channel: u64) -> Option<Candidate> {
        let entries = self
            .children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        entries.get(&channel)?.unresolved_late().cloned()
    }

    /// Records a relationship Discord itself returned; `fresh` when no Legacy turn ever ran there.
    pub(crate) fn resolve_child(
        &self,
        channel: u64,
        surface: VerifiedSurface,
        fresh: bool,
    ) -> bool {
        self.children.resolve(self, channel, surface, fresh)
    }

    /// Reserves each unconfigured thread a boot read left open whose hinted parent O selects. The
    /// hint only picks the reservation; the parent is bound by a later checked read.
    pub(crate) fn reserve_boot_children(&self, threads: &[(u64, u64)]) {
        for &(channel, hinted) in threads {
            if !self.configured_binding(channel) && self.channels.contains(&hinted) {
                self.children.reserve(channel);
            }
        }
    }

    /// A reservation whose channel turned out to be no O thread: plain Legacy from now on.
    pub(crate) fn release_reservation(&self, channel: u64) {
        let mut entries = self
            .children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if entries
            .get(&channel)
            .is_some_and(|entry| entry.unresolved_late().is_some())
        {
            entries.remove(&channel);
        }
    }

    /// Boot membership plus every known child, held ones included so no reader drops them.
    pub(crate) fn output_channels(&self) -> std::collections::BTreeSet<u64> {
        let entries = self
            .children
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let children = entries
            .iter()
            .filter(|(_, entry)| entry.read().is_some())
            .map(|(&channel, _)| channel);
        self.channels.iter().copied().chain(children).collect()
    }

    /// Whether O may host `channel` here: a boot TUI member or a resolved child of one.
    pub(crate) fn hosts_output(&self, channel: u64) -> bool {
        super::owns_output(&self.channels, channel, self.kinds.get(&channel).copied())
            || matches!(self.child(channel), Some(Ok(_)))
    }

    /// The channel whose boot policy governs `channel`: a child's parent, or the channel itself.
    pub(crate) fn policy_channel(&self, channel: u64) -> u64 {
        match self.child(channel) {
            Some(Ok(child)) => child.parent,
            _ => channel,
        }
    }

    /// Whether `channel`'s writer reads dormant: a thread child reads as its parent does.
    pub(crate) fn dormant(&self, channel: u64) -> bool {
        self.dormant_channels()
            .contains(&self.policy_channel(channel))
    }
}

#[cfg(all(test, unix))]
mod tests;
