//! When the host starts a halted channel writer again in this process, and how long it waits first.

use std::time::Duration;

use super::WriterAlarm;
use super::deliver::StopCause;
use crate::services::tui_o::store::transient_io;

pub const FIRST_WAIT: Duration = Duration::from_secs(30);
pub const MAX_WAIT: Duration = Duration::from_secs(300);
/// A resumed writer that ran this long before halting again starts the waits over.
pub const STABLE_RUN: Duration = MAX_WAIT;

/// Only a halt from a store write that failed for lack of space or a passing condition resumes.
pub fn resumable(cause: &StopCause) -> bool {
    matches!(cause.alarm, WriterAlarm::Halted { .. }) && cause.io.is_some_and(transient_io)
}

/// The wait before each attempt, doubling up to `MAX_WAIT`.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: FIRST_WAIT }
    }
}

impl Backoff {
    pub fn next_wait(&mut self) -> Duration {
        let wait = self.next;
        self.next = (wait * 2).min(MAX_WAIT);
        wait
    }
}
