use std::time::Instant;

use chrono::{DateTime, Utc};

pub(super) trait CatchUpClock: Send + Sync {
    fn sample(&self) -> (DateTime<Utc>, Instant);
}

pub(super) struct SystemClock;

pub(super) static SYSTEM_CLOCK: SystemClock = SystemClock;

impl CatchUpClock for SystemClock {
    fn sample(&self) -> (DateTime<Utc>, Instant) {
        (Utc::now(), Instant::now())
    }
}
