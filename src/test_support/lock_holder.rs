//! Holder record for the process-wide test mutexes: the shared env lock (E) and the PG lifecycle lock (P).
//! A watchdog thread names the holder whenever one holder has kept a waiter parked past `STALL_REPORT_AFTER`.

use std::io::Write;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, Once, PoisonError, TryLockError};
use std::time::{Duration, Instant};

/// Longer than any i7441 seam's normal run, shorter than the 60s seam bound that fails the test.
const STALL_REPORT_AFTER: Duration = Duration::from_secs(30);
const REPEAT_EVERY: Duration = Duration::from_secs(300);
const WATCH_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) static ENV_LOCK: LockSlot = LockSlot::new("config::shared_test_env_lock (E)");
pub(crate) static PG_LIFECYCLE_LOCK: LockSlot =
    LockSlot::new("db::postgres::POSTGRES_TEST_LIFECYCLE_LOCK (P)");

pub(crate) struct LockSlot {
    name: &'static str,
    holder: Mutex<Option<Party>>,
}

#[derive(Clone)]
struct Party {
    thread: String,
    at: &'static Location<'static>,
    since: Instant,
}

struct Wait {
    id: u64,
    slot: &'static LockSlot,
    waiter: Party,
    reported: Option<Instant>,
}

static WAITS: Mutex<Vec<Wait>> = Mutex::new(Vec::new());
static NEXT_WAIT: AtomicU64 = AtomicU64::new(0);
static WATCHDOG: Once = Once::new();

impl Party {
    fn current(at: &'static Location<'static>) -> Self {
        let thread = std::thread::current();
        // libtest names each test thread after its test, so this is the test name.
        let name = thread
            .name()
            .map_or_else(|| format!("{:?}", thread.id()), str::to_string);
        Self {
            thread: name,
            at,
            since: Instant::now(),
        }
    }
}

impl LockSlot {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            holder: Mutex::new(None),
        }
    }

    fn holder(&self) -> MutexGuard<'_, Option<Party>> {
        self.holder.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Blocks on `mutex` exactly like `lock()` (poison is ignored) and records the caller as holder.
    #[track_caller]
    pub(crate) fn lock(&'static self, mutex: &'static Mutex<()>) -> MutexGuard<'static, ()> {
        let at = Location::caller();
        let guard = match mutex.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(TryLockError::WouldBlock) => {
                let id = self.enter_wait(at);
                let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
                leave_wait(id);
                guard
            }
        };
        *self.holder() = Some(Party::current(at));
        guard
    }

    /// Called from the owning guard's `Drop` before its inner `MutexGuard` releases the mutex.
    pub(crate) fn release(&self) {
        *self.holder() = None;
    }

    fn enter_wait(&'static self, at: &'static Location<'static>) -> u64 {
        WATCHDOG.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("test-lock-holder-watchdog".to_string())
                .spawn(|| {
                    loop {
                        std::thread::sleep(WATCH_INTERVAL);
                        let lines = stall_lines(Instant::now(), None);
                        // Straight to fd 2: libtest captures `eprintln!` of a stuck test and never prints it.
                        let mut stderr = std::io::stderr().lock();
                        for line in lines {
                            let _ = writeln!(stderr, "{line}");
                        }
                    }
                });
        });
        let id = NEXT_WAIT.fetch_add(1, Ordering::Relaxed);
        waits().push(Wait {
            id,
            slot: self,
            waiter: Party::current(at),
            reported: None,
        });
        id
    }
}

fn waits() -> MutexGuard<'static, Vec<Wait>> {
    WAITS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn leave_wait(id: u64) {
    waits().retain(|wait| wait.id != id);
}

/// One line per waiter that a single holder has blocked for `STALL_REPORT_AFTER`, repeated every `REPEAT_EVERY`.
fn stall_lines(now: Instant, only: Option<&'static LockSlot>) -> Vec<String> {
    let mut lines = Vec::new();
    for wait in waits().iter_mut() {
        if only.is_some_and(|slot| !std::ptr::eq(slot, wait.slot)) {
            continue;
        }
        let holder = wait.slot.holder().clone();
        let blocked_since = holder
            .as_ref()
            .map_or(wait.waiter.since, |held| held.since.max(wait.waiter.since));
        if now.saturating_duration_since(blocked_since) < STALL_REPORT_AFTER
            || wait
                .reported
                .is_some_and(|at| now.saturating_duration_since(at) < REPEAT_EVERY)
        {
            continue;
        }
        wait.reported = Some(now);
        let holder = holder.map_or_else(
            || "unrecorded (a direct `.lock()` site outside the canonical acquire)".to_string(),
            |held| {
                format!(
                    "`{}` acquired at {} {}s ago",
                    held.thread,
                    held.at,
                    now.saturating_duration_since(held.since).as_secs()
                )
            },
        );
        lines.push(format!(
            "TEST_LOCK_STALL {}: waiter `{}` at {} parked {}s; holder {holder}",
            wait.slot.name,
            wait.waiter.thread,
            wait.waiter.at,
            now.saturating_duration_since(wait.waiter.since).as_secs()
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parked_waiter_is_reported_with_the_holding_test_and_its_acquire_site() {
        static SLOT: LockSlot = LockSlot::new("lock_holder test slot");
        static MUTEX: Mutex<()> = Mutex::new(());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::Builder::new()
            .name("holding-test".into())
            .spawn(move || {
                let guard = SLOT.lock(&MUTEX);
                held_tx.send(line!() - 1).unwrap();
                release_rx.recv().unwrap();
                SLOT.release();
                drop(guard);
            })
            .unwrap();
        let held_line = held_rx.recv().unwrap();
        let waiter = std::thread::Builder::new()
            .name("waiting-test".into())
            .spawn(|| drop(SLOT.lock(&MUTEX)))
            .unwrap();
        let parked = Instant::now();
        while !waits().iter().any(|wait| std::ptr::eq(wait.slot, &SLOT)) {
            assert!(
                parked.elapsed() < Duration::from_secs(10),
                "waiter never parked"
            );
            std::thread::yield_now();
        }

        let late = Instant::now() + STALL_REPORT_AFTER + Duration::from_secs(1);
        let ours = |now| stall_lines(now, Some(&SLOT));
        assert!(ours(Instant::now()).is_empty());
        let report = ours(late);
        assert_eq!(report.len(), 1, "{report:?}");
        assert!(report[0].contains("waiter `waiting-test`"), "{report:?}");
        let site = format!("`holding-test` acquired at {}:{held_line}:", file!());
        assert!(report[0].contains(&site), "{report:?} lacks {site}");
        assert!(ours(late).is_empty(), "repeats are rate-limited");

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        waiter.join().unwrap();
        assert!(!waits().iter().any(|wait| std::ptr::eq(wait.slot, &SLOT)));
    }
}
