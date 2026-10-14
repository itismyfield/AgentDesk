use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

mod cursor;
use cursor::{Cursors, Visit};
pub(crate) use cursor::{Page, Source};

const CACHE_LIMIT: usize = 256;
const RUNNING_LIMIT: usize = 4;
const SCAN_LIMIT: usize = 16;
const PROBE_LIMIT: usize = 4;
thread_local! {
    static INVOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
pub(crate) fn invocation_count() -> u64 {
    INVOCATIONS.with(std::cell::Cell::get)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    pub source: Source,
    pub namespace: u64,
    pub identity: u128,
}

pub(crate) struct Permit {
    serial: u64,
}

struct Hint {
    key: Key,
    due: Duration,
    failures: u8,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Visibility {
    pub cached: usize,
    pub in_flight: usize,
    pub page: usize,
    pub saturated: bool,
    pub cursors: [u64; 3],
    pub scan_age: [Option<Duration>; 3],
    pub revisit_lag: [Option<Duration>; 3],
}

pub(crate) struct Scheduler {
    hints: VecDeque<Hint>,
    running: Vec<(Key, u64)>,
    next_serial: u64,
    cursors: Cursors,
    round_at: Option<Duration>,
    scanned: usize,
    issued: usize,
}

// One process state, shared across sources and providers; callers cannot construct another.
pub(crate) fn with_scheduler<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    INVOCATIONS.with(|count| count.set(count.get().saturating_add(1)));
    static STATE: OnceLock<Mutex<Scheduler>> = OnceLock::new();
    let mut state = STATE
        .get_or_init(|| Mutex::new(Scheduler::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut state)
}

impl Scheduler {
    fn new() -> Self {
        Self {
            hints: VecDeque::new(),
            running: Vec::new(),
            next_serial: 0,
            cursors: Cursors::new(),
            round_at: None,
            scanned: 0,
            issued: 0,
        }
    }

    pub(crate) fn begin_round(&mut self, now: Duration) {
        if self.round_at.is_none_or(|previous| now > previous) {
            self.round_at = Some(now);
            self.scanned = 0;
            self.issued = 0;
        }
    }

    pub(crate) fn next_source(&mut self) -> Source {
        self.cursors.next_source()
    }

    pub(crate) fn page<'a, T: 'a, I: Iterator<Item = (u64, &'a T)>>(
        &mut self,
        source: Source,
        now: Duration,
        seek: impl FnOnce(u64) -> I,
    ) -> Page<'a, T> {
        self.cursors.page(source, now, seek)
    }

    pub(crate) fn visit<T>(
        &mut self,
        visit: &Visit<'_, T>,
        key: Key,
        now: Duration,
    ) -> Option<Permit> {
        if !self.take_candidate(now) || key.source != visit.source() {
            return None;
        }
        if !self.cursors.claim(visit) {
            return None;
        }
        if !self.hints.iter().any(|hint| hint.key == key) {
            let origin = if visit.previously_seen() {
                visit.epoch_started()
            } else {
                now
            };
            let due = origin.saturating_add(first_delay(key.source));
            self.insert_hint(Hint {
                key,
                due,
                failures: 0,
            });
        }
        self.admit(key, now)
    }

    pub(crate) fn probe_cached(&mut self, key: Key, now: Duration) -> Option<Permit> {
        if self.take_candidate(now) {
            self.admit(key, now)
        } else {
            None
        }
    }

    fn take_candidate(&mut self, now: Duration) -> bool {
        if self.round_at != Some(now) || self.scanned >= SCAN_LIMIT {
            return false;
        }
        self.scanned += 1;
        true
    }

    fn insert_hint(&mut self, hint: Hint) {
        if self.hints.len() >= CACHE_LIMIT {
            let removable = self
                .hints
                .iter()
                .position(|hint| !self.running.iter().any(|(key, _)| *key == hint.key));
            let Some(index) = removable else { return };
            self.hints.remove(index);
        }
        self.hints.push_back(hint);
    }

    fn admit(&mut self, key: Key, now: Duration) -> Option<Permit> {
        if self.issued >= PROBE_LIMIT
            || self.running.len() >= RUNNING_LIMIT
            || self.running.iter().any(|(running, _)| *running == key)
        {
            return None;
        }
        let Some(hint) = self.hints.iter().find(|hint| hint.key == key) else {
            return None;
        };
        if now < hint.due {
            return None;
        }
        let serial = self.next_serial.checked_add(1)?;
        self.next_serial = serial;
        self.running.push((key, serial));
        self.issued += 1;
        Some(Permit { serial })
    }

    pub(crate) fn complete_unmeasured(&mut self, permit: Permit, now: Duration) -> bool {
        let Some(index) = self
            .running
            .iter()
            .position(|(_, serial)| *serial == permit.serial)
        else {
            return false;
        };
        let (key, _) = self.running.swap_remove(index);
        if let Some(hint) = self.hints.iter_mut().find(|hint| hint.key == key) {
            hint.failures = hint.failures.saturating_add(1);
            let seconds = match hint.failures {
                1 => 60,
                2 => 120,
                _ => 300,
            };
            hint.due = now.saturating_add(Duration::from_secs(seconds));
        }
        true
    }

    pub(crate) fn visibility(&self, now: Duration) -> Visibility {
        let (cursors, scan_age, revisit_lag) = self.cursors.visibility(now);
        Visibility {
            cached: self.hints.len(),
            in_flight: self.running.len(),
            page: self.cursors.page_size(),
            saturated: self.hints.len() == CACHE_LIMIT,
            cursors,
            scan_age,
            revisit_lag,
        }
    }
}

fn first_delay(source: Source) -> Duration {
    Duration::from_secs(if source == Source::Inventory { 60 } else { 30 })
}

#[cfg(test)]
mod tests;
