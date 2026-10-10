//! First activation of a selected channel with no O store: its `init` is written only while pending
//! or deferred. A failed check releases only a pending one; after a failed write the store decides.

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::adoption::logged;
use super::binding::BindingEvents;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::same_file;
use crate::services::tui_o::store::{InitSource, Initialized, OStore};

/// What the gateway reports about a channel before its first activation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivationFacts {
    pub open_intake: i64,
    pub runner_sessions: i64,
    pub node_override: Option<String>,
}

impl ActivationFacts {
    fn blocker(&self) -> Option<String> {
        self.transient_blocker().or_else(|| self.final_blocker())
    }

    /// What keeps the channel off this node for good: sessions on another node or an override.
    pub fn final_blocker(&self) -> Option<String> {
        if self.runner_sessions != 0 {
            return Some(format!("{} sessions on another node", self.runner_sessions));
        }
        self.node_override
            .as_ref()
            .map(|node| format!("node override to {node}"))
    }

    /// What may clear while the channel waits: open intake rows.
    pub fn transient_blocker(&self) -> Option<String> {
        (self.open_intake != 0).then(|| format!("{} open intake rows", self.open_intake))
    }
}

/// Serializes first activations so two channels never race the era seal.
static ACTIVATING: Mutex<()> = Mutex::new(());

/// Creates the channel's `init` over every source its binding log binds, each still empty, and
/// seals the era first time round. Legacy judges the channel under the same `candidate` lock, so
/// the local checks and the write see no Legacy body in between. Returns why it did not commit.
pub fn activate<B: BindingEvents>(
    store: &OStore,
    channel: u64,
    facts: Result<ActivationFacts, String>,
    bindings: &B,
    local_custody: impl FnOnce() -> Result<bool, String>,
    candidate: &Candidate,
) -> Result<(), String> {
    let sources = || empty_sources(bindings, channel);
    activate_with(store, channel, facts, local_custody, candidate, sources)
}

/// As `activate`, with the init's sources judged by `sources` under the adoption lock.
pub fn activate_with(
    store: &OStore,
    channel: u64,
    facts: Result<ActivationFacts, String>,
    local_custody: impl FnOnce() -> Result<bool, String>,
    candidate: &Candidate,
    sources: impl FnOnce() -> Result<Vec<InitSource>, String>,
) -> Result<(), String> {
    #[cfg(test)]
    test_hook::run(channel, test_hook::Step::BeforeLock)?;
    let asked = Instant::now();
    let _serial = ACTIVATING.lock().unwrap_or_else(PoisonError::into_inner);
    let mut adoption = candidate.lock();
    let locked = Instant::now();
    let mut spent = Spent::default();
    let result = adopt(
        &mut adoption,
        candidate,
        store,
        (channel, facts),
        local_custody,
        sources,
        &mut spent,
    );
    tracing::info!(
        channel,
        adoption = ?*adoption,
        lock_wait_us = locked.duration_since(asked).as_micros() as u64,
        lock_held_us = locked.elapsed().as_micros() as u64,
        check_us = spent.check,
        write_us = spent.write,
        settle_us = spent.settle,
        "[tui_o] first activation decided the adoption"
    );
    result
}

/// Microseconds spent under the lock on the checks, the store write and a failed write's settling.
#[derive(Default)]
struct Spent {
    check: u64,
    write: u64,
    settle: u64,
}

fn since(at: Instant) -> u64 {
    at.elapsed().as_micros() as u64
}

/// A body admitted by Legacy must finish before O publishes its first init.
pub(super) fn body_in_flight((started, finished): (u64, u64)) -> bool {
    started != finished
}

fn adopt(
    adoption: &mut Adoption,
    candidate: &Candidate,
    store: &OStore,
    (channel, facts): (u64, Result<ActivationFacts, String>),
    local_custody: impl FnOnce() -> Result<bool, String>,
    sources: impl FnOnce() -> Result<Vec<InitSource>, String>,
    spent: &mut Spent,
) -> Result<(), String> {
    if !matches!(*adoption, Adoption::Pending | Adoption::Deferred) {
        return Err(format!("adoption is already {adoption:?}"));
    }
    if body_in_flight(candidate.sends()) {
        return Err("Legacy body send in flight".into());
    }
    let at = Instant::now();
    let checked = facts
        .and_then(|facts| facts.blocker().map_or(Ok(()), Err))
        .and_then(|()| match local_custody()? {
            true => Err("Legacy retains delivery custody".into()),
            false => Ok(()),
        })
        .and_then(|()| sources());
    spent.check = since(at);
    let sources = match checked {
        Ok(sources) => sources,
        Err(detail) => {
            // A deferred channel stays deferred: its host decides whether the refusal is final.
            if *adoption == Adoption::Pending {
                *adoption = Adoption::Released;
            }
            return Err(detail);
        }
    };
    // From here the store may change and an error may follow a published write.
    #[cfg(test)]
    test_hook::run(channel, test_hook::Step::BeforeWrite)?;
    let at = Instant::now();
    let created = create(store, channel, sources);
    #[cfg(test)]
    let created = created.and_then(|()| test_hook::run(channel, test_hook::Step::AfterWrite));
    spent.write = since(at);
    let Err(detail) = created else {
        *adoption = Adoption::Committed;
        return Ok(());
    };
    let at = Instant::now();
    *adoption = settled(store, channel);
    spent.settle = since(at);
    match *adoption {
        Adoption::Committed => {
            tracing::error!(
                channel,
                detail,
                "[tui_o] the init is readable after a failed write"
            );
            Ok(())
        }
        _ => Err(detail),
    }
}

/// After a failed write the store as it now reads decides: an init that recovers commits, no
/// trace of the channel releases it, and anything else holds it.
fn settled(store: &OStore, channel: u64) -> Adoption {
    let recovered = match store.read_era() {
        Ok(Some(era)) => match store.open_channel(&era, channel) {
            Ok(Some(opened)) => Some(opened.init().channel == channel),
            Ok(None) => None,
            Err(_) => Some(false),
        },
        Ok(None) => match store.read_init(channel) {
            Ok(None) => None,
            _ => Some(false),
        },
        Err(_) => Some(false),
    };
    match recovered {
        Some(true) => Adoption::Committed,
        None if !store.has_channel_dir(channel) => Adoption::Released,
        _ => Adoption::Held,
    }
}

fn create(store: &OStore, channel: u64, sources: Vec<InitSource>) -> Result<(), String> {
    if store.has_channel_dir(channel) {
        return Err("the channel has store files but no init".into());
    }
    let init = Initialized {
        channel,
        sources,
        initial_anchor: 0,
        build_digest: env!("CARGO_PKG_VERSION").into(),
        at: Utc::now(),
    };
    let era = store
        .read_era()
        .map_err(|error| format!("era: {error:?}"))?;
    let created = match era {
        Some(_) => store.init_channel(&init),
        None => store
            .begin_era(&[channel], init.at, |_| Ok(init.clone()))
            .map(drop),
    };
    created.map_err(|error| format!("init: {error:?}"))?;
    let count = init.sources.len();
    tracing::info!(
        channel,
        sources = count,
        "[tui_o] writer host created the channel's init"
    );
    Ok(())
}

/// Every source the log names must still be empty, and no bind may be left pending.
fn empty_sources<B: BindingEvents>(bindings: &B, channel: u64) -> Result<Vec<InitSource>, String> {
    let events = bindings.binding_events_since(channel, 0);
    let events = events.map_err(|error| format!("binding log: {error}"))?;
    let events = super::renumbered::first_named(events);
    let (bound, named) = logged(&events).map_err(|refused| refused.to_string())?;
    for source in bound.iter().chain(&named) {
        still_empty(source)?;
    }
    let empty_hash = hex::encode(Sha256::digest(b""));
    let mut attached: Vec<InitSource> = Vec::new();
    for source in bound {
        if !attached.iter().any(|s| s.source_id == *source) {
            attached.push(InitSource {
                source_id: source.clone(),
                delivery_start: 0,
                prefix_hash: empty_hash.clone(),
            });
        }
    }
    Ok(attached)
}

pub(super) fn still_empty(source: &SourceId) -> Result<(), String> {
    let path = source.path.display();
    let current = source_id_for(&source.session_id, &source.path);
    let current = current.map_err(|error| format!("source {path}: {error}"))?;
    // An empty file needs no bytes to prove it is the same after a renumbered dev.
    if !same_file(&current, source) {
        return Err(format!("source {path} was replaced"));
    }
    let len = std::fs::metadata(&source.path).map_err(|error| format!("source {path}: {error}"));
    match len?.len() {
        0 => Ok(()),
        len => Err(format!("source {path} already holds {len} bytes")),
    }
}

/// Pauses a first activation at a step or fails it there; the write steps run under the adoption lock.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::sync::Mutex;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Step {
        BeforeLegacyStarted,
        /// As an adoption starts pinning the sources of a channel that already holds output.
        Snapshot,
        /// Deferred retry entry; the clocks start at this instant.
        DeferredStarted,
        /// Before the next retry sleep, after the preceding cycle finished.
        DeferredTick,
        BeforeFence,
        AfterFence,
        /// Before the serial and adoption locks are taken.
        BeforeLock,
        BeforeWrite,
        AfterWrite,
    }

    type Hook = Box<dyn FnOnce() -> Result<(), String> + Send>;
    static HOOKS: Mutex<Vec<(u64, Step, Hook)>> = Mutex::new(Vec::new());

    pub(crate) fn set(
        channel: u64,
        step: Step,
        hook: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        HOOKS.lock().unwrap().push((channel, step, Box::new(hook)));
    }

    pub(in crate::services::tui_o) fn run(channel: u64, step: Step) -> Result<(), String> {
        let mut hooks = HOOKS.lock().unwrap();
        let Some(at) = hooks
            .iter()
            .position(|(c, s, _)| *c == channel && *s == step)
        else {
            return Ok(());
        };
        let (_, _, hook) = hooks.remove(at);
        drop(hooks);
        hook()
    }
}
