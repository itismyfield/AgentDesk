//! Capped waits for tests: a join, recv or future that outlives its cap panics with the wait,
//! the elapsed time and the caller, instead of holding the whole test worker until its timeout.
use std::future::Future;
use std::panic::Location;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default cap, far below the CI worker limit so one stuck test fails alone; longer caps are explicit.
pub(crate) const WAIT_CAP: Duration = Duration::from_secs(30);

/// Joins `handle` within `cap`, re-raising the thread's own panic; past the cap the thread is left behind.
#[track_caller]
pub(crate) fn join_within<T>(handle: JoinHandle<T>, cap: Duration) -> T {
    let caller = Location::caller();
    let started = Instant::now();
    while !handle.is_finished() {
        if started.elapsed() >= cap {
            let name = handle.thread().name().unwrap_or("unnamed").to_owned();
            overran(&format!("join of thread `{name}`"), started, cap, caller);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    handle
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}

/// Receives one value within `cap`; a dropped sender is also a failure, not a hang.
#[track_caller]
pub(crate) fn recv_within<T>(rx: &Receiver<T>, cap: Duration) -> T {
    let caller = Location::caller();
    let started = Instant::now();
    match rx.recv_timeout(cap) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => overran("channel recv", started, cap, caller),
        Err(RecvTimeoutError::Disconnected) => {
            panic!("channel recv at {caller}: every sender dropped without a value")
        }
    }
}

/// Awaits `fut` within `cap` on the current tokio runtime.
#[track_caller]
pub(crate) fn await_within<F: Future>(fut: F, cap: Duration) -> impl Future<Output = F::Output> {
    let caller = Location::caller();
    async move {
        let started = Instant::now();
        match tokio::time::timeout(cap, fut).await {
            Ok(value) => value,
            Err(_) => overran("await of future", started, cap, caller),
        }
    }
}

fn overran(what: &str, started: Instant, cap: Duration, caller: &Location<'_>) -> ! {
    panic!(
        "{what} at {caller} did not finish within its cap of {cap:?} (waited {:?})",
        started.elapsed()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;

    fn panic_text(result: std::thread::Result<impl Sized>) -> String {
        let payload = result.err().expect("the wait must panic");
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default()
    }

    /// A thread parked on a channel never finishes on its own; the capped join must end the wait,
    /// not the test worker. The waiter sits on its own thread so a hang shows up as "not finished".
    #[test]
    fn join_within_cuts_a_never_finishing_thread_at_its_cap() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let stuck = std::thread::Builder::new()
            .name("stuck-worker".into())
            .spawn(move || release_rx.recv())
            .unwrap();
        let waiter = std::thread::spawn(move || {
            catch_unwind(AssertUnwindSafe(|| {
                let _ = join_within(stuck, Duration::from_secs(1));
            }))
        });
        let started = Instant::now();
        while !waiter.is_finished() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the capped join did not return within 5s"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let text = panic_text(waiter.join().unwrap());
        assert!(text.contains("join of thread `stuck-worker`"), "{text}");
        assert!(text.contains("cap of 1s"), "{text}");
        assert!(text.contains(file!()), "{text}");
        release_tx.send(()).unwrap();
    }

    #[test]
    fn join_within_returns_the_value_and_reraises_the_thread_panic() {
        assert_eq!(join_within(std::thread::spawn(|| 7), WAIT_CAP), 7);
        let failing = std::thread::spawn(|| -> u8 { panic!("worker failed") });
        let text = panic_text(catch_unwind(AssertUnwindSafe(|| {
            join_within(failing, WAIT_CAP)
        })));
        assert_eq!(text, "worker failed");
    }

    #[test]
    fn recv_within_returns_the_value_or_panics_at_its_cap() {
        let (tx, rx) = mpsc::channel();
        tx.send(3).unwrap();
        assert_eq!(recv_within(&rx, WAIT_CAP), 3);
        let text = panic_text(catch_unwind(AssertUnwindSafe(|| {
            recv_within(&rx, Duration::from_millis(20))
        })));
        assert!(text.contains("channel recv"), "{text}");
        assert!(text.contains("cap of 20ms"), "{text}");
        assert!(text.contains(file!()), "{text}");
        drop(tx);
        let text = panic_text(catch_unwind(AssertUnwindSafe(|| {
            recv_within(&rx, WAIT_CAP)
        })));
        assert!(text.contains("every sender dropped"), "{text}");
    }

    #[test]
    fn await_within_panics_at_its_cap_on_a_paused_clock() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(await_within(async { 5 }, WAIT_CAP)), 5);
        let text = panic_text(catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(await_within(
                std::future::pending::<()>(),
                Duration::from_secs(2),
            ))
        })));
        assert!(text.contains("await of future"), "{text}");
        assert!(text.contains("cap of 2s"), "{text}");
        assert!(text.contains(file!()), "{text}");
    }
}
