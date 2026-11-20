//! Recovery only backs up the enqueue path's own worker start: brisk while a queue holds work,
//! else once a second, with queue garbage collected at most every ten seconds.

use std::time::{Duration, Instant};

const BUSY: Duration = Duration::from_millis(100);
const IDLE: Duration = Duration::from_secs(1);
const COLLECT_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct Pace {
    collected_at: Option<Instant>,
}

impl Pace {
    /// Whether the scan starting at `now` also collects garbage; the first scan always does.
    pub(super) fn collect(&mut self, now: Instant) -> bool {
        let due = self
            .collected_at
            .is_none_or(|at| now.saturating_duration_since(at) >= COLLECT_EVERY);
        if due {
            self.collected_at = Some(now);
        }
        due
    }

    /// The wait before the next scan, from the active queues the last one found; a failed
    /// scan retries at the busy pace.
    pub(super) fn wait(active: Option<usize>) -> Duration {
        match active {
            Some(0) => IDLE,
            _ => BUSY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_or_a_failed_scan_keeps_the_brisk_pace_and_idleness_slows_it() {
        assert_eq!(Pace::wait(Some(1)), Duration::from_millis(100));
        assert_eq!(Pace::wait(Some(7)), Duration::from_millis(100));
        assert_eq!(Pace::wait(None), Duration::from_millis(100));
        assert_eq!(Pace::wait(Some(0)), Duration::from_secs(1));
    }

    #[test]
    fn garbage_is_collected_on_the_first_scan_then_every_ten_seconds() {
        let start = Instant::now();
        let mut pace = Pace::default();
        assert!(pace.collect(start), "the first scan adopts and collects");
        assert!(!pace.collect(start + Duration::from_secs(9)));
        assert!(pace.collect(start + Duration::from_secs(10)));
        assert!(!pace.collect(start + Duration::from_secs(19)));
        assert!(pace.collect(start + Duration::from_secs(20)));
    }
}
