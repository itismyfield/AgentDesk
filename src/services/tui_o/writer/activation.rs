//! First activation of a selected channel that has no O store yet. Its `init` (and the era, on a
//! fresh store) is created only for a new, empty channel; anything else holds the channel.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::binding::{BindingEvents, BindingRecord, BindingTarget};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::{InitSource, Initialized, OStore};

/// What the gateway reports about a channel before its first activation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivationFacts {
    pub open_intake: i64,
    pub runner_sessions: i64,
    pub node_override: Option<String>,
    pub legacy_custody: bool,
}

impl ActivationFacts {
    fn blocker(&self) -> Option<String> {
        if self.open_intake != 0 {
            return Some(format!("{} open intake rows", self.open_intake));
        }
        if self.runner_sessions != 0 {
            return Some(format!("{} sessions on another node", self.runner_sessions));
        }
        if let Some(node) = &self.node_override {
            return Some(format!("node override to {node}"));
        }
        self.legacy_custody
            .then(|| "Legacy retains delivery custody".to_string())
    }
}

/// Serializes first activations so two channels never race the era seal.
static ACTIVATING: Mutex<()> = Mutex::new(());

/// Creates the channel's `init` over every source its binding log binds, each still empty, and
/// seals the era first time round. Returns why the channel is held instead.
pub fn activate<B: BindingEvents>(
    store: &OStore,
    channel: u64,
    facts: &ActivationFacts,
    bindings: &B,
) -> Result<(), String> {
    if let Some(blocker) = facts.blocker() {
        return Err(blocker);
    }
    let sources = empty_sources(bindings, channel)?;
    let _serial = ACTIVATING.lock().unwrap_or_else(PoisonError::into_inner);
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
