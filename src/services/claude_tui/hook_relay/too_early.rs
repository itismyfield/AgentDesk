//! How long a queued request the receiver keeps answering 425 waits before it is sent again.
//! The delay belongs to the queue head; the next head starts from the first delay.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::response_window::{may_publish, respond_by};

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

/// The reply window of each queued request, read once from its file while the request is queued.
#[derive(Debug, Default)]
pub(super) struct CallerWindows {
    windows: HashMap<PathBuf, Option<DateTime<Utc>>>,
}

impl CallerWindows {
    /// Whether a caller still waits at `now` for the reply of any request in `request_paths`.
    pub(super) fn any_open(&mut self, request_paths: &[PathBuf], now: DateTime<Utc>) -> bool {
        if mutation("backoff-ignores-caller") {
            return false;
        }
        if mutation("windows-reread") {
            self.windows.clear();
        }
        let queued: HashSet<&PathBuf> = request_paths.iter().collect();
        self.windows.retain(|path, _| queued.contains(path));
        request_paths.iter().any(|path| {
            let window = match self.windows.get(path) {
                Some(window) => *window,
                // A file that cannot be read yet is read again on the next pass.
                None => match read_window(path) {
                    Some(window) => *self.windows.entry(path.clone()).or_insert(window),
                    None => None,
                },
            };
            may_publish(window, now)
        })
    }
}

/// The reply window a request file names, `Some(None)` for a request nobody waits on; the payload
/// is skipped, not parsed.
fn read_window(path: &Path) -> Option<Option<DateTime<Utc>>> {
    #[derive(Deserialize)]
    struct Response {
        timeout_millis: u64,
    }
    #[derive(Deserialize)]
    struct Window {
        published_at: DateTime<Utc>,
        response: Option<Response>,
    }
    #[cfg(test)]
    tests::note_read(path);
    let encoded = std::fs::read(path).ok()?;
    let window = serde_json::from_slice::<Window>(&encoded).ok()?;
    let response = window.response.map(|response| response.timeout_millis);
    Some(response.map(|timeout| respond_by(window.published_at, timeout)))
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
pub(super) mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::*;

    static READS: LazyLock<Mutex<HashMap<PathBuf, usize>>> = LazyLock::new(Default::default);

    pub(super) fn note_read(path: &Path) {
        let mut reads = READS.lock().unwrap_or_else(|poison| poison.into_inner());
        *reads.entry(path.to_path_buf()).or_default() += 1;
    }

    /// How often each file under `dir` had its reply window read.
    pub(in crate::services::claude_tui::hook_relay) fn window_reads(dir: &Path) -> Vec<usize> {
        let reads = READS.lock().unwrap_or_else(|poison| poison.into_inner());
        let under = reads.iter().filter(|(path, _)| path.starts_with(dir));
        under.map(|(_, count)| *count).collect()
    }

    #[test]
    fn a_refused_head_waits_twice_as_long_each_time_up_to_two_seconds() {
        let millis = |n| delay(n).as_millis();
        let delays: Vec<_> = (1..=9).map(millis).collect();
        assert_eq!(delays, [25, 50, 100, 200, 400, 800, 1600, 2000, 2000]);
        assert_eq!(millis(u32::MAX), 2000);
    }
}
