//! Retained replay drops only durable acknowledgements after all derivation has finished.

use super::pieces::Derived;
use crate::services::tui_o::store::ledger::LedgerState;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplaySummary {
    pub before: usize,
    pub posted: usize,
    pub excluded: usize,
    pub kept: usize,
    pub deferred: bool,
}

pub(super) fn filter(
    ledger: &LedgerState,
    retain: impl FnOnce(&mut dyn FnMut(&Derived) -> bool),
) -> ReplaySummary {
    #[cfg(test)]
    let mut kept = Vec::new();
    #[cfg(test)]
    let (filter_off, generic_settled) = (mutant("filter_off"), mutant("generic_settled"));
    let mut summary = ReplaySummary {
        deferred: ledger.unresolved().is_some() || ledger.violation().is_some(),
        ..ReplaySummary::default()
    };
    retain(&mut |derived| {
        summary.before += 1;
        let acked = !summary.deferred && super::owed::acked(ledger, derived);
        #[cfg(test)]
        let acked = {
            let mut acked = acked;
            if filter_off {
                acked = false;
            }
            if !summary.deferred
                && generic_settled
                && let Derived::Piece(piece) = derived
            {
                acked = matches!(
                    ledger.disposition(&piece.unit_key, piece.index),
                    crate::services::tui_o::store::ledger::PieceDisposition::Settled
                );
            }
            acked
        };
        if !acked {
            summary.kept += 1;
            #[cfg(test)]
            SNAPSHOTS.with_borrow(|snapshots| {
                if snapshots.is_some() {
                    kept.push(derived.clone());
                }
            });
            return true;
        }
        let excluded = match derived {
            Derived::Piece(piece) => ledger.excluded(&piece.unit_key).is_some(),
            Derived::Excluded { .. } => true,
            Derived::Blocked { .. } => false,
        };
        if excluded {
            summary.excluded += 1;
        } else {
            summary.posted += 1;
        }
        false
    });
    #[cfg(test)]
    SNAPSHOTS.with_borrow_mut(|snapshots| {
        if let Some(snapshots) = snapshots {
            snapshots.push((summary, kept));
        }
    });
    summary
}

pub(super) fn after_open(pending: &mut bool, ledger: &LedgerState) -> bool {
    if !*pending || ledger.unresolved().is_some() || ledger.violation().is_some() {
        return false;
    }
    *pending = false;
    #[cfg(test)]
    if mutant("refilter_off") {
        return false;
    }
    true
}

pub(super) struct Budget {
    steps: usize,
    started: std::time::Instant,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self {
            steps: 0,
            started: std::time::Instant::now(),
        }
    }

    pub(super) async fn yield_if_due(&mut self) {
        self.steps += 1;
        #[cfg(test)]
        if mutant("yield_off") {
            return;
        }
        if self.steps >= 64 || self.started.elapsed() >= std::time::Duration::from_millis(10) {
            tokio::task::yield_now().await;
            self.steps = 0;
            self.started = std::time::Instant::now();
        }
    }
}

/// AGENTDESK_REPLAY_MUTANT selects faults only in dedicated mutation test builds.
#[cfg(test)]
pub(crate) fn mutant(name: &str) -> bool {
    option_env!("AGENTDESK_REPLAY_MUTATION_BUILD") == Some("1")
        && std::env::var("AGENTDESK_REPLAY_MUTANT").is_ok_and(|value| value == name)
}

#[cfg(test)]
type Snapshot = (ReplaySummary, Vec<Derived>);

#[cfg(test)]
thread_local! {
    static SNAPSHOTS: std::cell::RefCell<Option<Vec<Snapshot>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn capture() {
    SNAPSHOTS.with_borrow_mut(|snapshots| *snapshots = Some(Vec::new()));
}

#[cfg(test)]
pub(super) fn snapshots() -> Vec<Snapshot> {
    SNAPSHOTS.with_borrow(|snapshots| snapshots.clone().unwrap_or_default())
}
