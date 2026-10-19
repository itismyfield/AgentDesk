//! Reply window of a queued hook: the provider stops waiting at `respond_by`, while the
//! observation keeps the full delivery deadline of its queue entry.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::services::claude_tui::hook_server::relay_receipts::DELIVERY_TTL;

/// Reply budget left at publish, so the queued window ends when the waiting caller gives up.
pub(super) fn budget_millis(respond_until: Instant) -> u64 {
    let left = respond_until.saturating_duration_since(Instant::now());
    left.min(DELIVERY_TTL)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn respond_by(published_at: DateTime<Utc>, timeout_millis: u64) -> DateTime<Utc> {
    let window = i64::try_from(timeout_millis)
        .ok()
        .and_then(chrono::Duration::try_milliseconds);
    window
        .and_then(|window| published_at.checked_add_signed(window))
        .unwrap_or(published_at)
}

/// A reply is written for the caller only while the caller still waits for it.
pub(super) fn may_publish(respond_by: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    respond_by.is_some_and(|respond_by| now < respond_by)
}

/// A waiting caller's deadlines as its own thread saw them, read back by the queue tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DeadlineObservation {
    /// The deadline the caller handed to enqueue.
    pub(super) first: Option<Instant>,
    /// Monotonic time beside the request's `published_at`.
    pub(super) published: Option<Instant>,
    /// The deadline the caller's wait last compared against.
    pub(super) waited: Option<Instant>,
}

#[cfg(test)]
thread_local! {
    pub(super) static DEADLINES: std::cell::Cell<DeadlineObservation> = Default::default();
}

#[cfg(test)]
fn observe(record: impl FnOnce(&mut DeadlineObservation)) {
    DEADLINES.with(|cell| {
        let mut seen = cell.get();
        record(&mut seen);
        cell.set(seen);
    });
}

/// The caller's deadline as handed to enqueue.
pub(super) fn first_deadline(deadline: Instant) -> Instant {
    #[cfg(test)]
    observe(|seen| seen.first = Some(deadline));
    deadline
}

/// A request's publish time.
pub(super) fn published_now() -> DateTime<Utc> {
    #[cfg(test)]
    observe(|seen| seen.published = Some(Instant::now()));
    Utc::now()
}

/// Whether the caller's own wait, `timeout` from `started`, has run out.
pub(super) fn caller_deadline_passed(started: Instant, timeout: Duration) -> bool {
    #[cfg(test)]
    observe(|seen| seen.waited = Some(started + timeout));
    started.elapsed() >= timeout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_window_ends_at_the_callers_deadline_and_never_after() {
        let published = Utc::now();
        let respond_by = respond_by(published, 750);
        assert_eq!(respond_by - published, chrono::Duration::milliseconds(750));
        assert!(may_publish(Some(respond_by), published));
        assert!(!may_publish(Some(respond_by), respond_by));
        assert!(!may_publish(None, published));
        let spent = Instant::now() + std::time::Duration::from_millis(400);
        assert!(budget_millis(spent) <= 400);
        assert_eq!(budget_millis(Instant::now()), 0);
        assert_eq!(super::respond_by(published, u64::MAX), published);
    }
}
