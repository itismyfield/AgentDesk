//! Claude late takeover of a transcript bound after the reservation's floor and before its first
//! output, and the proof that the transcript Legacy ran before it was delivered to its end.

use super::codex::prefix::Verdict;
use super::codex::{Baseline, Taken, Takeover, bound_after};
use crate::services::tui_o::channel_policy::Candidate;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::{InitOrigin, OStore};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::host::HostIo;
use crate::services::tui_o::writer::rotation::bound_source;

mod prefix;
pub(in crate::services::tui_o::writer) mod thread {
    use sha2::{Digest, Sha256};

    use crate::services::tui_o::cutover::claude_thread::protected_launch;
    use crate::services::tui_o::shadow::{
        SourceId,
        binding_reader::source_id_for,
        capture::{SourceCapture, same_file},
    };
    use crate::services::tui_o::store::{InitSource, spool::source_key};
    use crate::services::tui_o::writer::{
        adoption::{self, LegacyEpoch},
        binding::{BindingEvent, BindingRecord, BindingTarget},
        historical_hops,
        rotation::bound_source,
    };
    use crate::services::tui_prompt_dedupe::binding_events::claude_fold;

    use super::super::codex::prefix::{Prefix, PrefixClass, Verdict};

    pub(in crate::services::tui_o::writer) fn is_protected(events: &[BindingEvent]) -> bool {
        events.first().is_some_and(|event| {
            !claude_fold::read_thread_resumes(event.channel_id)
                .is_ok_and(|entries| entries.is_empty())
        }) || events.iter().any(|event| {
            !matches!(event.record, BindingRecord::Rejected { .. })
                && protected_launch(event).is_some()
        })
    }

    pub(in crate::services::tui_o::writer) fn plan(
        events: &[BindingEvent],
        parent: u64,
    ) -> Option<((SourceId, u64), Vec<InitSource>)> {
        let (current, tmux) = adoption::current(events)?;
        let latest = events
            .iter()
            .rev()
            .find(|event| bound_source(event) == Some(&current))?;
        let resumes = claude_fold::read_thread_resumes(latest.channel_id).ok()?;
        let verified = claude_fold::verified_claude_sources(latest.channel_id).ok()?;
        let hops = historical_hops::fold(events, latest.seq);
        let mut sources = Vec::<InitSource>::new();
        let empty = hex::encode(Sha256::digest(b""));
        for event in events {
            let scope = protected_launch(event)
                .filter(|ctx| ctx.claude_thread_writer_parent() == Some(parent));
            if let Some(scope) = scope.as_ref()
                && matches!(&event.record, BindingRecord::Bound { new: BindingTarget::Pending { .. }, .. })
                && !verified.iter().any(|proof| {
                    proof.tmux_session == event.tmux_session
                        && proof.execution_nonce.as_deref() == Some(&event.execution_nonce)
                        && matches!(&proof.new, crate::services::tui_prompt_dedupe::binding_events::BindingTarget::Source(source)
                            | crate::services::tui_prompt_dedupe::binding_events::BindingTarget::Resolved { source, .. }
                            if scope.expected_native_session_id.as_deref() == Some(source.session_id.as_str()))
                })
            {
                return None;
            }
            let Some(source) = bound_source(event) else {
                continue;
            };
            let fresh = scope.is_some_and(|ctx| {
                ctx.expected_native_session_id.as_deref() == Some(source.session_id.as_str())
            });
            let successor = sources.last().is_some_and(|previous| {
                hops.get(&source_key(&previous.source_id))
                    .is_some_and(|(_, hop)| {
                        hop.tmux_session.as_deref() == Some(event.tmux_session.as_str())
                            && same_file(&hop.source, source)
                    })
            });
            let resume = resumes.iter().find(|entry| {
                entry.parent == parent
                    && entry.tmux == event.tmux_session
                    && same_file(&entry.source, source)
            });
            if !fresh && !successor && resume.is_none() {
                continue;
            }
            // A same-file tmux resume need not produce a new P5 record.
            let confirmed = verified.iter().any(|proof| {
                claude_fold::names_source(proof, &event.tmux_session, source)
                    && (!(fresh || successor)
                        || proof.execution_nonce.as_deref() == Some(&event.execution_nonce))
            });
            if !confirmed {
                return None;
            }
            let (delivery_start, prefix_hash) = if fresh || successor {
                (0, empty.clone())
            } else {
                let resume = resume?;
                (resume.offset, resume.sha256.clone())
            };
            if let Some(old) = sources
                .iter_mut()
                .find(|old| same_file(&old.source_id, source))
            {
                if delivery_start < old.delivery_start {
                    old.delivery_start = delivery_start;
                    old.prefix_hash = prefix_hash;
                }
            } else {
                sources.push(InitSource {
                    source_id: source.clone(),
                    delivery_start,
                    prefix_hash,
                });
            }
        }
        if resumes.iter().any(|entry| {
            entry.parent == parent
                && entry.tmux == tmux
                && !sources
                    .iter()
                    .any(|source| same_file(&source.source_id, &entry.source))
        }) || !sources
            .last()
            .is_some_and(|source| same_file(&source.source_id, &current))
        {
            return None;
        }
        sources
            .windows(2)
            .all(|pair| {
                hops.get(&source_key(&pair[0].source_id))
                    .is_some_and(|(_, hop)| same_file(&hop.source, &pair[1].source_id))
            })
            .then_some(((current, latest.seq), sources))
    }

    pub(super) fn source(source: &SourceId) -> Verdict {
        match source_id_for(&source.session_id, &source.path) {
            Ok(actual) if same_file(&actual, source) => Verdict::Eligible(Prefix {
                class: PrefixClass::OutputFree,
                len: 0,
                sha256: hex::encode(Sha256::digest(b"")),
                prompt_digest: None,
                prompt_end: None,
            }),
            Ok(_) => Verdict::Refuse("the reserved transcript was replaced".into()),
            Err(error) => Verdict::Wait(format!("the reserved transcript is unavailable: {error}")),
        }
    }

    pub(in crate::services::tui_o::writer) fn verify(sources: &[InitSource]) -> Result<(), String> {
        for source in sources {
            SourceCapture::reopen(
                source.source_id.clone(),
                source.delivery_start,
                &source.prefix_hash,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(in crate::services::tui_o::writer) fn owns_row(
        epoch: LegacyEpoch,
        sources: &[InitSource],
    ) -> bool {
        let (Some(turn), Some(delivery)) = (epoch.turn, epoch.delivery) else {
            return false;
        };
        turn.3.is_some_and(|start| {
            sources.iter().any(|source| {
                start >= source.delivery_start && delivery.source_path == source.source_id.path
            })
        })
    }
}

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
    let protected_parent = at
        .candidate
        .child_surface()
        .filter(|_| thread::is_protected(events))
        .map(|surface| surface.parent());
    let bound = if let Some(parent) = protected_parent {
        thread::plan(events, parent).map(|(bound, _)| bound)
    } else {
        takeover_binding(events, floor.seq)
    };
    let Some(bound) = bound else {
        return Look::Pass("no unique transcript bound since the floor".into());
    };
    let takeover = Takeover {
        io: at.io,
        channel: at.channel,
        store: at.store,
        log: at.log,
        candidate: at.candidate,
        gate: at.gate,
        provider: ShadowProvider::Claude,
        judge: if protected_parent.is_some() {
            thread::source
        } else {
            prefix::judge
        },
        origin: InitOrigin::LateClaudeTakeover,
        protected_parent,
    };
    let prefix = match (takeover.judge)(&bound.0) {
        Verdict::Eligible(prefix) => prefix,
        Verdict::Wait(detail) => return Look::Wait(detail),
        Verdict::Refuse(detail) => return Look::Pass(detail),
    };
    let at = Baseline {
        sends: if protected_parent.is_some() {
            at.candidate.sends()
        } else {
            floor.sends
        },
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

/// Source verification can confirm a bind without introducing another execution.
pub(super) fn takeover_binding(events: &[BindingEvent], after: u64) -> Option<(SourceId, u64)> {
    let (source, _) = bound_after(events, after)?;
    let mut binds = events.iter().filter(|event| {
        bound_source(event).is_some_and(|bound| bound == &source || event.seq > after)
    });
    let root = binds.next()?;
    (root.seq > after
        && bound_source(root) == Some(&source)
        && !matches!(
            root.record,
            BindingRecord::Bound {
                cause: BindingCause::SameSessionVerified,
                ..
            }
        )
        && binds.all(|event| confirms(root, event, &source)))
    .then_some((source, root.seq))
}

fn confirms(root: &BindingEvent, event: &BindingEvent, source: &SourceId) -> bool {
    !root.execution_nonce.is_empty()
        && event.execution_nonce == root.execution_nonce
        && event.tmux_session == root.tmux_session
        && event.channel_id == root.channel_id
        && event.provider == root.provider
        && matches!(&event.record, BindingRecord::Bound {
            old: Some(old), new: BindingTarget::Source(new), cause: BindingCause::SameSessionVerified,
            parent_hint: None, evidence,
        } if old == source && new == source && !evidence.reclaims)
}

/// A transcript proved delivered through `len`, by its current pin or frozen receipt scope,
/// with the Legacy sends read before that look.
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
    let sample = checked_sample(sample, old, events)?;
    let sends = {
        let _adoption = candidate.lock();
        candidate.sends()
    };
    sampled_floor(sample, old.1, sends)
}

/// The publication caller already holds the adoption lock.
pub(in crate::services::tui_o::writer) fn old_source_done_with_sends(
    sample: Option<&Sample>,
    old: &(SourceId, u64),
    events: &[BindingEvent],
    sends: (u64, u64),
) -> Result<Floor, String> {
    sampled_floor(checked_sample(sample, old, events)?, old.1, sends)
}
fn checked_sample<'a>(
    sample: Option<&'a Sample>,
    old: &(SourceId, u64),
    events: &[BindingEvent],
) -> Result<&'a Sample, String> {
    let sample = sample.ok_or("no delivered sample of the old transcript")?;
    if sample.source != old.0 || nonce_of(events, &old.0).as_ref() != Some(&sample.nonce) {
        return Err("the delivered sample is of another transcript or execution".into());
    }
    prefix::metadata_since(&old.0, sample.len)?;
    Ok(sample)
}
fn sampled_floor(sample: &Sample, seq: u64, sends: (u64, u64)) -> Result<Floor, String> {
    if sends.0 != sample.sends.0 {
        return Err("a Legacy body send started since the delivered sample".into());
    }
    Ok(Floor {
        seq,
        sends: sample.sends,
    })
}
