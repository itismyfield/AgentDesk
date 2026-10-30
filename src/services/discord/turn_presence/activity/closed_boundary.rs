//! The native end a `closed` judgment stands on, for the carried-turn close.

use std::time::Instant;

use super::{Activity, Observed, Outcome, Watch, observed};

/// A `closed` judgment's native end: the source and position read, when that position last
/// grew, and whether the close was an interrupt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ClosedBoundary {
    pub source: std::path::PathBuf,
    pub through: u64,
    pub grew_at: Instant,
    pub aborted: bool,
}

/// The end a `closed` answer stands on, read from the facts that decided it.
pub(super) fn closed_boundary(guard: &Watch, answer: Observed) -> Option<ClosedBoundary> {
    if answer != observed(Activity::Idle, "closed") {
        return None;
    }
    let facts = match &guard.outcome {
        Outcome::Source { facts, .. } => facts.as_ref(),
        Outcome::Tail(reader) => &reader.facts,
        _ => return None,
    };
    if facts.pending_receipt()
        && !crate::services::tui_o::turn_mode::carried_close_mutant("receipt")
    {
        return None;
    }
    Some(ClosedBoundary {
        source: facts.binding().source.path.clone(),
        through: guard.through,
        grew_at: guard.grew_at,
        aborted: facts.last_close_aborted()?,
    })
}
