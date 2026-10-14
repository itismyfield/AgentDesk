//! Judgment of an operator's transition cleanup: which unproven old rotation hops could be retired
//! with an audit record. It reads only the snapshot it is given, never files, time or store.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::rotation::RETIRE_QUIET;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::cleanup_witness::CleanupWitness;
use crate::services::tui_o::store::rotation::{Rotation, Successor};
use crate::services::tui_o::store::spool::{
    Cursor, ExecutionEnd, TransitionCleanup, VerifiedEnd, source_key,
};

#[path = "transition_observe.rs"]
pub(crate) mod observe;
#[path = "transition_scope.rs"]
pub(crate) mod scope;
use observe::{FileSeen, Observation, Present};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Home {
    Settled,
    /// Draining or moving to another node.
    Moving,
    Unread(String),
}

/// What the actor's reader holds for one source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReaderView {
    /// `(captured_through, read_through)` of the open FD; `None` while dormant or closed.
    pub capture: Option<(u64, u64)>,
    pub pending_batch: bool,
    /// The reader's drain target as held, before it is compared with the cursor.
    pub drain_to: Option<u64>,
    pub captured_any: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceFacts {
    pub reader: Option<ReaderView>,
    /// The GC ledger logged a collected segment, so the source has captured a record.
    pub gc_logged: bool,
    pub witness: Option<CleanupWitness>,
    /// Two probes of the file; the later one at least `RETIRE_QUIET` after the earlier.
    pub earlier: Option<Observation>,
    pub later: Option<Observation>,
}

/// Output the channel still owes. The general queue names no source, so any of it holds every hop.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Obligations {
    pub owed: usize,
    pub prepared: usize,
    pub unsealed: usize,
    pub posting: usize,
    /// Codex verified delivery work, and the spool keys it protects.
    pub verified_work: usize,
    pub protected: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub channel: u64,
    pub provider: ShadowProvider,
    pub home: Home,
    pub rotation: Rotation,
    /// The actor's cursors, including init sources that never wrote a cursor file.
    pub cursors: Vec<Cursor>,
    pub pinned: Result<Option<SourceId>, String>,
    /// Sources the current execution reads.
    pub execution: Result<Vec<SourceId>, String>,
    /// By spool key.
    pub sources: BTreeMap<String, SourceFacts>,
    /// Validated `(hop seq, pane)` of hops stored without a seq, by spool key.
    pub legacy_hops: Result<BTreeMap<String, Vec<(u64, String)>>, String>,
    pub obligations: Result<Obligations, String>,
}

/// How the request vouches that the execution writing a source has ended.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) enum Termination {
    /// The operator checked the host and pane and states the basis.
    Confirmed(ExecutionEnd),
    /// Inferred without checking the execution, such as from a configuration switch.
    Implied(String),
    Unread(String),
}

/// The operator's request as its route reads it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Request {
    pub channel: u64,
    /// The last hop seq the transition covers.
    pub cutoff: u64,
    pub job_id: String,
    pub operator: String,
    pub at: DateTime<Utc>,
    pub reason: String,
    pub termination: Option<Termination>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Owed {
    Queue,
    Prepared,
    Unsealed,
    Posting,
    VerifiedWork,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Refusal {
    OtherChannel,
    IncompleteRequest,
    CodexDeferred,
    HomeMoving,
    Proven,
    AfterCutoff,
    CurrentSource,
    PendingBatch,
    DrainOwed,
    CaptureAhead,
    SuccessorUncaptured,
    OutputOwed(Owed),
    Replaced,
    Shrunk,
    PrefixMismatch,
    Growing,
    TornTail,
    NotQuiet,
    NoWitness,
    WitnessUnbound,
    WitnessNotEligible,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Unclear {
    HomeUnread(String),
    CursorMissing,
    HistoryUnread(String),
    NoHistory,
    AmbiguousHistory,
    PinnedUnread(String),
    NoPinnedSource,
    ExecutionUnread(String),
    NoReaderState,
    NoTermination,
    ImpliedTermination,
    TerminationUnread(String),
    TerminationIncomplete,
    TerminationOutOfRange,
    TerminationOtherPane,
    ObligationsUnread(String),
    NotObserved,
    StaleObservation,
    FileUnreadable(String),
    MissingWithOpenFd,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Verdict {
    /// Every condition holds; the draft is what the retirement would record.
    Eligible(Box<TransitionCleanup>),
    /// This job retired it, and the cursor and file still match what it verified.
    AlreadyDone,
    Refused(Refusal),
    Unknown(Unclear),
    /// Retired without a transition record, by the provider-proof or missing-source path.
    NotTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Judged {
    pub key: String,
    pub hop_seq: Option<u64>,
    pub verdict: Verdict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Summary {
    /// Old sources whose cursor is not retired, counted as `rotation_unsettled` counts them.
    pub unsettled: usize,
    pub eligible: usize,
    pub already_done: usize,
    pub refused: usize,
    pub unknown: usize,
    pub not_target: usize,
    /// Owed output, apart from the rotation count; `None` when it could not be read.
    pub obligations: Option<Obligations>,
    /// Hops a historical provider proof would settle; this judgment does not compute it.
    pub proof_recovery: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Judgment {
    pub hops: Vec<Judged>,
    pub summary: Summary,
}

pub(crate) fn judge(snapshot: &Snapshot, request: &Request) -> Judgment {
    let hops: Vec<Judged> = (snapshot.rotation.successors.iter())
        .map(|(key, next)| {
            let hop = hop(snapshot, key, next);
            let hop_seq = hop.as_ref().ok().map(|(seq, _)| *seq);
            let verdict = check(snapshot, request, key, next, hop).unwrap_or_else(|v| v);
            Judged {
                key: key.clone(),
                hop_seq,
                verdict,
            }
        })
        .collect();
    let count = |pick: fn(&Verdict) -> bool| hops.iter().filter(|h| pick(&h.verdict)).count();
    let retired: BTreeSet<String> = (snapshot.cursors.iter())
        .filter(|cursor| cursor.retired)
        .map(|cursor| source_key(&cursor.source))
        .collect();
    let summary = Summary {
        unsettled: (snapshot.rotation.successors.keys())
            .filter(|key| !retired.contains(*key))
            .count(),
        eligible: count(|v| matches!(v, Verdict::Eligible(_))),
        already_done: count(|v| matches!(v, Verdict::AlreadyDone)),
        refused: count(|v| matches!(v, Verdict::Refused(_))),
        unknown: count(|v| matches!(v, Verdict::Unknown(_))),
        not_target: count(|v| matches!(v, Verdict::NotTarget)),
        obligations: snapshot.obligations.as_ref().ok().cloned(),
        proof_recovery: None,
    };
    Judgment { hops, summary }
}

/// The hop's seq and pane; a hop stored without them takes both from one validated history match.
/// The approved scope places each hop with this same resolution.
pub(super) fn hop(
    snapshot: &Snapshot,
    key: &str,
    next: &Successor,
) -> Result<(u64, Option<String>), Unclear> {
    if let Some(seq) = next.seq {
        return Ok((seq, next.tmux_session.clone()));
    }
    let history = (snapshot.legacy_hops.as_ref()).map_err(|e| Unclear::HistoryUnread(e.clone()))?;
    // Every candidate counts, a repeated one too: only a single history match fixes the hop.
    match history.get(key).map_or(&[][..], Vec::as_slice) {
        [] => Err(Unclear::NoHistory),
        [(seq, pane)] => Ok((*seq, Some(pane.clone()))),
        _ => Err(Unclear::AmbiguousHistory),
    }
}

fn check(
    snapshot: &Snapshot,
    request: &Request,
    key: &str,
    next: &Successor,
    hop: Result<(u64, Option<String>), Unclear>,
) -> Result<Verdict, Verdict> {
    use Refusal::*;
    let refuse = Verdict::Refused;
    if request.channel != snapshot.channel {
        return Err(refuse(OtherChannel));
    }
    if [&request.job_id, &request.operator, &request.reason]
        .iter()
        .any(|field| field.trim().is_empty())
    {
        return Err(refuse(IncompleteRequest));
    }
    let cursor = (snapshot.cursors.iter())
        .find(|cursor| source_key(&cursor.source) == key)
        .ok_or(Verdict::Unknown(Unclear::CursorMissing))?;
    let facts = snapshot.sources.get(key);
    if cursor.retired {
        match &cursor.transition_cleanup {
            None => return Err(Verdict::NotTarget),
            // A past record settles only its own job, and only while nothing it checked changed.
            Some(done)
                if done.job_id == request.job_id
                    && done.cutoff == request.cutoff
                    && done.verified.offset == cursor.captured_through
                    && done.verified.prefix_hash == cursor.prefix_hash
                    && end_seen(cursor, facts) == Some(&done.verified.fingerprint) =>
            {
                return Err(Verdict::AlreadyDone);
            }
            Some(_) => {}
        }
    }
    if snapshot.provider == ShadowProvider::Codex {
        return Err(refuse(CodexDeferred));
    }
    match &snapshot.home {
        Home::Settled => {}
        Home::Moving => return Err(refuse(HomeMoving)),
        Home::Unread(detail) => return Err(unclear(Unclear::HomeUnread(detail.clone()))),
    }
    if next.proof.is_some() {
        return Err(refuse(Proven));
    }
    let (hop_seq, pane) = hop.map_err(unclear)?;
    if hop_seq > request.cutoff {
        return Err(refuse(AfterCutoff));
    }
    not_current(snapshot, cursor)?;
    let facts = facts.ok_or(Verdict::Unknown(Unclear::NoReaderState))?;
    let reader = (facts.reader.as_ref()).ok_or(Verdict::Unknown(Unclear::NoReaderState))?;
    drained(reader, next, cursor)?;
    let successor = snapshot.sources.get(&source_key(&next.source));
    let captured =
        |f: &SourceFacts| f.gc_logged || f.reader.as_ref().is_some_and(|r| r.captured_any);
    if !successor.is_some_and(captured) {
        return Err(refuse(SuccessorUncaptured));
    }
    let ended = ended(request, hop_seq, pane.as_deref()).map_err(unclear)?;
    nothing_owed(snapshot, key)?;
    let fingerprint = settled_file(cursor, facts, reader)?;
    Ok(Verdict::Eligible(Box::new(TransitionCleanup {
        job_id: request.job_id.clone(),
        operator: request.operator.clone(),
        at: request.at,
        reason: request.reason.clone(),
        cutoff: request.cutoff,
        hop_seq,
        verified: VerifiedEnd {
            offset: cursor.captured_through,
            prefix_hash: cursor.prefix_hash.clone(),
            fingerprint,
        },
        ended,
    })))
}

fn unclear(reason: Unclear) -> Verdict {
    Verdict::Unknown(reason)
}

/// The file end seen now for the cursor: the probed file, or the witness of a missing one.
fn end_seen<'a>(cursor: &Cursor, facts: Option<&'a SourceFacts>) -> Option<&'a Fingerprint> {
    let facts = facts?;
    match &facts.later.as_ref().filter(|seen| seen.is_of(cursor))?.file {
        FileSeen::Present(present) if present.same_file && present.prefix_matches => {
            Some(&present.fingerprint)
        }
        FileSeen::Missing => (facts.witness.as_ref())
            .filter(|w| same_end(w, cursor))
            .map(|w| &w.fingerprint),
        _ => None,
    }
}

/// The witness checked this cursor's source, offset and hash. Unlike `bound_to` it asks nothing
/// of the cursor's lifecycle, so a retired cursor's own record still compares.
fn same_end(witness: &CleanupWitness, cursor: &Cursor) -> bool {
    witness.source == cursor.source
        && witness.captured_through == cursor.captured_through
        && witness.prefix_hash == cursor.prefix_hash
}

fn not_current(snapshot: &Snapshot, cursor: &Cursor) -> Result<(), Verdict> {
    let pinned =
        (snapshot.pinned.as_ref()).map_err(|e| unclear(Unclear::PinnedUnread(e.clone())))?;
    let pinned = pinned
        .as_ref()
        .ok_or(Verdict::Unknown(Unclear::NoPinnedSource))?;
    let execution =
        (snapshot.execution.as_ref()).map_err(|e| unclear(Unclear::ExecutionUnread(e.clone())))?;
    // Any shared name or inode counts as current: a wrong refusal is safe, a wrong cleanup is not.
    let same = |current: &SourceId| {
        let old = &cursor.source;
        current.path == old.path || current.session_id == old.session_id || current.ino == old.ino
    };
    if std::iter::once(pinned).chain(execution).any(same) {
        return Err(Verdict::Refused(Refusal::CurrentSource));
    }
    Ok(())
}

/// A drain target binds only while the cursor is short of it, as the reader restores it.
fn drained(reader: &ReaderView, next: &Successor, cursor: &Cursor) -> Result<(), Verdict> {
    let through = cursor.captured_through;
    let owes = |end: Option<u64>| end.is_some_and(|end| through < end);
    if reader.pending_batch {
        return Err(Verdict::Refused(Refusal::PendingBatch));
    }
    if owes(reader.drain_to) || owes(next.drain_to) {
        return Err(Verdict::Refused(Refusal::DrainOwed));
    }
    if reader
        .capture
        .is_some_and(|read| read != (through, through))
    {
        return Err(Verdict::Refused(Refusal::CaptureAhead));
    }
    Ok(())
}

fn ended(request: &Request, hop_seq: u64, pane: Option<&str>) -> Result<ExecutionEnd, Unclear> {
    let end = match &request.termination {
        None => return Err(Unclear::NoTermination),
        Some(Termination::Implied(_)) => return Err(Unclear::ImpliedTermination),
        Some(Termination::Unread(detail)) => {
            return Err(Unclear::TerminationUnread(detail.clone()));
        }
        Some(Termination::Confirmed(end)) => end,
    };
    if [&end.host, &end.pane, &end.basis]
        .iter()
        .any(|field| field.trim().is_empty())
    {
        return Err(Unclear::TerminationIncomplete);
    }
    if !(end.first_seq..=end.last_seq).contains(&hop_seq) {
        return Err(Unclear::TerminationOutOfRange);
    }
    if pane != Some(end.pane.as_str()) {
        return Err(Unclear::TerminationOtherPane);
    }
    Ok(end.clone())
}

fn nothing_owed(snapshot: &Snapshot, key: &str) -> Result<(), Verdict> {
    let owed = (snapshot.obligations.as_ref())
        .map_err(|e| unclear(Unclear::ObligationsUnread(e.clone())))?;
    let held = [
        (owed.owed, Owed::Queue),
        (owed.prepared, Owed::Prepared),
        (owed.unsealed, Owed::Unsealed),
        (owed.posting, Owed::Posting),
        (owed.verified_work, Owed::VerifiedWork),
    ];
    if let Some((_, what)) = held.into_iter().find(|(count, _)| *count > 0) {
        return Err(Verdict::Refused(Refusal::OutputOwed(what)));
    }
    if owed.protected.contains(key) {
        return Err(Verdict::Refused(Refusal::OutputOwed(Owed::VerifiedWork)));
    }
    Ok(())
}

/// The file stayed at the cursor's verified end across both probes, or is gone with a fresh
/// witness bound to the cursor; returns the fingerprint the retirement would record.
fn settled_file(
    cursor: &Cursor,
    facts: &SourceFacts,
    reader: &ReaderView,
) -> Result<Fingerprint, Verdict> {
    use Refusal::*;
    let (Some(earlier), Some(later)) = (&facts.earlier, &facts.later) else {
        return Err(unclear(Unclear::NotObserved));
    };
    if !earlier.is_of(cursor) || !later.is_of(cursor) {
        return Err(unclear(Unclear::StaleObservation));
    }
    let quiet = (later.at.duration_since(earlier.at)).is_ok_and(|gap| gap >= RETIRE_QUIET);
    if !quiet {
        return Err(Verdict::Refused(NotQuiet));
    }
    match (&earlier.file, &later.file) {
        (FileSeen::Present(first), FileSeen::Present(last)) => {
            at_end(cursor, first)?;
            at_end(cursor, last)?;
            if first.fingerprint != last.fingerprint {
                return Err(Verdict::Refused(NotQuiet));
            }
            Ok(last.fingerprint.clone())
        }
        (FileSeen::Missing, FileSeen::Missing) => {
            if reader.capture.is_some() {
                return Err(unclear(Unclear::MissingWithOpenFd));
            }
            let witness = facts.witness.as_ref().ok_or(Verdict::Refused(NoWitness))?;
            if !same_end(witness, cursor) {
                return Err(Verdict::Refused(WitnessUnbound));
            }
            if !witness.eligible(later.at) {
                return Err(Verdict::Refused(WitnessNotEligible));
            }
            Ok(witness.fingerprint.clone())
        }
        (FileSeen::Unreadable(detail), _) | (_, FileSeen::Unreadable(detail)) => {
            Err(unclear(Unclear::FileUnreadable(detail.clone())))
        }
        (FileSeen::Replaced, _) | (_, FileSeen::Replaced) => Err(Verdict::Refused(Replaced)),
        // The file appeared or vanished between the probes.
        _ => Err(Verdict::Refused(NotQuiet)),
    }
}

fn at_end(cursor: &Cursor, present: &Present) -> Result<(), Verdict> {
    use Refusal::*;
    let len = present.fingerprint.len;
    let refusal = if !present.same_file {
        Replaced
    } else if len < cursor.captured_through {
        Shrunk
    } else if !present.prefix_matches {
        PrefixMismatch
    } else if len > cursor.captured_through {
        Growing
    } else if !present.whole_tail {
        TornTail
    } else if !present.steady {
        NotQuiet
    } else {
        return Ok(());
    };
    Err(Verdict::Refused(refusal))
}

#[cfg(test)]
#[path = "transition_cleanup_tests.rs"]
mod tests;
