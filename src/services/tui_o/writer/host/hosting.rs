//! What a home's watch reads of a hosted writer: the claim that hosts the channel, whether the
//! host still runs, and whether its first init could run once Owned, judged without writing.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{HostIo, ManagedWriterHandle, Readiness, first_init, locked};
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::adoption::{self, Hold};
use crate::services::tui_o::writer::binding::BindingEvents;
use crate::services::tui_o::writer::{activation, late_adopt, renumbered};

/// The first-init precheck of one hosted channel: why it could not run once Owned, if so.
pub(crate) type Ready = Arc<dyn Fn() -> Result<(), &'static str> + Send + Sync>;

/// Test-only mutants of the precheck and of a writer's liveness.
#[cfg(test)]
fn mutant(name: &str) -> bool {
    std::env::var("ADK_S3B_MUTANT").ok().as_deref() == Some(name)
}

impl Readiness {
    /// The generation whose claim hosts `channel`, read now. A claim is not a live host: one whose
    /// actor ended abnormally keeps it. No claim does mean no host, as each claims before it spawns.
    pub fn hosted_by(&self, channel: u64) -> Option<u64> {
        locked(&self.hosted).get(&channel).copied()
    }
}

impl ManagedWriterHandle {
    /// Whether the host task still runs and was not told to stop.
    pub fn live(&self) -> bool {
        #[cfg(test)]
        if mutant("writer_liveness_ignored") {
            return true;
        }
        !self.host.is_finished() && !*self.stop.signal.borrow()
    }

    /// The precheck of the channel's first init, run by the caller: why the init could not run
    /// once its gate is Owned; `Ok` when only the gate and the facts read then stand in the way.
    pub fn ready(&self) -> Ready {
        Arc::clone(&self.ready)
    }

    /// Leaves the late reservation unsettled when this host ends, for a successor that takes it
    /// over; set before the stop is signalled.
    pub fn keep_reservation(&self) {
        self.keep.store(true, Ordering::SeqCst);
    }
}

pub(super) fn ready_probe<I: HostIo>(
    io: &Arc<I>,
    channel: u64,
    provider: ShadowProvider,
    candidate: &Candidate,
) -> Ready {
    let (io, candidate) = (Arc::clone(io), candidate.clone());
    Arc::new(move || judged(&*io, channel, provider, &candidate))
}

/// The adoption kinds a first init runs from, then what the first init itself checks before it
/// writes and could check now: the bound sources, the late flag and the late binding.
fn judged<I: HostIo>(
    io: &I,
    channel: u64,
    provider: ShadowProvider,
    candidate: &Candidate,
) -> Result<(), &'static str> {
    #[cfg(test)]
    let ignores_candidate = mutant("adopt_ignores_candidate");
    #[cfg(not(test))]
    let ignores_candidate = false;
    match candidate.peek() {
        Adoption::Committed => Ok(()),
        Adoption::Deferred if candidate.late_adopt_origin() => late(io, channel, provider),
        _ if ignores_candidate => Ok(()),
        Adoption::Released => Err("adopt_refused:released"),
        Adoption::Held => Err("adopt_refused:held"),
        Adoption::Deferred => Err("adopt_refused:deferred"),
        #[cfg(test)]
        Adoption::Pending if mutant("pending_always_executable") => Ok(()),
        Adoption::Pending => {
            let log = io.bindings(channel, provider);
            match provider {
                ShadowProvider::Codex => codex(&*log, channel),
                ShadowProvider::Claude => claude(&*log, channel),
            }
        }
    }
}

/// A late reservation adopts only while late adoption is on now, and only off a bound Claude
/// source: a late Codex rollout must be bound after a baseline its first Owned read takes.
fn late<I: HostIo>(io: &I, channel: u64, provider: ShadowProvider) -> Result<(), &'static str> {
    #[cfg(test)]
    let enabled = io.late_adopt_enabled() || mutant("ignore_late_enabled");
    #[cfg(not(test))]
    let enabled = io.late_adopt_enabled();
    if !enabled {
        return Err("adopt_refused:late_disabled");
    }
    if provider == ShadowProvider::Codex {
        return Err("adopt_refused:late_awaits_turn");
    }
    let log = io.bindings(channel, provider);
    let events = log.binding_events_since(channel, 0);
    let events = events.map_err(|_| "adopt_refused:binding_unreadable")?;
    let bound = late_adopt::first_bound(&renumbered::first_named(events));
    bound.map(drop).ok_or("adopt_refused:late_awaits_binding")
}

/// A pending Codex init may start with no source bound yet, or over sources holding metadata only.
fn codex<B: BindingEvents>(log: &B, channel: u64) -> Result<(), &'static str> {
    log.binding_events_since(channel, 0)
        .map_err(|_| "adopt_refused:binding_unreadable")?;
    #[cfg(test)]
    if mutant("pending_codex_requires_bound_source")
        && log
            .binding_events_since(channel, 0)
            .is_ok_and(|events| adoption::logged(&renumbered::first_named(events)).is_err())
    {
        return Err("adopt_refused:no_bound_source");
    }
    let sources = first_init::codex_sources(log, channel);
    sources
        .map(drop)
        .map_err(|_| "adopt_refused:source_not_metadata")
}

/// A pending Claude init needs every bound source still empty, unless its sources already hold
/// output, which the adoption of a running Legacy session judges once Owned.
fn claude<B: BindingEvents>(log: &B, channel: u64) -> Result<(), &'static str> {
    if adoption::holds_output(log, channel) {
        return Ok(());
    }
    let events = log.binding_events_since(channel, 0);
    let events = events.map_err(|_| "adopt_refused:binding_unreadable")?;
    if let Err(refused) = adoption::logged(&renumbered::first_named(events)) {
        return Err(match refused.hold {
            Hold::Binding => "adopt_refused:binding_pending",
            _ => "adopt_refused:no_bound_source",
        });
    }
    let sources = activation::empty_sources(log, channel);
    sources
        .map(drop)
        .map_err(|_| "adopt_refused:source_not_empty")
}
