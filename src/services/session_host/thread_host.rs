//! A thread's host, judged as two separate facts: the parent's policy for its next execution, and
//! the execution this process last judged for it. The raw boot `session_hosts` read is unchanged.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use super::surface_policy::{Relation, relation};
use crate::config::session_hosts::{self, ChannelEndpoint};

/// The host a channel's next new execution follows. It grants no execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyHost {
    /// `via_parent` is `None` for the channel's own boot entry.
    Herdr {
        endpoint: ChannelEndpoint,
        via_parent: Option<u64>,
    },
    Tmux,
    /// An unresolved child: read as neither host.
    Unknown(&'static str),
}

/// The boot entry of `channel` itself, else of the parent a resolved child follows.
pub fn parent_policy(channel: u64) -> PolicyHost {
    // The one raw boot read of this judgement.
    let boot = |channel: u64| session_hosts::herdr_endpoint(channel);
    if let Some(endpoint) = boot(channel) {
        return PolicyHost::Herdr {
            endpoint,
            via_parent: None,
        };
    }
    match relation(channel) {
        Relation::NotChild => PolicyHost::Tmux,
        Relation::Unresolved => PolicyHost::Unknown("thread parent unresolved"),
        Relation::Child { parent } => match boot(parent) {
            Some(endpoint) => PolicyHost::Herdr {
                endpoint,
                via_parent: Some(parent),
            },
            None => PolicyHost::Tmux,
        },
    }
}

/// Whether boot policy routes neither `channel` nor its parent to Herdr. An unresolved child has
/// not left: its policy is unknown, so what it holds for a Herdr host is kept.
pub fn left_herdr(channel: u64) -> bool {
    match parent_policy(channel) {
        PolicyHost::Tmux => true,
        PolicyHost::Herdr { .. } => false,
        PolicyHost::Unknown(reason) => {
            tracing::debug!(
                channel_id = channel,
                reason,
                "host policy unknown: not left"
            );
            false
        }
    }
}

/// The host of the execution a synchronous reader acts on now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Execution {
    Herdr(ChannelEndpoint),
    Tmux,
    /// Nothing was judged for a thread whose parent runs Herdr, or its parent is unresolved; never
    /// shortened to tmux.
    Unknown,
}

impl Execution {
    pub(crate) fn is_herdr(&self) -> bool {
        matches!(self, Self::Herdr(_))
    }

    pub fn endpoint(self) -> Option<ChannelEndpoint> {
        match self {
            Self::Herdr(endpoint) => Some(endpoint),
            Self::Tmux | Self::Unknown => None,
        }
    }
}

/// Threads under a Herdr parent whose turn host this process last judged: `true` for Herdr.
static JUDGED: Mutex<BTreeMap<u64, bool>> = Mutex::new(BTreeMap::new());

/// Records the turn host's final judgement of a thread; `None` (refused or returning) forgets it.
pub fn record_judged(channel: u64, herdr: Option<bool>) {
    if !matches!(relation(channel), Relation::Child { .. }) {
        return;
    }
    let mut judged = JUDGED.lock().unwrap_or_else(PoisonError::into_inner);
    match herdr {
        Some(herdr) => judged.insert(channel, herdr),
        None => judged.remove(&channel),
    };
}

/// A channel's own boot entry is its host until restart. A thread under a Herdr parent reads what
/// the turn host last judged for it in this process, and nothing judged is `Unknown`.
pub fn current_execution_cached(channel: u64) -> Execution {
    match parent_policy(channel) {
        PolicyHost::Herdr {
            endpoint,
            via_parent: None,
        } => Execution::Herdr(endpoint),
        PolicyHost::Tmux => Execution::Tmux,
        PolicyHost::Unknown(_) => Execution::Unknown,
        PolicyHost::Herdr { endpoint, .. } => {
            let judged = JUDGED.lock().unwrap_or_else(PoisonError::into_inner);
            match judged.get(&channel) {
                Some(true) => Execution::Herdr(endpoint),
                Some(false) => Execution::Tmux,
                None => Execution::Unknown,
            }
        }
    }
}

/// The host the next execution of `channel` runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NextHost {
    Herdr,
    Tmux,
}

impl NextHost {
    pub(crate) fn is_herdr(self) -> bool {
        self == Self::Herdr
    }
}

/// The one judgement every reader of the next host shares. A channel's own boot entry is Herdr; a
/// thread under a Herdr parent is Herdr only while its judged execution already runs there.
pub(crate) fn next_host(channel: u64, execution: &Execution) -> NextHost {
    match (parent_policy(channel), execution) {
        (
            PolicyHost::Herdr {
                via_parent: None, ..
            },
            _,
        ) => NextHost::Herdr,
        (PolicyHost::Herdr { endpoint, .. }, Execution::Herdr(at)) if endpoint == *at => {
            NextHost::Herdr
        }
        _ => NextHost::Tmux,
    }
}

/// [`next_host`] over the execution this process last judged.
pub(crate) fn next_host_cached(channel: u64) -> NextHost {
    next_host(channel, &current_execution_cached(channel))
}

/// Whether the next execution of `channel` runs on Herdr, by [`next_host_cached`].
pub fn next_is_herdr(channel: u64) -> bool {
    next_host_cached(channel).is_herdr()
}

/// Whether the execution a synchronous reader acts on runs on Herdr.
pub fn runs_herdr(channel: u64) -> bool {
    current_execution_cached(channel).is_herdr()
}

#[cfg(test)]
#[path = "thread_host_tests.rs"]
mod tests;
