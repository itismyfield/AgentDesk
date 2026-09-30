//! First activation of a selected channel that has no O store yet. Its `init` (and the era, on a
//! fresh store) is created only for a new, empty channel still pending adoption. A check that
//! fails before any store write releases the channel to Legacy; a failed write holds it with O.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::binding::{BindingEvents, BindingRecord, BindingTarget};
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
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
        if self.open_intake != 0 {
            return Some(format!("{} open intake rows", self.open_intake));
        }
        if self.runner_sessions != 0 {
            return Some(format!("{} sessions on another node", self.runner_sessions));
        }
        self.node_override
            .as_ref()
            .map(|node| format!("node override to {node}"))
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
    let asked = Instant::now();
    let _serial = ACTIVATING.lock().unwrap_or_else(PoisonError::into_inner);
    let mut adoption = candidate.lock();
    let locked = Instant::now();
    let result = adopt(
        &mut adoption,
        store,
        channel,
        facts,
        bindings,
        local_custody,
    );
    tracing::info!(
        channel,
        adoption = ?*adoption,
        lock_wait_us = locked.duration_since(asked).as_micros() as u64,
        lock_held_us = locked.elapsed().as_micros() as u64,
        "[tui_o] first activation decided the adoption"
    );
    result
}

fn adopt<B: BindingEvents>(
    adoption: &mut Adoption,
    store: &OStore,
    channel: u64,
    facts: Result<ActivationFacts, String>,
    bindings: &B,
    local_custody: impl FnOnce() -> Result<bool, String>,
) -> Result<(), String> {
    if *adoption != Adoption::Pending {
        return Err(format!("adoption is already {adoption:?}"));
    }
    let checked = facts
        .and_then(|facts| facts.blocker().map_or(Ok(()), Err))
        .and_then(|()| match local_custody()? {
            true => Err("Legacy retains delivery custody".into()),
            false => Ok(()),
        })
        .and_then(|()| empty_sources(bindings, channel));
    let sources = match checked {
        Ok(sources) => sources,
        Err(detail) => {
            *adoption = Adoption::Released;
            return Err(detail);
        }
    };
    // From here the store may change and an error may follow a published write.
    #[cfg(test)]
    test_hook::run(channel, test_hook::Step::BeforeWrite)?;
    let created = create(store, channel, sources);
    #[cfg(test)]
    let created = created.and_then(|()| test_hook::run(channel, test_hook::Step::AfterWrite));
    *adoption = match created {
        Ok(()) => Adoption::Committed,
        Err(_) => Adoption::Held,
    };
    created
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
    let (mut bound, mut named, mut pending) = (Vec::new(), Vec::new(), BTreeSet::new());
    for event in &events {
        match &event.record {
            BindingRecord::Bound {
                old,
                new,
                parent_hint,
                ..
            } => {
                named.extend(old.iter().chain(parent_hint));
                match new {
                    BindingTarget::Source(source) => bound.push(source),
                    BindingTarget::Pending { .. } => {
                        pending.insert(event.seq);
                    }
                }
            }
            BindingRecord::Resolved {
                resolves_seq,
                source,
            } => {
                pending.remove(resolves_seq);
                bound.push(source);
            }
            BindingRecord::Rejected { .. } => {}
        }
    }
    if let Some(seq) = pending.first() {
        return Err(format!("bind {seq} is still pending"));
    }
    if bound.is_empty() {
        return Err("no source is bound".into());
    }
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

fn still_empty(source: &SourceId) -> Result<(), String> {
    let path = source.path.display();
    let current = source_id_for(&source.session_id, &source.path);
    let current = current.map_err(|error| format!("source {path}: {error}"))?;
    if current != *source {
        return Err(format!("source {path} was replaced"));
    }
    let len = std::fs::metadata(&source.path).map_err(|error| format!("source {path}: {error}"));
    match len?.len() {
        0 => Ok(()),
        len => Err(format!("source {path} already holds {len} bytes")),
    }
}

/// Pauses a first activation or fails it after its store write, still under the adoption lock.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::sync::Mutex;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Step {
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

    pub(super) fn run(channel: u64, step: Step) -> Result<(), String> {
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
