//! How long a queued request the receiver keeps answering 425 waits before it is sent again.
//! The delay belongs to the queue head; the next head starts from the first delay.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The wait after a request's first 425, as long as the worker's fixed retry interval was.
const FIRST_DELAY: Duration = Duration::from_millis(25);
/// The longest wait; well inside a Herdr launch's 30-second wait for its SessionStart.
const MAX_DELAY: Duration = Duration::from_secs(2);

/// The queue head the receiver refused with 425, how often in a row, and its next send time.
#[derive(Debug, Default)]
pub(super) struct Backoff {
    head: Option<PathBuf>,
    refusals: u32,
    next_send: Option<Instant>,
}

impl Backoff {
    /// Records another 425 of `head` at `now`; a head other than the last one starts over.
    pub(super) fn refused(&mut self, head: &Path, now: Instant) {
        let carried = mutation("carry-to-next-request");
        if self.head.as_deref() != Some(head) && !carried {
            self.refusals = 0;
        }
        self.head = Some(head.to_path_buf());
        self.refusals = self.refusals.saturating_add(1);
        self.next_send = Some(now + delay(self.refusals));
    }

    /// Whether `head` must still wait at `now` before it is sent again.
    pub(super) fn holds(&self, head: &Path, now: Instant) -> bool {
        self.head.as_deref() == Some(head) && self.next_send.is_some_and(|at| now < at)
    }
}

/// 25ms after the first 425, doubling with each one after it up to 2s.
pub(super) fn delay(refusals: u32) -> Duration {
    if mutation("fixed-25ms") {
        return FIRST_DELAY;
    }
    let factor = 1u32
        .checked_shl(refusals.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let delay = FIRST_DELAY.saturating_mul(factor);
    if mutation("no-cap") {
        return delay;
    }
    delay.min(MAX_DELAY)
}

#[cfg(test)]
fn mutation(name: &str) -> bool {
    std::env::var("AGENTDESK_HOOK_RELAY_TEST_MUTATION").is_ok_and(|value| value == name)
}

#[cfg(not(test))]
fn mutation(_: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_head_waits_twice_as_long_each_time_up_to_two_seconds() {
        let millis = |n| delay(n).as_millis();
        let delays: Vec<_> = (1..=9).map(millis).collect();
        assert_eq!(delays, [25, 50, 100, 200, 400, 800, 1600, 2000, 2000]);
        assert_eq!(millis(u32::MAX), 2000);
    }
}
