//! Logs a continuing state once: a WARN when it starts or changes kind, an INFO when it ends,
//! because every snapshot consumer of every tick reads the same state again.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Transition {
    /// The key had no state and now has one.
    Started,
    /// The key's state changed from the given one.
    Changed(&'static str),
    /// Same state as last read.
    Continuing,
    /// The given state ended.
    Ended(&'static str),
    /// No state before or now.
    Quiet,
}

impl Transition {
    /// Whether this read is the one that logs at WARN.
    pub(in crate::services::discord) fn warns(self) -> bool {
        matches!(self, Self::Started | Self::Changed(_))
    }
}

struct Entry {
    state: &'static str,
    /// What health names while the state lasts; `None` keeps the state out of health.
    reason: Option<String>,
    seen: Instant,
}

/// One state per key; reading and replacing it is one step under the lock, so concurrent
/// readers of the same state see exactly one `Started`.
#[derive(Default)]
pub(in crate::services::discord) struct TransitionLog(Mutex<HashMap<String, Entry>>);

impl TransitionLog {
    pub(in crate::services::discord) fn record(
        &self,
        key: &str,
        state: Option<(&'static str, Option<String>)>,
        now: Instant,
    ) -> Transition {
        let mut states = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((state, reason)) = state else {
            return match states.remove(key) {
                Some(previous) => Transition::Ended(previous.state),
                None => Transition::Quiet,
            };
        };
        let entry = Entry {
            state,
            reason,
            seen: now,
        };
        match states.insert(key.to_owned(), entry) {
            None => Transition::Started,
            Some(previous) if previous.state == state => Transition::Continuing,
            Some(previous) => Transition::Changed(previous.state),
        }
    }

    /// Reasons of states read within `fresh`; older entries are dropped, since a key nobody
    /// reads any more (its channel left) must not hold health down.
    pub(in crate::services::discord) fn reasons(
        &self,
        fresh: Duration,
        now: Instant,
    ) -> Vec<String> {
        let mut states = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        states.retain(|_, entry| now.saturating_duration_since(entry.seen) <= fresh);
        let mut reasons: Vec<String> = states.values().filter_map(|e| e.reason.clone()).collect();
        reasons.sort();
        reasons
    }
}
