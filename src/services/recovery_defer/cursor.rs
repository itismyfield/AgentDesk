use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

const PAGE_LIMIT: usize = 64;
const EPOCH_DELAY: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Row,
    Report,
    Inventory,
}

impl Source {
    fn index(self) -> usize {
        match self {
            Self::Row => 0,
            Self::Report => 1,
            Self::Inventory => 2,
        }
    }
}

pub(crate) struct Visit<'a, T> {
    pub value: &'a T,
    source: Source,
    ordinal: u64,
    epoch: u64,
    epoch_started: Duration,
    previously_seen: bool,
}

impl<T> Visit<'_, T> {
    pub(super) fn source(&self) -> Source {
        self.source
    }
    pub(super) fn epoch_started(&self) -> Duration {
        self.epoch_started
    }
    pub(super) fn previously_seen(&self) -> bool {
        self.previously_seen
    }
}

pub(crate) struct Page<'a, T> {
    entries: Vec<Visit<'a, T>>,
    lease: Option<Arc<AtomicUsize>>,
}

impl<T> Page<'_, T> {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Visit<'_, T>> {
        self.entries.iter()
    }
}

impl<T> Drop for Page<'_, T> {
    fn drop(&mut self) {
        self.entries.clear();
        if let Some(lease) = &self.lease {
            lease.store(0, Ordering::Release);
        }
    }
}

struct Cursor {
    position: u64,
    previous_high_water: u64,
    epoch: u64,
    started: Option<Duration>,
    exhausted: bool,
    last_scan: Option<Duration>,
    revisit_lag: Option<Duration>,
}

impl Cursor {
    fn new() -> Self {
        Self {
            position: 0,
            previous_high_water: 0,
            epoch: 0,
            started: None,
            exhausted: false,
            last_scan: None,
            revisit_lag: None,
        }
    }
}

pub(super) struct Cursors {
    cursors: [Cursor; 3],
    next: usize,
    page_count: Arc<AtomicUsize>,
}

impl Cursors {
    pub(super) fn new() -> Self {
        Self {
            cursors: std::array::from_fn(|_| Cursor::new()),
            next: 0,
            page_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(super) fn next_source(&mut self) -> Source {
        let source = [Source::Row, Source::Report, Source::Inventory][self.next];
        self.next = (self.next + 1) % 3;
        source
    }

    // Ordinals are stable, positive and never reused; no carrier is copied.
    pub(super) fn page<'a, T: 'a, I: Iterator<Item = (u64, &'a T)>>(
        &mut self,
        source: Source,
        now: Duration,
        seek: impl FnOnce(u64) -> I,
    ) -> Page<'a, T> {
        let empty = || Page {
            entries: Vec::new(),
            lease: None,
        };
        if self.page_count.load(Ordering::Acquire) != 0 {
            return empty();
        }
        let cursor = &mut self.cursors[source.index()];
        let started = *cursor.started.get_or_insert(now);
        if now < started || (cursor.exhausted && now.saturating_sub(started) < EPOCH_DELAY) {
            return empty();
        }
        if cursor.exhausted {
            cursor.revisit_lag = Some(now.saturating_sub(started));
            cursor.started = Some(now);
            cursor.previous_high_water = cursor.previous_high_water.max(cursor.position);
            cursor.position = 0;
            cursor.epoch = cursor.epoch.saturating_add(1);
            cursor.exhausted = false;
        }
        cursor.last_scan = Some(now);
        let mut entries = Vec::with_capacity(PAGE_LIMIT);
        let mut stream = seek(cursor.position);
        let mut after = cursor.position;
        while entries.len() < PAGE_LIMIT {
            let Some((ordinal, value)) = stream.next() else {
                if entries.is_empty() {
                    cursor.exhausted = true;
                }
                break;
            };
            if ordinal <= after {
                break;
            }
            after = ordinal;
            entries.push(Visit {
                value,
                source,
                ordinal,
                epoch: cursor.epoch,
                epoch_started: cursor.started.unwrap_or(now),
                previously_seen: ordinal <= cursor.previous_high_water,
            });
        }
        self.page_count.store(entries.len(), Ordering::Release);
        let lease = (!entries.is_empty()).then(|| Arc::clone(&self.page_count));
        Page { entries, lease }
    }

    pub(super) fn claim<T>(&mut self, visit: &Visit<'_, T>) -> bool {
        let cursor = &mut self.cursors[visit.source.index()];
        if visit.epoch != cursor.epoch || visit.ordinal <= cursor.position {
            return false;
        }
        cursor.position = visit.ordinal;
        true
    }

    pub(super) fn page_size(&self) -> usize {
        self.page_count.load(Ordering::Acquire)
    }

    pub(super) fn visibility(
        &self,
        now: Duration,
    ) -> ([u64; 3], [Option<Duration>; 3], [Option<Duration>; 3]) {
        (
            std::array::from_fn(|i| self.cursors[i].position),
            std::array::from_fn(|i| {
                self.cursors[i]
                    .last_scan
                    .map(|scanned| now.saturating_sub(scanned))
            }),
            std::array::from_fn(|i| self.cursors[i].revisit_lag),
        )
    }
}
