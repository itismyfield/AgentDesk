use super::{RestartAttempt, RestartFailure, RestartReportState};
thread_local! {
    static DECISIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
fn decision_count() -> u64 {
    DECISIONS.with(std::cell::Cell::get)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Association<E> {
    RowEpisode(RowBinding<E>),
    RestartOnly,
    LegacyUnassociated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RowContext {
    pub nonce: Option<String>,
    pub actor_cutoff: Option<(u64, u64)>,
    pub turn_id: u64,
    pub turn_start_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RowBinding<E> {
    full_pin: E,
    row_revision: u64,
    context: RowContext,
}

impl<E> RowBinding<E> {
    pub(super) fn capture(full_pin: E, row_revision: u64, context: RowContext) -> Option<Self> {
        if context
            .nonce
            .as_ref()
            .is_none_or(|nonce| nonce.trim().is_empty())
            || context
                .actor_cutoff
                .is_none_or(|(actor, cutoff)| actor == 0 || cutoff == 0)
            || (context.turn_id == 0 && context.turn_start_offset.is_none())
        {
            return None;
        }
        Some(Self {
            full_pin,
            row_revision,
            context,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReportPin<E> {
    pub provider: String,
    pub channel_id: u64,
    pub attempt: RestartAttempt,
    pub association: Association<E>,
    pub state: RestartReportState,
}

// E is the existing full episode pin. Evidence verification belongs to the future entry.
pub(super) enum Evidence<E> {
    RowCompleted(RowBinding<E>),
    ExplicitRestartSettle {
        attempt: RestartAttempt,
        state: RestartReportState,
    },
    RestartDelivery {
        attempt: RestartAttempt,
        accepted: Result<(), ()>,
    },
    Unverified,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Preserve,
    ConsumeRowReport,
    SettleRestart(RestartReportState),
    ConsumeRestartReport,
}

pub(super) fn decide<E: PartialEq>(
    expected: &ReportPin<E>,
    current: &ReportPin<E>,
    evidence: Evidence<E>,
) -> Decision {
    DECISIONS.with(|count| count.set(count.get().saturating_add(1)));
    if expected != current {
        return Decision::Preserve;
    }
    match (&current.association, evidence) {
        (Association::RowEpisode(binding), Evidence::RowCompleted(proof)) if *binding == proof => {
            Decision::ConsumeRowReport
        }
        (Association::RestartOnly, Evidence::ExplicitRestartSettle { attempt, state })
            if attempt == current.attempt
                && current.state == RestartReportState::Awaiting
                && proven_terminal(&state) =>
        {
            Decision::SettleRestart(state)
        }
        (Association::RestartOnly, Evidence::RestartDelivery { attempt, accepted })
            if attempt == current.attempt
                && accepted.is_ok()
                && proven_terminal(&current.state) =>
        {
            Decision::ConsumeRestartReport
        }
        _ => Decision::Preserve,
    }
}

fn proven_terminal(state: &RestartReportState) -> bool {
    matches!(
        state,
        RestartReportState::Succeeded
            | RestartReportState::Failed(RestartFailure::Cancelled | RestartFailure::RolledBack)
    )
}

#[cfg(test)]
mod entry_tests;
#[cfg(test)]
mod tests;
