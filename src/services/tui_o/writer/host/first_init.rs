//! A channel's first activation: the init it starts from, written only under the gate's admission
//! and only unless Legacy took it.
//! A Codex channel may start with no source bound yet, or over sources holding only metadata.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::watch;

use super::{Custody, HostIo, hold, owned_now, release, stop, unless_stopped, until_owned};
use crate::services::tui_o::channel_policy::Candidate;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::capture::{FileMatch, file_match};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::{ChannelStore, InitSource, OStore};
use crate::services::tui_o::writer::activation::{self, ActivationFacts};
use crate::services::tui_o::writer::adoption::{self, logged};
use crate::services::tui_o::writer::binding::{
    BindingEvent, BindingEvents, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::codex_adoption::scan::{Scanned, scan};
use crate::services::tui_o::writer::codex_adoption::{Parse, Suffix};
use crate::services::tui_o::writer::{deferred, late_adopt, renumbered};

use super::recover::{Recovered, recover_boot};

/// Bytes a channel's metadata prefixes may hold; the read runs under the adoption lock.
const METADATA_BUDGET: u64 = 1 << 20;

/// The store a first activation committed and the binding log it judged.
pub(super) struct Started<B> {
    pub(super) store: ChannelStore,
    pub(super) bindings: Arc<B>,
}

/// Activates `fresh` for `channel`; `None` once the channel was held, released or told to stop.
pub(super) async fn start<I: HostIo>(
    io: &Arc<I>,
    (channel, provider): (u64, ShadowProvider),
    runtime_root: &Path,
    fresh: OStore,
    candidate: &Candidate,
    (gate, stopping): (&Arc<OwnershipGate>, &watch::Receiver<bool>),
) -> Option<Started<I::Bindings>> {
    let alarms = io.alarms();
    let log = io.bindings(channel, provider);
    if candidate.late_adopt_origin() {
        let late = late_adopt::activate(&**io, channel, provider, &fresh, &*log, candidate, gate);
        if unless_stopped(stopping, late).await != Some(true) {
            return None;
        }
    } else {
        let held = provider == ShadowProvider::Claude && adoption::holds_output(&*log, channel);
        let legacy = held.then(|| io.legacy());
        if let Some(legacy) = &legacy
            && unless_stopped(stopping, adoption::legacy_started(&**legacy)).await != Some(true)
        {
            release(candidate, &alarms, channel, "legacy cursor not established");
            return None;
        }
        let created = loop {
            let facts = loop {
                unless_stopped(stopping, until_owned(gate)).await?;
                let facts =
                    unless_stopped(stopping, io.activation_facts(channel, provider)).await?;
                // Facts read before a lost gate are not acted on; wait for Owned and read again.
                if owned_now(gate) {
                    break facts;
                }
            };
            let local = || Ok(io.local_custody(channel, provider)? != Custody::Free);
            let decided = match legacy.clone() {
                None if provider == ShadowProvider::Codex => {
                    let sources = || codex_sources(&*log, channel);
                    admitted(channel, gate, || {
                        activation::activate_with(&fresh, channel, facts, local, candidate, sources)
                    })
                }
                None => admitted(channel, gate, || {
                    activation::activate(&fresh, channel, facts, &*log, local, candidate)
                }),
                Some(legacy) => 'held: {
                    // Sessions on another node or an override end it before an open turn defers it.
                    let last = facts.as_ref().ok().and_then(ActivationFacts::final_blocker);
                    if let Some(detail) = last {
                        release(candidate, &alarms, channel, &detail);
                        return None;
                    }
                    let events = log.binding_events_since(channel, 0);
                    let events = events.map_err(|error| format!("binding log: {error}"));
                    // O's own copy names each file once; the shared log keeps every dev it read.
                    let events = events.map(renumbered::first_named);
                    let seq = events.as_ref().map_or(0, |e| e.last().map_or(0, |e| e.seq));
                    let current = events.as_deref().ok().and_then(adoption::current);
                    let first = deferred::first(&**io, channel, provider, &legacy, events);
                    let snapshot = match unless_stopped(stopping, first).await? {
                        deferred::First::Adopt(snapshot) => snapshot,
                        deferred::First::Wait(refused) => {
                            let detail = refused.to_string();
                            let Some((source, _)) = current else {
                                release(candidate, &alarms, channel, &detail);
                                return None;
                            };
                            if !candidate.defer(channel) {
                                release(candidate, &alarms, channel, &detail);
                                return None;
                            }
                            tracing::info!(channel, %refused, "[tui_o] adoption waits for Legacy");
                            let waiting = deferred::Waiting {
                                io: &**io,
                                channel,
                                provider,
                                candidate,
                                gate: &**gate,
                                store: &fresh,
                                log: &*log,
                                legacy,
                                bound: (source, seq),
                                late: false,
                            };
                            let retried = deferred::retry(waiting, refused, seq);
                            if unless_stopped(stopping, retried).await != Some(true) {
                                return None;
                            }
                            break 'held Some(Ok(()));
                        }
                        deferred::First::Leave(refused) => {
                            release(candidate, &alarms, channel, &refused.to_string());
                            return None;
                        }
                    };
                    let sources = || {
                        snapshot
                            .recheck(&*legacy, &*log, channel)
                            .map_err(String::from)
                    };
                    admitted(channel, gate, || {
                        activation::activate_with(&fresh, channel, facts, local, candidate, sources)
                    })
                }
            };
            // A gate closed after the facts were judged wrote nothing; wait for Owned again.
            if let Some(created) = decided {
                break created;
            }
        };
        if let Err(detail) = created {
            stop(
                candidate,
                &alarms,
                channel,
                &format!("first activation: {detail}"),
            );
            return None;
        }
    }
    match recover_boot(runtime_root, channel, stopping).await? {
        Ok(Recovered::Store(store)) => {
            io.adopted(channel, provider);
            let bindings = log;
            Some(Started { store, bindings })
        }
        Ok(Recovered::Fresh(_)) => {
            hold(&alarms, channel, "init missing after activation");
            None
        }
        Err(error) => {
            hold(&alarms, channel, &error.detail);
            None
        }
    }
}

/// Writes the first `init` under the gate's admission: a drain's close or a lease transition waits
/// for its synchronous I/O and the activation serial lock. `None` once the gate is not Owned.
fn admitted(
    _channel: u64,
    gate: &OwnershipGate,
    activate: impl FnOnce() -> Result<(), String>,
) -> Option<Result<(), String>> {
    #[cfg(test)]
    if std::env::var("ADK_S3B_MUTANT").ok().as_deref() == Some("first_init_unfenced") {
        return Some(activate());
    }
    #[cfg(test)]
    {
        use activation::test_hook::{Step, note_refused, run};
        if let Err(detail) = run(_channel, Step::BeforeAdmit) {
            return Some(Err(detail));
        }
        let admitted = gate.admit(|_| activate());
        if admitted.is_none() {
            note_refused(_channel);
        }
        admitted
    }
    #[cfg(not(test))]
    gate.admit(|_| activate())
}

/// A Codex init judged under the adoption lock: none while the typed log names no source, else
/// each bound source from the end of a prefix holding only metadata.
pub(super) fn codex_sources<B: BindingEvents>(
    bindings: &B,
    channel: u64,
) -> Result<Vec<InitSource>, String> {
    let events = bindings.binding_events_since(channel, 0);
    let events = events.map_err(|error| format!("binding log: {error}"))?;
    if !events.iter().any(names_source) {
        return Ok(Vec::new());
    }
    let events = renumbered::first_named(events);
    let (bound, named) = logged(&events).map_err(|refused| refused.to_string())?;
    let mut budget = METADATA_BUDGET;
    let mut sources: Vec<InitSource> = Vec::new();
    for source in bound.iter().chain(&named) {
        if sources.iter().any(|s| s.source_id == **source) {
            continue;
        }
        let scanned = metadata_only(source, &mut budget)?;
        if bound.contains(source) {
            sources.push(InitSource {
                source_id: (*source).clone(),
                delivery_start: scanned.len,
                prefix_hash: scanned.hash,
            });
        }
    }
    Ok(sources)
}

/// Whether `event` names a source as bound, resolved, replaced or parent.
fn names_source(event: &BindingEvent) -> bool {
    match &event.record {
        BindingRecord::Bound {
            old,
            new,
            parent_hint,
            ..
        } => old.is_some() || parent_hint.is_some() || matches!(new, BindingTarget::Source(_)),
        BindingRecord::Resolved { .. } => true,
        BindingRecord::Rejected { .. } => false,
    }
}

/// `source` read whole as complete session, turn and token-count records of the same file.
fn metadata_only(source: &SourceId, budget: &mut u64) -> Result<Scanned, String> {
    let path = source.path.display();
    let scanned = scan(&source.path, *budget)?;
    *budget -= scanned.len;
    if file_match(source, scanned.file) == FileMatch::Other {
        return Err(format!("source {path} was replaced"));
    }
    if scanned.prefix != Parse::Strict || scanned.suffix(0) != Suffix::Quiet {
        let len = scanned.len;
        return Err(format!(
            "source {path} holds more than metadata in {len} bytes"
        ));
    }
    Ok(scanned)
}
