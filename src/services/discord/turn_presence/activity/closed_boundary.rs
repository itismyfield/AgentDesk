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

/// Why a judgment carries no native end, for the close's observation log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct NoBoundary {
    pub reason: &'static str,
    /// Received inputs whose run has not started, when that is the reason.
    pub owed_inputs: usize,
}

impl NoBoundary {
    pub(super) const fn because(reason: &'static str) -> Self {
        Self {
            reason,
            owed_inputs: 0,
        }
    }
}

/// The end a `closed` answer stands on, read from the facts that decided it.
pub(super) fn closed_boundary(
    guard: &Watch,
    answer: Observed,
) -> Result<ClosedBoundary, NoBoundary> {
    if answer != observed(Activity::Idle, "closed") {
        return Err(NoBoundary::because(answer.reason));
    }
    let facts = match &guard.outcome {
        Outcome::Source { facts, .. } => facts.as_ref(),
        Outcome::Tail(reader) => &reader.facts,
        _ => return Err(NoBoundary::because("no_facts")),
    };
    if facts.pending_receipt()
        && !crate::services::tui_o::turn_mode::carried_close_mutant("receipt")
    {
        let owed_inputs = facts.owed_inputs();
        let reason = "pending_receipt";
        return Err(NoBoundary {
            reason,
            owed_inputs,
        });
    }
    let aborted = (facts.last_close_aborted()).ok_or(NoBoundary::because("opener_unread"))?;
    Ok(ClosedBoundary {
        source: facts.binding().source.path.clone(),
        through: guard.through,
        grew_at: guard.grew_at,
        aborted,
    })
}
