//! Volatile first-attempt tickets. Eviction and restart lose a claim, never create an End.

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};

use super::producer::Candidate;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptId {
    scope: Scope,
    token: u64,
}

impl AttemptId {
    pub(crate) fn channel(&self) -> u64 {
        self.scope.channel
    }
    pub(crate) fn pane(&self) -> &str {
        &self.scope.pane
    }
    pub(crate) fn root(&self) -> &Path {
        &self.scope.root
    }
    fn matches(&self, authority: &tc::TmuxSourceAuthority<'_>) -> bool {
        Scope::new(self.channel(), authority.session()).as_ref() == Some(&self.scope)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    MaySpawn,
    Bound { nonce: String },
    Done,
}

struct PendingEndTicket(String);

struct CandidateSlot {
    id: AttemptId,
    value: Candidate,
}

#[derive(Default)]
struct Entry {
    epoch: u64,
    active: HashMap<u64, Phase>,
    claims: HashMap<u64, PendingEndTicket>,
    ticket: Option<PendingEndTicket>,
    candidate: Option<CandidateSlot>,
    bound: Option<(u64, String)>,
    done: Option<u64>,
}

impl Entry {
    fn valid(&self, id: &AttemptId) -> bool {
        (self.epoch == id.token || super::mutant("epoch_removed"))
            && (self.active.contains_key(&id.token)
                || self
                    .bound
                    .as_ref()
                    .is_some_and(|(token, _)| *token == id.token))
            && !self
                .active
                .iter()
                .any(|(token, phase)| *token != id.token && *phase == Phase::MaySpawn)
    }
}

#[derive(Default)]
struct Registry {
    serial: u64,
    scopes: HashMap<Scope, Entry>,
    untracked: usize,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
const MAX_SCOPES: usize = 64;

#[derive(Default)]
struct ThreadContext {
    serial: u64,
    stack: Vec<(u64, Option<AttemptId>)>,
}

thread_local! {
    static CURRENT: RefCell<ThreadContext> = RefCell::new(ThreadContext::default());
}

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

/// Dispatch is synchronous; this guard cannot move its calling-thread identity to another thread.
pub(crate) struct AttemptGuard {
    observed: Option<(AttemptId, bool)>,
    context: u64,
    _same_thread: PhantomData<Rc<()>>,
}

impl AttemptGuard {
    fn new(observed: Option<(AttemptId, bool)>) -> Self {
        let current = observed
            .as_ref()
            .filter(|(_, tracked)| *tracked)
            .map(|(id, _)| id.clone());
        let context = CURRENT.with_borrow_mut(|state| {
            state.serial = state
                .serial
                .checked_add(1)
                .expect("thread attempt generation exhausted");
            let context = state.serial;
            state.stack.push((context, current));
            context
        });
        Self {
            observed,
            context,
            _same_thread: PhantomData,
        }
    }
}

/// Every exact-pane dispatch takes its ticket, even when it later resumes or is refused.
pub(crate) fn begin(channel: Option<u64>, pane: Option<&str>) -> AttemptGuard {
    if super::mutant("attempt_off") {
        return AttemptGuard::new(None);
    }
    let Some((channel, pane)) = channel.zip(pane).filter(|(c, p)| *c != 0 && !p.is_empty()) else {
        return AttemptGuard::new(None);
    };
    let Some(scope) = Scope::new(channel, pane) else {
        return AttemptGuard::new(None);
    };
    let observed = tc::with_tmux_source_authority(pane, |_| {
        let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
        let token = registry.serial();
        let tracked = if let Some(entry) = registry.entry(&scope) {
            let overlapping = entry.active.values().any(|phase| *phase == Phase::MaySpawn);
            if !super::mutant("adjacent_removed") {
                if let Some(candidate) = &entry.candidate
                    && !super::mutant("begin_discard_warn_removed")
                {
                    candidate.value.discarded(
                        pane,
                        "Bound:Discarded",
                        "superseded by next attempt",
                    );
                }
                entry.candidate = None;
                entry.claims.clear();
            }
            entry.epoch = token;
            entry.bound = None;
            entry.done = None;
            let ticket = if super::mutant("chokepoint_removed") {
                None
            } else {
                entry.ticket.take()
            };
            if !overlapping && let Some(ticket) = ticket {
                entry.claims.insert(token, ticket);
            }
            entry.active.insert(token, Phase::MaySpawn);
            true
        } else {
            registry.untracked += 1;
            false
        };
        tracing::debug!(
            channel,
            pane,
            attempt = token,
            tracked,
            "Codex execution attempt"
        );
        (AttemptId { scope, token }, tracked)
    });
    AttemptGuard::new(Some(observed))
}

/// The calling attempt is never inferred from the newest registry entry.
pub(crate) fn current(channel: u64, pane: &str) -> Option<AttemptId> {
    let scope = Scope::new(channel, pane)?;
    CURRENT.with_borrow(|state| {
        state
            .stack
            .last()?
            .1
            .as_ref()
            .filter(|id| id.scope == scope)
            .cloned()
    })
}

pub(crate) fn take_ticket(id: &AttemptId) -> Option<String> {
    tc::with_tmux_source_authority(id.pane(), |authority| {
        if !id.matches(authority) {
            return None;
        }
        let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
        if registry.untracked != 0 {
            return None;
        }
        let entry = registry.scopes.get_mut(&id.scope)?;
        entry
            .valid(id)
            .then(|| entry.claims.remove(&id.token))
            .flatten()
            .map(|ticket| ticket.0)
    })
}

pub(crate) fn bound_under(
    id: &AttemptId,
    authority: &tc::TmuxSourceAuthority<'_>,
    nonce: &str,
) -> bool {
    if nonce.is_empty() || !id.matches(authority) {
        return false;
    }
    let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    if registry.untracked != 0 {
        return false;
    }
    let Some(entry) = registry.scopes.get_mut(&id.scope) else {
        return false;
    };
    if !entry.valid(id) {
        return false;
    }
    let Some(phase) = entry.active.get_mut(&id.token) else {
        return false;
    };
    if matches!(phase, Phase::Bound { nonce: existing } if existing.as_str() != nonce) {
        return false;
    }
    *phase = Phase::Bound {
        nonce: nonce.to_owned(),
    };
    entry.bound = Some((id.token, nonce.to_owned()));
    entry.claims.remove(&id.token);
    true
}

pub(crate) fn phase_under(
    id: &AttemptId,
    authority: &tc::TmuxSourceAuthority<'_>,
) -> Option<Phase> {
    if !id.matches(authority) {
        return None;
    }
    let registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    let entry = registry.scopes.get(&id.scope)?;
    if entry.epoch != id.token {
        return None;
    }
    entry
        .active
        .get(&id.token)
        .cloned()
        .or_else(|| (entry.done == Some(id.token)).then_some(Phase::Done))
}

pub(super) fn put_candidate_under(
    id: &AttemptId,
    authority: &tc::TmuxSourceAuthority<'_>,
    value: Candidate,
) -> bool {
    if !id.matches(authority) {
        return false;
    }
    let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    if registry.untracked != 0 {
        return false;
    }
    let Some(entry) = registry.scopes.get_mut(&id.scope) else {
        return false;
    };
    if !entry.valid(id) || entry.candidate.is_some() {
        return false;
    }
    entry.candidate = Some(CandidateSlot {
        id: id.clone(),
        value,
    });
    true
}

/// The callback may write the End store; it never runs while the registry mutex is held.
pub(super) fn with_candidate_under(
    channel: u64,
    authority: &tc::TmuxSourceAuthority<'_>,
    nonce: &str,
    operation: impl FnOnce(Candidate) -> Option<Candidate>,
) -> bool {
    let Some(scope) = Scope::new(channel, authority.session()) else {
        return false;
    };
    let slot = {
        let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
        if registry.untracked != 0 {
            return false;
        }
        let Some(entry) = registry.scopes.get_mut(&scope) else {
            return false;
        };
        let Some(slot) = entry.candidate.as_ref() else {
            return false;
        };
        if !entry.valid(&slot.id)
            || entry.bound.as_ref() != Some(&(slot.id.token, nonce.to_owned()))
        {
            return false;
        }
        entry.candidate.take().expect("candidate checked")
    };
    if let Some(value) = operation(slot.value) {
        let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
        if registry.untracked == 0
            && let Some(entry) = registry.scopes.get_mut(&scope)
            && entry.valid(&slot.id)
            && entry.candidate.is_none()
            && entry.bound.as_ref() == Some(&(slot.id.token, nonce.to_owned()))
        {
            entry.candidate = Some(CandidateSlot { id: slot.id, value });
        } else {
            value.discarded(
                authority.session(),
                "Bound:Discarded",
                "candidate reinsertion failed",
            );
        }
    }
    true
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        CURRENT.with_borrow_mut(|state| state.stack.retain(|(key, _)| *key != self.context));
        let Some((id, tracked)) = self.observed.take() else {
            return;
        };
        tc::with_tmux_source_authority(id.pane(), |_| {
            let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
            if !tracked {
                registry.untracked = registry.untracked.saturating_sub(1);
                return;
            }
            if let Some(entry) = registry.scopes.get_mut(&id.scope) {
                let phase = entry.active.remove(&id.token);
                entry.claims.remove(&id.token);
                if matches!(phase, Some(Phase::MaySpawn))
                    && entry.candidate.as_ref().is_some_and(|slot| slot.id == id)
                {
                    entry.candidate = None;
                }
                if entry.epoch == id.token {
                    entry.done = Some(id.token);
                }
            }
        });
    }
}

/// Caller holds this pane's source authority, so dispatch cannot race the snapshot.
pub(crate) fn generation(channel: u64, authority: &tc::TmuxSourceAuthority<'_>) -> u64 {
    let Some(scope) = Scope::new(channel, authority.session()) else {
        return 0;
    };
    REGISTRY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(&scope)
        .map_or(0, |entry| entry.epoch)
}

/// Bound streams cannot spawn again; only MaySpawn blocks a clear's ticket installation.
pub(crate) fn install_ticket(
    channel: u64,
    authority: &tc::TmuxSourceAuthority<'_>,
    epoch: u64,
    end_id: &str,
) -> bool {
    let Some(scope) = Scope::new(channel, authority.session()) else {
        return false;
    };
    let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    if epoch == 0 || registry.untracked != 0 {
        return false;
    }
    let Some(entry) = registry.scopes.get_mut(&scope) else {
        return false;
    };
    if entry.epoch != epoch || entry.active.values().any(|phase| *phase == Phase::MaySpawn) {
        return false;
    }
    if let Some(candidate) = &entry.candidate {
        candidate.value.discarded(
            authority.session(),
            "Bound:Discarded",
            "superseded by confirmed clear",
        );
    }
    entry.candidate = None;
    entry.claims.clear();
    entry.ticket = Some(PendingEndTicket(end_id.to_owned()));
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
        .as_ref()
        .map(|ticket| ticket.0.clone())
}

#[cfg(test)]
pub(crate) fn claim_for_tests(id: &AttemptId) -> Option<String> {
    REGISTRY
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .scopes
        .get(&id.scope)?
        .claims
        .get(&id.token)
        .map(|ticket| ticket.0.clone())
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut registry = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    registry.scopes.clear();
    registry.untracked = 0;
}
