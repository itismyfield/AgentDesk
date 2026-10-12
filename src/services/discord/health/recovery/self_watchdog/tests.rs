use super::WatchdogMode;
use super::driver;
use super::policy::{Decision, Reason, WatchdogPolicy, WatchdogState};
use crate::services::hang_forensics::{self, Breadcrumbs, HealthProbeOutcome};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const OS_ALLOWANCE: Duration = Duration::from_millis(120);

fn policy() -> WatchdogPolicy {
    WatchdogPolicy {
        check_interval: Duration::from_millis(20),
        probe_limit: Duration::from_millis(900),
        blackout_limit: Duration::from_millis(600),
        dump_limit: Duration::from_millis(350),
        ..WatchdogPolicy::DEFAULT
    }
}

fn scheduling() -> Breadcrumbs {
    Breadcrumbs {
        db_in_flight: 1,
        db_probes_started: 1,
        db_probes_failed: 0,
        last_db_ok_age_ms: Some(0),
        last_db_err_age_ms: None,
        runtime_ticks: 1,
        runtime_tick_age_ms: Some(0),
        runtime_workers: 1,
    }
}

#[derive(Default)]
struct Threads(Vec<JoinHandle<()>>);

impl Drop for Threads {
    fn drop(&mut self) {
        for thread in self.0.drain(..) {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct DumpGate(Arc<(Mutex<bool>, Condvar)>);

impl DumpGate {
    fn helper(
        &self,
        calls: Arc<Mutex<usize>>,
    ) -> impl FnOnce() -> Result<(), String> + Send + 'static {
        let gate = self.0.clone();
        move || {
            *calls.lock().unwrap() += 1;
            let (lock, changed) = &*gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            Ok(())
        }
    }
}

impl Drop for DumpGate {
    fn drop(&mut self) {
        let (lock, changed) = &*self.0;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
}

fn deadline_crossing(late_response: bool, blocked_dump: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let policy = policy();
    let started = Instant::now();
    let deadline = started + policy.blackout_limit;
    let (request_tx, request_rx) = mpsc::channel();
    let mut threads = Threads::default();
    // Stop an unused listener before joining if a regression skips the probe.
    let server_stop = ServerStop::default();
    let stopping = server_stop.0.clone();
    threads.0.push(std::thread::spawn(move || {
        let mut stream = loop {
            if stopping.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("test listener failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 1024];
        assert!(stream.read(&mut request).unwrap() > 0);
        request_tx.send(Instant::now()).unwrap();
        std::thread::sleep(
            (deadline + Duration::from_millis(220)).saturating_duration_since(Instant::now()),
        );
        if late_response {
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
        } else {
            std::thread::sleep(Duration::from_millis(600));
        }
    }));
    // Declare the dump release after thread cleanup so unwinding releases it before joining.
    let gate = DumpGate::default();
    let dump_calls = Arc::new(Mutex::new(0));
    let dump = gate.helper(dump_calls.clone());
    let (exit_tx, exit_rx) = mpsc::channel();
    let decisions = Arc::new(Mutex::new(Vec::new()));
    let recorded = decisions.clone();
    let mut state = WatchdogState::new(started);
    state.last_response_at = Some(started);
    threads.0.push(std::thread::spawn(move || {
        driver::run(
            &policy,
            WatchdogMode::Enforce,
            state,
            deadline - Duration::from_millis(150),
            driver::Hooks {
                probe: |probe_deadline| driver::tcp_probe(&addr, "127.0.0.1", probe_deadline),
                snapshot: scheduling,
                dump: move || {
                    if blocked_dump { dump() } else { Ok(()) }
                },
                log: move |state: &WatchdogState,
                           decision: &Decision,
                           _: Option<&driver::Sample>,
                           at| {
                    recorded
                        .lock()
                        .unwrap()
                        .push((*decision, at, state.fast_failures));
                },
                exit: move |reason, _| exit_tx.send((reason, Instant::now())).unwrap(),
            },
        );
    }));
    let requested_at = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        requested_at < deadline,
        "TCP request must actually start before D"
    );
    assert!(requested_at >= deadline - Duration::from_millis(170));
    let (reason, exited_at) = exit_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("OS observer must see exit sink");
    let records = decisions.lock().unwrap();
    let (_, decided_at, _) = records
        .iter()
        .find(|(decision, _, _)| matches!(decision, Decision::Exit(_)))
        .unwrap();
    eprintln!(
        "deadline crossing late={late_response} blocked_dump={blocked_dump}: request_ms={} decision_ms={} exit_sink_ms={} D_ms=600 OS_allowance_ms=120",
        requested_at.duration_since(started).as_millis(),
        decided_at.duration_since(started).as_millis(),
        exited_at.duration_since(started).as_millis()
    );
    assert_eq!(reason, Reason::HealthResponseBlackoutLimit);
    assert!(*decided_at >= deadline);
    assert!(
        *decided_at <= deadline + OS_ALLOWANCE,
        "probe added waiting after D"
    );
    assert!(
        exited_at <= deadline + OS_ALLOWANCE,
        "dump added waiting after D"
    );
    assert_eq!(
        *dump_calls.lock().unwrap(),
        0,
        "blackout exit skips dump work"
    );
}

#[test]
fn silent_tcp_probe_crossing_deadline_bounds_actual_exit_sink() {
    deadline_crossing(false, false);
}

#[test]
fn tcp_response_after_deadline_does_not_extend_blackout() {
    deadline_crossing(true, false);
}

#[test]
fn blackout_exit_skips_blocked_dump_helper() {
    deadline_crossing(false, true);
}

#[test]
fn fast_exit_waits_for_dump_only_within_remaining_blackout_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let policy = WatchdogPolicy {
        dump_limit: Duration::from_secs(5),
        ..policy()
    };
    let started = Instant::now();
    let deadline = started + policy.blackout_limit;
    let mut state = WatchdogState::new(started);
    state.fast_failures = 2;
    state.observed_failures = 2;
    let (exit_tx, exit_rx) = mpsc::channel();
    let (decision_tx, decision_rx) = mpsc::channel();
    let mut threads = Threads::default();
    let gate = DumpGate::default();
    let dump_calls = Arc::new(Mutex::new(0));
    let dump = gate.helper(dump_calls.clone());
    threads.0.push(std::thread::spawn(move || {
        driver::run(
            &policy,
            WatchdogMode::Enforce,
            state,
            deadline - Duration::from_millis(150),
            driver::Hooks {
                probe: |deadline| driver::tcp_probe(&addr, "127.0.0.1", deadline),
                snapshot: scheduling,
                dump,
                log: move |_: &WatchdogState,
                           decision: &Decision,
                           _: Option<&driver::Sample>,
                           at| {
                    decision_tx.send((*decision, at)).unwrap();
                },
                exit: move |reason, _| {
                    exit_tx.send((reason, Instant::now())).unwrap();
                },
            },
        );
    }));
    let (decision, decided_at) = decision_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(decision, Decision::Exit(Reason::ListenerGone));
    assert!(decided_at < deadline);
    let (reason, exited_at) = exit_rx
        .recv_timeout(Duration::from_millis(800))
        .expect("dump helper cannot hold the exit sink past D");
    eprintln!(
        "fast dump: decision_ms={} exit_sink_ms={} D_ms=600 OS_allowance_ms=120",
        decided_at.duration_since(started).as_millis(),
        exited_at.duration_since(started).as_millis()
    );
    assert_eq!(reason, Reason::ListenerGone);
    assert_eq!(*dump_calls.lock().unwrap(), 1);
    assert!(exited_at <= deadline + OS_ALLOWANCE);
}

#[test]
fn runtime_stall_exits_three_times_before_external_observation_limit() {
    let _serial = hang_forensics::counter_test_lock();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let mut threads = Threads::default();
    let gate = DumpGate::default();
    let stalled = gate.0.clone();
    let (block_tx, block_rx) = tokio::sync::oneshot::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel();
    threads.0.push(std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let before_ticks = hang_forensics::snapshot().runtime_ticks;
            let _armed = hang_forensics::spawn_runtime_liveness_beacon_with_period(Duration::from_millis(10));
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            while hang_forensics::snapshot().runtime_ticks.saturating_sub(before_ticks) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            ready_tx.send((before_ticks, hang_forensics::snapshot())).unwrap();
            tokio::pin!(block_rx);
            loop {
                tokio::select! {
                    _ = &mut block_rx => {
                        let (lock, changed) = &*stalled;
                        let mut released = lock.lock().unwrap();
                        while !*released { released = changed.wait(released).unwrap(); }
                        break;
                    }
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        tokio::spawn(async move {
                            let mut request = [0; 1024];
                            let _ = stream.read(&mut request).await;
                            tokio::time::sleep(Duration::from_millis(160)).await;
                            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
                        });
                    }
                }
            }
        });
    }));
    let (before_ticks, ready) = ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(ready.runtime_ticks.saturating_sub(before_ticks) >= 2);
    let policy = WatchdogPolicy {
        probe_limit: Duration::from_millis(60),
        runtime_stale_limit_ms: 100,
        blackout_limit: Duration::from_secs(10),
        dump_limit: Duration::ZERO,
        ..policy()
    };
    let started = Instant::now();
    let (exit_tx, exit_rx) = mpsc::channel();
    let (probe_tx, probe_rx) = mpsc::channel();
    threads.0.push(std::thread::spawn(move || {
        driver::run(
            &policy,
            WatchdogMode::Enforce,
            WatchdogState::new(started),
            started,
            driver::Hooks {
                probe: |deadline| driver::tcp_probe(&addr, "127.0.0.1", deadline),
                snapshot: hang_forensics::snapshot,
                dump: || Ok(()),
                log: move |state: &WatchdogState,
                           decision: &Decision,
                           sample: Option<&driver::Sample>,
                           _| {
                    probe_tx
                        .send((
                            *decision,
                            state.fast_failures,
                            sample.map(|sample| sample.current),
                        ))
                        .unwrap();
                },
                exit: move |reason, _| {
                    exit_tx.send((reason, Instant::now())).unwrap();
                },
            },
        );
    }));
    for _ in 0..4 {
        let (decision, fast, crumbs) = probe_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("four slow handler probes must survive");
        assert!(matches!(decision, Decision::Warn(_)));
        assert_eq!(fast, 0);
        assert!(crumbs.unwrap().runtime_tick_age_ms.unwrap() <= 100);
    }
    assert!(exit_rx.try_recv().is_err());
    block_tx.send(()).unwrap();
    let stalled_at = Instant::now();
    let (reason, exited_at) = exit_rx
        .recv_timeout(Duration::from_millis(1_500))
        .expect("fast exit must beat 1.5s OS observer with blackout=10s");
    assert_eq!(reason, Reason::RuntimeStalled);
    let remaining: Vec<_> = probe_rx.try_iter().collect();
    assert_eq!(remaining.last().unwrap().1, 3);
    assert!(exited_at.duration_since(stalled_at) < Duration::from_millis(1_500));
}

#[test]
fn scheduling_runtime_and_db_progress_cannot_extend_silence_deadline() {
    let _serial = hang_forensics::counter_test_lock();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let _armed = hang_forensics::spawn_runtime_liveness_beacon();
    let db = runtime.spawn(async {
        loop {
            hang_forensics::DbProbeGuard::new().finish(true);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let policy = WatchdogPolicy {
        probe_limit: Duration::from_millis(45),
        ..policy()
    };
    let started = Instant::now();
    let mut threads = Threads::default();
    let (exit_tx, exit_rx) = mpsc::channel();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    threads.0.push(std::thread::spawn(move || {
        driver::run(
            &policy,
            WatchdogMode::Enforce,
            WatchdogState::new(started),
            started,
            driver::Hooks {
                probe: |deadline| driver::tcp_probe(&addr, "127.0.0.1", deadline),
                snapshot: hang_forensics::snapshot,
                dump: || Ok(()),
                log: move |state: &WatchdogState,
                           decision: &Decision,
                           sample: Option<&driver::Sample>,
                           _| {
                    recorded.lock().unwrap().push((
                        *decision,
                        state.fast_failures,
                        sample.map(|sample| sample.current),
                    ))
                },
                exit: move |reason, _| exit_tx.send((reason, Instant::now())).unwrap(),
            },
        );
    }));
    let (reason, exited_at) = exit_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("live beacon cannot pardon silent health");
    db.abort();
    assert_eq!(reason, Reason::HealthResponseBlackoutLimit);
    assert!(exited_at <= started + policy.blackout_limit + OS_ALLOWANCE);
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|(_, fast, _)| *fast == 0));
    let probes: Vec<_> = seen.iter().filter_map(|(_, _, crumbs)| *crumbs).collect();
    assert!(probes.len() >= 3);
    assert!(probes.last().unwrap().db_probes_started > probes.first().unwrap().db_probes_started);
    assert!(probes.last().unwrap().runtime_ticks > 0);
    assert!(probes.last().unwrap().runtime_tick_age_ms.unwrap() < 5_000);
    drop(listener);
}

#[test]
fn watchdog_mode_defaults_to_enforce() {
    assert_eq!(WatchdogMode::parse(None), WatchdogMode::Enforce);
    assert_eq!(WatchdogMode::parse(Some("enforce")), WatchdogMode::Enforce);
    assert_eq!(
        WatchdogMode::parse(Some("unexpected")),
        WatchdogMode::Enforce
    );
    assert_eq!(WatchdogMode::parse(Some("observe")), WatchdogMode::Observe);
}

#[test]
fn expired_deadline_reaches_exit_sink_without_probe_snapshot_or_dump() {
    let policy = WatchdogPolicy {
        blackout_limit: Duration::from_millis(200),
        ..policy()
    };
    let called_at = Instant::now();
    let started = called_at - policy.blackout_limit - Duration::from_millis(20);
    let deadline = started + policy.blackout_limit;
    let calls = Arc::new((
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ));
    let probe_calls = calls.clone();
    let snapshot_calls = calls.clone();
    let dump_calls = calls.clone();
    let (decision_tx, decision_rx) = mpsc::channel();
    let (exit_tx, exit_rx) = mpsc::channel();
    let mut threads = Threads::default();
    threads.0.push(std::thread::spawn(move || {
        driver::run(
            &policy,
            WatchdogMode::Enforce,
            WatchdogState::new(started),
            called_at + Duration::from_secs(30),
            driver::Hooks {
                probe: move |_| {
                    probe_calls.0.fetch_add(1, Ordering::Relaxed);
                    (
                        HealthProbeOutcome::NoResponse {
                            elapsed_ms: 0,
                            error: "unexpected probe".into(),
                        },
                        Instant::now(),
                    )
                },
                snapshot: move || {
                    snapshot_calls.1.fetch_add(1, Ordering::Relaxed);
                    scheduling()
                },
                dump: move || {
                    dump_calls.2.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
                log: move |state: &WatchdogState,
                           decision: &Decision,
                           sample: Option<&driver::Sample>,
                           at| {
                    decision_tx
                        .send((*decision, at, sample.is_none(), state.observed_failures))
                        .unwrap();
                },
                exit: move |reason, _| exit_tx.send((reason, Instant::now())).unwrap(),
            },
        );
    }));
    let (reason, exited_at) = exit_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("expired D must reach the actual exit sink without a probe");
    let (decision, decided_at, no_sample, failures) =
        decision_rx.recv_timeout(Duration::from_millis(50)).unwrap();
    assert_eq!(
        decision,
        Decision::Exit(Reason::HealthResponseBlackoutLimit)
    );
    assert_eq!(reason, Reason::HealthResponseBlackoutLimit);
    assert!(no_sample);
    assert_eq!(failures, 0);
    assert!(decided_at >= called_at);
    assert!(exited_at >= decided_at);
    assert!(
        exited_at <= deadline + OS_ALLOWANCE,
        "expired D must not add a new waiting budget"
    );
    assert_eq!(calls.0.load(Ordering::Relaxed), 0);
    assert_eq!(calls.1.load(Ordering::Relaxed), 0);
    assert_eq!(calls.2.load(Ordering::Relaxed), 0);
    eprintln!(
        "expired before probe: decision_after_D_ms={} exit_sink_after_D_ms={} OS_allowance_ms=120",
        decided_at.duration_since(deadline).as_millis(),
        exited_at.duration_since(deadline).as_millis()
    );
}

#[derive(Default)]
struct ServerStop(Arc<AtomicBool>);

impl Drop for ServerStop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn observe_mode_keeps_actual_probes_without_dump_exit_or_post_deadline_spin() {
    const PROBES: usize = 5;
    struct ObservationComplete;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let mut threads = Threads::default();
    // Release the server loop before joining if any assertion unwinds.
    let server_stop = ServerStop::default();
    let stopping = server_stop.0.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = requests.clone();
    threads.0.push(std::thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(80)))
                        .unwrap();
                    let mut request = [0; 1024];
                    if stream.read(&mut request).is_ok_and(|n| n > 0) {
                        request_count.fetch_add(1, Ordering::Relaxed);
                        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("test listener failed: {error}"),
            }
        }
    }));
    let policy = WatchdogPolicy {
        check_interval: Duration::from_millis(35),
        probe_limit: Duration::from_millis(100),
        blackout_limit: Duration::from_millis(200),
        ..policy()
    };
    let called_at = Instant::now();
    let started = called_at - policy.blackout_limit - Duration::from_millis(20);
    let deadline = started + policy.blackout_limit;
    let calls = Arc::new((
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ));
    let snapshot_calls = calls.clone();
    let dump_calls = calls.clone();
    let exit_calls = calls.clone();
    let probe_starts = Arc::new(Mutex::new(Vec::new()));
    let recorded_starts = probe_starts.clone();
    let (done_tx, done_rx) = mpsc::channel();
    threads.0.push(std::thread::spawn(move || {
        let mut samples = Vec::new();
        // Stop only the test observer; production observe mode has no shutdown side effect.
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            driver::run(
                &policy,
                WatchdogMode::Observe,
                WatchdogState::new(started),
                called_at,
                driver::Hooks {
                    probe: |probe_deadline| {
                        recorded_starts.lock().unwrap().push(Instant::now());
                        driver::tcp_probe(&addr, "127.0.0.1", probe_deadline)
                    },
                    snapshot: move || {
                        snapshot_calls.0.fetch_add(1, Ordering::Relaxed);
                        scheduling()
                    },
                    dump: move || {
                        dump_calls.1.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    },
                    log: |_: &WatchdogState,
                          decision: &Decision,
                          sample: Option<&driver::Sample>,
                          at| {
                        samples.push((
                            *decision,
                            at,
                            sample.map(|sample| sample.observation.0.is_ok()),
                        ));
                        if samples.len() == PROBES {
                            std::panic::panic_any(ObservationComplete);
                        }
                    },
                    exit: move |_, _| {
                        exit_calls.2.fetch_add(1, Ordering::Relaxed);
                    },
                },
            );
        }));
        let sentinel = stopped.is_err_and(|panic| panic.is::<ObservationComplete>());
        done_tx.send((sentinel, samples)).unwrap();
    }));
    let (sentinel, samples) = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("observe mode must keep probing after D");
    assert!(
        sentinel,
        "only the finite test observer may stop the observe driver"
    );
    assert_eq!(samples.len(), PROBES);
    assert!(samples.iter().all(|(decision, at, responded)| *decision
        == Decision::Exit(Reason::HealthResponseBlackoutLimit)
        && *at >= deadline
        && *responded == Some(true)));
    assert_eq!(requests.load(Ordering::Relaxed), PROBES);
    assert_eq!(calls.0.load(Ordering::Relaxed), PROBES);
    assert_eq!(calls.1.load(Ordering::Relaxed), 0);
    assert_eq!(calls.2.load(Ordering::Relaxed), 0);
    let starts = probe_starts.lock().unwrap();
    assert_eq!(starts.len(), PROBES);
    assert!(
        starts
            .windows(2)
            .all(|pair| pair[1].duration_since(pair[0]) >= policy.check_interval),
        "expired observe mode must retain its check interval"
    );
    eprintln!(
        "observe after D: actual_TCP_probes={} snapshots={} dump_calls=0 exit_sink_calls=0 interval_ms=35 span_ms={}",
        starts.len(),
        calls.0.load(Ordering::Relaxed),
        starts.last().unwrap().duration_since(starts[0]).as_millis()
    );
}
