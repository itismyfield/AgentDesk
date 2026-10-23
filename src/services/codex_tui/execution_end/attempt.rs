//! Volatile first-attempt tickets. Eviction and restart lose a claim, never create an End.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use crate::services::tmux_common as tc;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Scope {
    root: PathBuf,
    channel: u64,
    pane: String,
}

impl Scope {
    fn new(channel: u64, pane: &str) -> Option<Self> {
        Some(Self {
            root: crate::config::runtime_root()?,
            channel,
            pane: if super::mutant("channel_only") {
                String::new()
            } else {
                pane.to_owned()
            },
        })
    }
}

#[derive(Default)]
struct Entry {
    epoch: u64,
    active: HashSet<u64>,
    ticket: Option<String>,
}

#[derive(Default)]
struct Registry {
    serial: u64,
    scopes: HashMap<Scope, Entry>,
    untracked: usize,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
const MAX_SCOPES: usize = 64;

impl Registry {
    fn serial(&mut self) -> u64 {
        self.serial = self
            .serial
            .checked_add(1)
            .expect("attempt generation exhausted");
        self.serial
    }

    fn entry(&mut self, scope: &Scope) -> Option<&mut Entry> {
        if !self.scopes.contains_key(scope) && self.scopes.len() >= MAX_SCOPES {
            let oldest = self
                .scopes
                .iter()
                .filter(|(_, e)| e.active.is_empty())
                .min_by_key(|(_, e)| e.epoch)
                .map(|(s, _)| s.clone())?;
            self.scopes.remove(&oldest);
        }
        let epoch = self.serial();
        Some(self.scopes.entry(scope.clone()).or_insert_with(|| Entry {
            epoch,
            ..Entry::default()
        }))
    }
}

/// Owns this dispatch's observation until every possible spawn has returned.
pub(crate) struct AttemptGuard(Option<(Scope, String, u64, bool)>);

/// Every dispatch takes its scope's ticket once, including refused, resume and remote attempts.
/// A call without an exact pane and channel cannot touch the registry.
pub(crate) fn begin(channel: Option<u64>, pane: Option<&str>) -> AttemptGuard {
    if super::mutant("attempt_off") {
        return AttemptGuard(None);
    }
    let Some((channel, pane)) = channel.zip(pane).filter(|(c, p)| *c != 0 && !p.is_empty()) else {
        return AttemptGuard(None);
    };
    let Some(scope) = Scope::new(channel, pane) else {
        return AttemptGuard(None);
    };
    let (token, tracked) = tc::with_tmux_source_authority(pane, |_| {
        let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
        let token = registry.serial();
        let (ticket, tracked) = if let Some(entry) = registry.entry(&scope) {
            entry.epoch = token;
            entry.active.insert(token);
            (entry.ticket.take(), true)
        } else {
            registry.untracked += 1;
            (None, false)
        };
        tracing::debug!(channel, pane, attempt = token, end_ticket = ?ticket, "Codex execution attempt");
        (token, tracked)
    });
    AttemptGuard(Some((scope, pane.to_owned(), token, tracked)))
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        let Some((scope, pane, token, tracked)) = self.0.take() else {
            return;
        };
        tc::with_tmux_source_authority(&pane, |_| {
            let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
            if !tracked {
                registry.untracked = registry.untracked.saturating_sub(1);
                return;
            }
            if let Some(entry) = registry.scopes.get_mut(&scope) {
                entry.active.remove(&token);
            }
        });
    }
}

/// Caller holds this pane's source authority, so dispatch cannot race the snapshot.
pub(crate) fn generation(channel: u64, authority: &tc::TmuxSourceAuthority<'_>) -> u64 {
    let pane = authority.session();
    let Some(scope) = Scope::new(channel, pane) else {
        return 0;
    };
    REGISTRY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(&scope)
        .map_or(0, |entry| entry.epoch)
}

/// Caller holds source authority; only an unchanged scope without a possible spawn gets a ticket.
pub(crate) fn install_ticket(
    channel: u64,
    authority: &tc::TmuxSourceAuthority<'_>,
    epoch: u64,
    end_id: &str,
) -> bool {
    let pane = authority.session();
    let Some(scope) = Scope::new(channel, pane) else {
        return false;
    };
    let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    if epoch == 0 || registry.untracked != 0 {
        return false;
    }
    let Some(entry) = registry.scopes.get_mut(&scope) else {
        return false;
    };
    if entry.epoch != epoch || !entry.active.is_empty() {
        return false;
    }
    entry.ticket = Some(end_id.to_owned());
    true
}

#[cfg(test)]
pub(crate) fn ticket(channel: u64, pane: &str) -> Option<String> {
    let scope = Scope::new(channel, pane)?;
    REGISTRY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .scopes
        .get(&scope)?
        .ticket
        .clone()
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    REGISTRY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .scopes
        .clear();
}
