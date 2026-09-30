//! Whether this process may still adopt a selected channel into O. A state changes only under the
//! channel's own lock and at most once; it cancels or records an adoption, never a delivery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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
}

impl Adoption {
    pub(crate) fn owned(self) -> bool {
        matches!(self, Self::Committed | Self::Held)
    }
}

/// One selected channel's adoption and the lock every transition of it takes.
#[derive(Clone, Debug)]
pub(crate) struct Candidate(Arc<Mutex<Adoption>>);

impl Candidate {
    pub(crate) fn new(state: Adoption) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Adoption> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Read without deciding anything, for callers that carry no body.
    pub(crate) fn peek(&self) -> Adoption {
        *self.lock()
    }

    /// Whether O owns the channel for a body Legacy would otherwise send; a pending adoption is
    /// released first, so O never starts under a body Legacy already took.
    pub(crate) fn claim(&self, channel: u64) -> bool {
        let mut state = self.lock();
        if *state == Adoption::Pending {
            *state = Adoption::Released;
            tracing::info!(channel, "[tui_o] Legacy took the channel before O adoption");
        }
        state.owned()
    }

    /// Leaves a pending adoption to Legacy for the rest of this process; a decided one is kept.
    pub(crate) fn release(&self, channel: u64) {
        let mut state = self.lock();
        if *state == Adoption::Pending {
            *state = Adoption::Released;
            tracing::info!(channel, "[tui_o] adoption released before the first init");
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
