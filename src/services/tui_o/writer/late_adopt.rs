//! Late adoption keeps Legacy ownership until the normal fenced cursor handoff.

use tokio::sync::watch;

use super::adoption::{self, Hold, Refused};
use super::binding::{BindingEvent, BindingEvents, BindingRecord, BindingTarget};
use super::deferred::{self, Waiting};
use super::host::{Custody, HostIo, release, until_owned};
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::OStore;

mod codex;

/// Subscribe before reading; a closed notice still gets the bounded disk fallback.
pub(super) struct Wake {
    binding: watch::Receiver<u64>,
    config: watch::Receiver<Option<std::sync::Arc<crate::config::Config>>>,
    binding_open: bool,
    config_open: bool,
}

impl Wake {
    pub(super) fn new(log: &impl BindingEvents, channel: u64) -> Self {
        Self {
            binding: log.subscribe(channel),
            config: crate::config_live_reload::subscribe(),
            binding_open: true,
            config_open: true,
        }
    }

    pub(super) async fn wait(&mut self) {
        let retry = std::time::Duration::from_secs(5);
        tokio::select! {
            result = self.binding.changed(), if self.binding_open => {
                self.binding_open = result.is_ok();
            }
            result = self.config.changed(), if self.config_open => { self.config_open = result.is_ok(); }
            () = tokio::time::sleep(retry) => {}
        }
    }
}

pub(super) fn first_bound(events: &[BindingEvent]) -> Option<(SourceId, u64)> {
    events.iter().find_map(|event| match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => Some((source.clone(), event.seq)),
        _ => None,
    })
}

/// The source last bound at or before `boundary`, else the first bound after it, with its seq.
pub(super) fn baseline(events: &[BindingEvent], boundary: u64) -> Option<(SourceId, u64)> {
    let binds = events.iter().filter_map(|event| match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => Some((source, event.seq)),
        _ => None,
    });
    let within = binds.clone().take_while(|(_, seq)| *seq <= boundary).last();
    let bound = within.or_else(|| binds.clone().find(|(_, seq)| *seq > boundary));
    bound.map(|(source, seq)| (source.clone(), seq))
}

/// The reservation's boundary: the end of its first read when this process's Legacy had started
/// no body and held no custody for the channel, else 0 so the whole log is the baseline.
fn reservation_boundary<I: HostIo>(
    io: &I,
    channel: u64,
    provider: ShadowProvider,
    candidate: &Candidate,
    events: &[BindingEvent],
) -> u64 {
    candidate.fix_late_boundary(|| {
        let end = events.last().map_or(0, |event| event.seq);
        let started = || {
            let _adoption = candidate.lock();
            candidate.sends().0
        };
        // Sends are read again after custody, so a body sent during that read still counts.
        let before = started();
        let custody = io.local_custody(channel, provider);
        let clean = before == 0 && matches!(custody, Ok(Custody::Free)) && started() == 0;
        let boundary = if clean { end } else { 0 };
        tracing::info!(
            channel,
            boundary,
            end,
            ?custody,
            "[tui_o] late reservation boundary fixed"
        );
        boundary
    })
}

pub(crate) async fn activate<I: HostIo>(
    io: &I,
    channel: u64,
    provider: ShadowProvider,
    store: &OStore,
    log: &I::Bindings,
    candidate: &Candidate,
    gate: &OwnershipGate,
) -> bool {
    if provider == ShadowProvider::Codex {
        return codex::activate(io, channel, store, log, candidate, gate).await;
    }
    let alarms = io.alarms();
    let mut wake = Wake::new(log, channel);
    let mut checked = false;
    loop {
        if candidate.peek() != Adoption::Deferred {
            return false;
        }
        if io.late_adopt_enabled() {
            until_owned(gate).await;
            if !checked {
                let expected = gate.current();
                let facts = io.activation_facts(channel, provider).await;
                if gate.current() != expected || !super::host::owned_now(gate) {
                    continue;
                }
                if let Ok(facts) = facts {
                    if let Some(detail) = facts.final_blocker() {
                        release(candidate, &alarms, channel, &detail);
                        return false;
                    }
                    checked = true;
                }
            }
            let events = log
                .binding_events_since(channel, 0)
                .map(super::renumbered::first_named);
            let events = match events {
                Ok(events) => events,
                Err(detail) => {
                    release(candidate, &alarms, channel, &detail);
                    return false;
                }
            };
            let boundary = reservation_boundary(io, channel, provider, candidate, &events);
            if let Some(bound) = baseline(&events, boundary) {
                let seq = events.last().map_or(0, |event| event.seq);
                let waiting = Waiting {
                    io,
                    channel,
                    provider,
                    candidate,
                    gate,
                    store,
                    log,
                    legacy: io.legacy(),
                    bound,
                    late: true,
                };
                return deferred::retry(
                    waiting,
                    Refused::retry("late binding awaits safe Legacy cursor"),
                    seq,
                )
                .await;
            }
            let no_bound_source = first_bound(&events).is_none()
                && !events.iter().any(|event| {
                    matches!(
                        &event.record,
                        BindingRecord::Bound {
                            new: BindingTarget::Pending { .. },
                            ..
                        }
                    )
                });
            match adoption::logged(&events) {
                Err(refused) if refused.hold == Hold::Binding => {}
                Err(_) if no_bound_source => {}
                Err(refused) => {
                    release(candidate, &alarms, channel, &refused.to_string());
                    return false;
                }
                Ok(_) => unreachable!("a bound source supplies the baseline"),
            }
        }
        wake.wait().await;
    }
}
