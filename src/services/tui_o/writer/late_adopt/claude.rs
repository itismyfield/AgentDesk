//! Claude late takeover of a transcript bound after the reservation's floor and before its first
//! output, and the proof that the transcript Legacy ran before it was delivered to its end.

use super::codex::prefix::Verdict;
use super::codex::{Baseline, Taken, Takeover, bound_after};
use crate::services::tui_o::channel_policy::Candidate;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::{InitOrigin, OStore};
use crate::services::tui_o::writer::binding::BindingEvent;
use crate::services::tui_o::writer::host::HostIo;
use crate::services::tui_o::writer::rotation::bound_source;

mod prefix;

/// Only a transcript bound above `seq` is taken, and only while no Legacy body send started since
/// `sends` was read; `sends` is read before the binding log so no bind precedes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::tui_o::writer) struct Floor {
    pub(in crate::services::tui_o::writer) seq: u64,
    pub(in crate::services::tui_o::writer) sends: (u64, u64),
}

/// What one takeover look found.
pub(super) enum Look {
    Committed,
    Final(String),
    /// The same bind may still be taken; it is judged again.
    Wait(String),
    /// Nothing is taken over; the deferred handoff goes on.
    Pass(String),
}

pub(super) struct Channel<'a, I: HostIo> {
    pub(super) io: &'a I,
    pub(super) channel: u64,
    pub(super) store: &'a OStore,
    pub(super) log: &'a I::Bindings,
    pub(super) candidate: &'a Candidate,
    pub(super) gate: &'a OwnershipGate,
}

/// Commits the transcript bound after `floor` while its bytes hold no output, from its first byte.
pub(super) async fn look<I: HostIo>(
    at: &Channel<'_, I>,
    events: &[BindingEvent],
    floor: Floor,
) -> Look {
    let takeover = Takeover {
        io: at.io,
        channel: at.channel,
        store: at.store,
        log: at.log,
        candidate: at.candidate,
        gate: at.gate,
        provider: ShadowProvider::Claude,
        judge: prefix::judge,
        origin: InitOrigin::LateClaudeTakeover,
    };
    let Some(bound) = bound_after(events, floor.seq) else {
        return Look::Pass("no transcript bound since the floor".into());
    };
    // Turn confirmation finds the taken execution again as the transcript's only bind; a source
    // bound after the floor before it may hold output Legacy has not claimed yet.
    let binds = events.iter().filter(|e| bound_source(e) == Some(&bound.0));
    let since = events
        .iter()
        .filter(|e| e.seq > floor.seq && bound_source(e).is_some());
    if binds.count() != 1 || since.count() != 1 {
        return Look::Pass("the transcript is not the only source bound since the floor".into());
    }
    let prefix = match prefix::judge(&bound.0) {
        Verdict::Eligible(prefix) => prefix,
        Verdict::Wait(detail) => return Look::Wait(detail),
        Verdict::Refuse(detail) => return Look::Pass(detail),
    };
    let at = Baseline {
        sends: floor.sends,
        seq: floor.seq,
        order: 0,
        turn_mark: false,
    };
    match takeover.commit(at, &bound, &prefix).await {
        Ok(()) => Look::Committed,
        Err(Taken::Final(detail)) => Look::Final(detail),
        Err(Taken::Skip(detail)) => Look::Pass(detail),
        Err(Taken::Retry(detail) | Taken::Soon(detail) | Taken::Again(detail)) => {
            Look::Wait(detail)
        }
    }
}

/// A transcript Legacy's late pin found delivered through `len` while it was current, with the
/// Legacy sends read before that look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::tui_o::writer) struct Sample {
    pub(in crate::services::tui_o::writer) source: SourceId,
    pub(in crate::services::tui_o::writer) nonce: String,
    pub(in crate::services::tui_o::writer) len: u64,
    pub(in crate::services::tui_o::writer) sends: (u64, u64),
}

/// A source a rebase left without proof that Legacy delivered it, with the sample taken before.
#[derive(Clone, Debug)]
pub(in crate::services::tui_o::writer) struct Owed {
    pub(in crate::services::tui_o::writer) old: (SourceId, u64),
    pub(in crate::services::tui_o::writer) sample: Option<Sample>,
}

impl Owed {
    /// Only the old source's own proof settles it; the current source's delivery says nothing.
    pub(in crate::services::tui_o::writer) fn settled(
        &self,
        events: &[BindingEvent],
        candidate: &Candidate,
    ) -> Result<Floor, String> {
        old_source_done(self.sample.as_ref(), &self.old, events, candidate)
    }
}

/// The execution nonce of the last bind of `source`.
pub(in crate::services::tui_o::writer) fn nonce_of(
    events: &[BindingEvent],
    source: &SourceId,
) -> Option<String> {
    let bind = events
        .iter()
        .rev()
        .find(|e| bound_source(e) == Some(source));
    bind.map(|event| event.execution_nonce.clone())
}

/// The floor a rebased takeover judges from, once `sample` proves `old` delivered: the same
/// transcript and execution, only metadata appended since, and no Legacy body send started since.
pub(in crate::services::tui_o::writer) fn old_source_done(
    sample: Option<&Sample>,
    old: &(SourceId, u64),
    events: &[BindingEvent],
    candidate: &Candidate,
) -> Result<Floor, String> {
    let sample = sample.ok_or("no delivered sample of the old transcript")?;
    if sample.source != old.0 || nonce_of(events, &old.0).as_ref() != Some(&sample.nonce) {
        return Err("the delivered sample is of another transcript or execution".into());
    }
    prefix::metadata_since(&old.0, sample.len)?;
    let sends = {
        let _adoption = candidate.lock();
        candidate.sends()
    };
    if sends.0 != sample.sends.0 {
        return Err("a Legacy body send started since the delivered sample".into());
    }
    Ok(Floor {
        seq: old.1,
        sends: sample.sends,
    })
}
