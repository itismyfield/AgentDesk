//! Whether this process may still adopt a selected channel into O. A state changes only under the
//! channel's own lock and is decided once from Pending or Deferred; it cancels or records an
//! adoption, never a delivery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[cfg(test)]
pub(crate) mod body_check;

use crate::services::tui_o::store::OStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Adoption {
    /// Undecided: a Legacy body or placement releases it, a first `init` commits it.
    Pending,
    /// The channel's store exists, so O owns its output.
    Committed,
    /// Store state is damaged or may be public unconfirmed: O keeps the output and holds it.
    Held,
    /// Legacy keeps the channel for the rest of this process.
    Released,
    /// Legacy keeps the channel while its writer host waits for a turn to close and retries;
    /// only that host sets or leaves it, and a Legacy body does not.
    Deferred,
}

impl Adoption {
    pub(crate) fn owned(self) -> bool {
        matches!(self, Self::Committed | Self::Held)
    }
}

/// One selected channel's adoption and the lock every transition of it takes.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    state: Arc<Mutex<Adoption>>,
    /// Test builds: set when a claim released a pending adoption, cleared once a sink saw a body.
    #[cfg(test)]
    bodiless: Arc<std::sync::atomic::AtomicBool>,
}

impl Candidate {
    pub(crate) fn new(state: Adoption) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            #[cfg(test)]
            bodiless: Arc::default(),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Adoption> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Read without deciding anything, for callers that carry no body.
    pub(crate) fn peek(&self) -> Adoption {
        *self.lock()
    }

    /// Whether O owns the channel for a body Legacy would otherwise send; a pending adoption is
    /// released first, and a deferred one later adopts only past what Legacy delivered.
    pub(in crate::services::tui_o) fn claim(&self, channel: u64) -> bool {
        let mut state = self.lock();
        if *state == Adoption::Pending {
            *state = Adoption::Released;
            #[cfg(test)]
            body_check::note_release(self);
            tracing::info!(channel, "[tui_o] Legacy took the channel before O adoption");
        }
        state.owned()
    }

    /// Leaves a pending or deferred adoption to Legacy for the rest of this process; a decided
    /// one is kept.
    pub(crate) fn release(&self, channel: u64) {
        let mut state = self.lock();
        if matches!(*state, Adoption::Pending | Adoption::Deferred) {
            *state = Adoption::Released;
            tracing::info!(channel, "[tui_o] adoption released before the first init");
        }
    }

    /// Leaves a pending adoption to Legacy until its host retries; true while it is deferred.
    pub(crate) fn defer(&self, channel: u64) -> bool {
        let mut state = self.lock();
        match *state {
            Adoption::Pending => {
                *state = Adoption::Deferred;
                tracing::info!(
                    channel,
                    "[tui_o] adoption deferred until Legacy's turn closes"
                );
                true
            }
            Adoption::Deferred => true,
            Adoption::Committed | Adoption::Held | Adoption::Released => false,
        }
    }

    /// A recovered store commits the adoption, unless Legacy already took the channel.
    pub(crate) fn confirm_store(&self) -> bool {
        let mut state = self.lock();
        if *state == Adoption::Released {
            return false;
        }
        *state = Adoption::Committed;
        true
    }
}

/// Where this node stands for the selected channels: only the O home hosts or adopts them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Site {
    #[default]
    Home,
    Foreign {
        home: String,
    },
}

/// `stored` over the selection and every channel the local store committed: an `init` entry or
/// an era member. The whole store is searched before the selection joins it.
pub(crate) fn stored_with_committed(
    runtime_root: Option<&Path>,
    selected: &BTreeSet<u64>,
) -> std::io::Result<BTreeMap<u64, Adoption>> {
    let committed = match runtime_root.and_then(OStore::existing) {
        Some(store) => {
            let mut committed = store.channels_with_init()?;
            if let Ok(Some(era)) = store.read_era() {
                committed.extend(era.initial_channels);
            }
            committed
        }
        None => BTreeSet::new(),
    };
    let channels = selected.union(&committed).copied().collect();
    Ok(stored(runtime_root, &channels))
}

/// The adoption each selected channel starts with, from the local store as it is on disk.
/// A readable `init` commits; unreadable, orphaned or era-only state holds; absent is pending.
pub(crate) fn stored(
    runtime_root: Option<&Path>,
    channels: &BTreeSet<u64>,
) -> BTreeMap<u64, Adoption> {
    let all = |state| channels.iter().map(|&channel| (channel, state)).collect();
    let Some(root) = runtime_root else {
        return all(Adoption::Held);
    };
    let Some(store) = OStore::existing(root) else {
        return all(Adoption::Pending);
    };
    let era = store.read_era();
    let judged = |&channel: &u64| {
        let state = match store.read_init(channel) {
            Ok(Some(_)) => Adoption::Committed,
            Err(_) => Adoption::Held,
            Ok(None) => match &era {
                Ok(Some(era)) if era.initial_channels.contains(&channel) => Adoption::Held,
                Ok(_) if !store.has_channel_dir(channel) => Adoption::Pending,
                _ => Adoption::Held,
            },
        };
        (channel, state)
    };
    channels.iter().map(judged).collect()
}
