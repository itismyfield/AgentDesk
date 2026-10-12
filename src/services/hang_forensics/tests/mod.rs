use super::*;
use std::net::TcpListener;
use std::sync::MutexGuard;
use std::time::Duration;

fn exclusive() -> MutexGuard<'static, ()> {
    counter_test_lock()
}

#[test]
fn guard_records_success_and_releases_in_flight() {
    let _serial = exclusive();
    let before = snapshot();
    let guard = DbProbeGuard::new();
    let during = snapshot();
    assert_eq!(
        during.db_in_flight,
        before.db_in_flight + 1,
        "an unresolved probe must be visible as in-flight"
    );
    assert_eq!(during.db_probes_started, before.db_probes_started + 1);

    guard.finish(true);
    let after = snapshot();
    assert_eq!(
        after.db_in_flight, before.db_in_flight,
        "finishing must release the in-flight slot"
    );
    assert_eq!(
        after.db_probes_failed, before.db_probes_failed,
        "a successful probe must not count as a failure"
    );
    assert!(
        after.last_db_ok_age_ms.is_some(),
        "a successful probe must leave a success timestamp"
    );
}

#[test]
fn guard_records_failure() {
    let _serial = exclusive();
    let before = snapshot();
    DbProbeGuard::new().finish(false);
    let after = snapshot();
    assert_eq!(after.db_in_flight, before.db_in_flight);
    assert_eq!(
        after.db_probes_failed,
        before.db_probes_failed + 1,
        "a failed probe must be counted"
    );
    assert!(after.last_db_err_age_ms.is_some());
}

#[test]
fn dropping_an_unfinished_guard_still_releases_the_slot() {
    // Models a cancelled `/api/health` request dropped mid-query.
    let _serial = exclusive();
    let before = snapshot();
    drop(DbProbeGuard::new());
    let after = snapshot();
    assert_eq!(
        after.db_in_flight, before.db_in_flight,
        "a cancelled probe must not leak an in-flight slot"
    );
    assert_eq!(
        after.db_probes_started,
        before.db_probes_started + 1,
        "a cancelled probe must still be counted as started"
    );
}

/// Base value for tests that vary one field; distinct values keep a rendered
/// field identifiable in assertions.
fn crumbs() -> Breadcrumbs {
    Breadcrumbs {
        db_in_flight: 0,
        db_probes_started: 9,
        db_probes_failed: 3,
        last_db_ok_age_ms: Some(10),
        last_db_err_age_ms: Some(1234),
        runtime_ticks: 500,
        runtime_tick_age_ms: Some(120),
        runtime_workers: 14,
    }
}

/// A failed probe that reached the read timeout.
fn no_response() -> HealthProbeOutcome {
    HealthProbeOutcome::NoResponse {
        elapsed_ms: 5_000,
        error: "timed out".to_string(),
    }
}

#[test]
fn never_observed_ages_render_as_never() {
    let crumbs = Breadcrumbs {
        db_in_flight: 2,
        last_db_ok_age_ms: None,
        runtime_ticks: 0,
        runtime_tick_age_ms: None,
        ..crumbs()
    };
    let rendered = crumbs.render();
    assert!(rendered.contains("db_in_flight=2"), "{rendered}");
    assert!(rendered.contains("db_probes_started=9"), "{rendered}");
    assert!(rendered.contains("db_probes_failed=3"), "{rendered}");
    assert!(rendered.contains("last_db_ok_age_ms=never"), "{rendered}");
    assert!(rendered.contains("last_db_err_age_ms=1234"), "{rendered}");
    assert!(rendered.contains("runtime_ticks=0"), "{rendered}");
    assert!(rendered.contains("runtime_tick_age_ms=never"), "{rendered}");
    assert!(
        rendered.contains("runtime=unknown"),
        "a beacon that never ticked must not render as stalled: {rendered}"
    );
}

#[test]
fn a_live_beacon_renders_its_age_and_label() {
    let rendered = crumbs().render();
    assert!(rendered.contains("runtime=scheduling"), "{rendered}");
    assert!(rendered.contains("runtime_ticks=500"), "{rendered}");
    assert!(rendered.contains("runtime_tick_age_ms=120"), "{rendered}");
    assert!(
        rendered.contains("runtime_workers=14"),
        "`runtime=scheduling` only says ONE worker was free; without the \
         worker count beside it a reader cannot tell whether that leaves 13 \
         workers unaccounted for: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn the_beacon_records_the_runtime_worker_count() {
    let _serial = exclusive();
    let _armed = spawn_runtime_liveness_beacon();
    let after = snapshot();
    assert_eq!(
        after.runtime_workers, 3,
        "the beacon must record the worker count of the runtime it was \
         spawned on, or `runtime=scheduling` is unreadable"
    );
    assert!(
        after.render().contains("runtime_workers=3"),
        "{}",
        after.render()
    );
}

#[test]
fn age_sentinel_treats_zero_as_never() {
    assert_eq!(age_since(0, 500), None, "0 is the never-happened sentinel");
    assert_eq!(age_since(1, 500), Some(500));
}

#[test]
fn accepted_but_silent_server_is_classified_as_no_response() {
    // Accepted, but nothing is written back, as with a handler blocked on Postgres.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let accepted = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        std::thread::sleep(Duration::from_millis(600));
        drop(stream);
    });

    let outcome = probe_health_once(
        &addr,
        "127.0.0.1",
        Instant::now() + Duration::from_millis(150),
    )
    .outcome;
    accepted.join().expect("listener thread");

    assert_eq!(outcome.stage(), "no_response", "got {outcome:?}");
    assert!(!outcome.is_ok());
    assert!(
        !outcome.render().contains("err=-"),
        "a failure must carry the underlying error: {}",
        outcome.render()
    );
}

#[test]
fn dead_port_is_classified_as_connect_failed() {
    // Bind then drop, so the port is almost certainly unused.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    drop(listener);

    let outcome = probe_health_once(
        &addr,
        "127.0.0.1",
        Instant::now() + Duration::from_millis(150),
    )
    .outcome;

    assert_eq!(outcome.stage(), "connect_failed", "got {outcome:?}");
    assert!(!outcome.is_ok());
}

#[test]
fn responding_server_is_classified_as_responded() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let responder = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("request deadline");
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).expect("read health request");
            request.push(byte[0]);
            assert!(request.len() <= 4096, "health request must be bounded");
        }
        assert!(request.starts_with(b"GET /api/health HTTP/1.1\r\n"));
        stream
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")
            .expect("write response");
        stream.flush().expect("flush response");
    });

    let started = Instant::now();
    let observation = probe_health_once(&addr, "127.0.0.1", started + Duration::from_secs(2));
    let returned_at = Instant::now();
    assert!(observation.observed_at >= started);
    assert!(observation.observed_at <= returned_at);
    let outcome = observation.outcome;
    responder.join().expect("responder thread");

    assert_eq!(outcome.stage(), "responded", "got {outcome:?}");
    assert!(
        outcome.is_ok(),
        "a 503 still proves the runtime is scheduling tasks"
    );
}

#[test]
fn unparseable_address_is_connect_failed_not_a_panic() {
    let outcome = probe_health_once(
        "not-an-addr",
        "h",
        Instant::now() + Duration::from_millis(50),
    )
    .outcome;
    assert_eq!(outcome.stage(), "connect_failed", "got {outcome:?}");
}

/// A live `RequestFailed` needs the peer to vanish between `connect` and
/// `write`, a race, so the table is pinned by construction.
#[test]
fn every_stage_reports_its_own_label_elapsed_error_and_verdict_input() {
    let cases = [
        (
            HealthProbeOutcome::Responded { elapsed_ms: 7 },
            "responded",
            7u64,
            "-",
            true,
        ),
        (
            HealthProbeOutcome::ConnectFailed {
                elapsed_ms: 11,
                error: "connection refused".to_string(),
            },
            "connect_failed",
            11,
            "connection refused",
            false,
        ),
        (
            HealthProbeOutcome::RequestFailed {
                elapsed_ms: 13,
                error: "broken pipe".to_string(),
            },
            "request_failed",
            13,
            "broken pipe",
            false,
        ),
        (
            HealthProbeOutcome::NoResponse {
                elapsed_ms: 17,
                error: "timed out".to_string(),
            },
            "no_response",
            17,
            "timed out",
            false,
        ),
    ];

    for (outcome, stage, elapsed_ms, error, is_ok) in cases {
        assert_eq!(outcome.stage(), stage, "{outcome:?}");
        assert_eq!(outcome.elapsed_ms(), elapsed_ms, "{outcome:?}");
        assert_eq!(outcome.error(), error, "{outcome:?}");
        assert_eq!(
            outcome.is_ok(),
            is_ok,
            "only a response may count as healthy: {outcome:?}"
        );
        assert_eq!(
            outcome.render(),
            format!("stage={stage} elapsed_ms={elapsed_ms} err={error}"),
            "{outcome:?}"
        );
    }
}

/// The server drains the request before shutting down: closing with unread
/// bytes would make the kernel send RST, taking the `Err` arm, not `Ok(0)`.
#[test]
fn a_peer_that_takes_the_request_and_closes_without_answering_is_a_failure() {
    use std::io::Read;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let closer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let _ = stream.shutdown(std::net::Shutdown::Write);
        std::thread::sleep(Duration::from_millis(200));
    });

    let outcome =
        probe_health_once(&addr, "127.0.0.1", Instant::now() + Duration::from_secs(2)).outcome;
    closer.join().expect("closer thread");

    assert!(
        !outcome.is_ok(),
        "a peer that answered nothing is not healthy: {outcome:?}"
    );
    assert_eq!(outcome.stage(), "no_response", "got {outcome:?}");
    assert!(
        outcome.render().contains("peer closed without responding"),
        "the clean-EOF case must be distinguishable from a read timeout: {}",
        outcome.render()
    );
}

/// The kernel completes the handshake from the listen backlog with no
/// `accept`, so a runtime that stopped polling its acceptor is `no_response`.
/// If this ever fails, [`verdict`]'s reasoning is wrong.
#[test]
fn a_never_accepted_connection_is_no_response_not_connect_failed() {
    // Held open for the whole probe, never accepted.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();

    let outcome = probe_health_once(
        &addr,
        "127.0.0.1",
        Instant::now() + Duration::from_millis(200),
    )
    .outcome;
    drop(listener);

    assert_eq!(
        outcome.stage(),
        "no_response",
        "the kernel completes the handshake from the backlog with no accept, \
         so a wedged acceptor cannot surface as connect_failed: {outcome:?}"
    );
    assert!(!outcome.is_ok());
}

/// Same stage, opposite root cause: only the beacon splits them.
#[test]
fn the_beacon_splits_a_wedged_runtime_from_a_blocked_handler() {
    let no_response = no_response();

    let wedged = Breadcrumbs {
        runtime_tick_age_ms: Some(5_001),
        db_in_flight: 0,
        ..crumbs()
    };
    let blocked_on_db = Breadcrumbs {
        runtime_tick_age_ms: Some(200),
        db_in_flight: 1,
        ..crumbs()
    };

    assert_eq!(verdict(&no_response, &wedged), "runtime_stalled");
    assert_eq!(
        verdict(&no_response, &blocked_on_db),
        "handler_blocked_on_db"
    );
    assert_ne!(
        verdict(&no_response, &wedged),
        verdict(&no_response, &blocked_on_db),
        "the two failures share a stage, so the verdict is the only thing \
         that can tell an investigator which one happened"
    );
}

// Beacon thresholds are pinned to literals: an expectation computed from
// the constant under test moves with it and never fails.

#[test]
fn the_beacon_constants_are_pinned_in_absolute_units() {
    assert_eq!(
        RUNTIME_TICK_PERIOD,
        Duration::from_millis(1_000),
        "the beacon must tick once a second. Longer and a single tick \
         cannot resolve the runtime's state inside the probe's 5s window; \
         this is not free to drift with whatever the constant happens to say"
    );
    assert_eq!(
        RUNTIME_TICK_STALE_MS, 5_000,
        "the stale threshold must be 5000ms, independently of the blackout \
         deadline and the fast-failure count"
    );
    assert_eq!(
        RUNTIME_TICK_STALE_MS,
        5 * RUNTIME_TICK_PERIOD.as_millis() as u64,
        "the docs call the threshold `Five periods`; if that stops being \
         true, one of the two literals above moved and the comment lies"
    );
}

/// Pins the watchdog constants the beacon thresholds are derived from.
#[test]
fn the_stale_threshold_matches_the_watchdogs_own_probe_timeout() {
    use crate::services::discord::health::self_watchdog;

    assert_eq!(
        self_watchdog::TCP_TIMEOUT,
        Duration::from_secs(5),
        "the watchdog waits 5s for a byte. `RUNTIME_TICK_STALE_MS` is that \
         same 5s in milliseconds, so the two must be changed together"
    );
    assert_eq!(
        self_watchdog::CHECK_INTERVAL,
        Duration::from_secs(30),
        "the normal probe cadence remains 30s; it does not define the blackout deadline"
    );
    assert_eq!(
        self_watchdog::MAX_FAILURES,
        3,
        "only three consecutive fast-exit failures force-exit before blackout"
    );
    assert_eq!(
        RUNTIME_TICK_STALE_MS, 5_000,
        "the threshold is the probe's 5s read timeout expressed in ms"
    );
    assert_eq!(
        self_watchdog::BLACKOUT_LIMIT,
        Duration::from_secs(300),
        "silence has its own 300s deadline, independent of failure count"
    );
    assert_eq!(self_watchdog::DUMP_TIMEOUT, Duration::from_secs(5));
}

/// Beacon classification remains independent of the two exit-policy thresholds.
#[test]
fn a_runtime_that_stopped_ticking_reads_as_stalled_before_the_watchdog_kills() {
    for age_ms in [5_001_u64, 10_000, 35_000, 60_000, 300_000] {
        let crumbs = Breadcrumbs {
            runtime_tick_age_ms: Some(age_ms),
            ..crumbs()
        };
        assert_eq!(
            crumbs.runtime_liveness(),
            RuntimeLiveness::Stalled { age_ms },
            "a beacon {age_ms}ms stale is a stalled runtime regardless of \
             which exit-policy threshold is reached first"
        );
        assert_eq!(
            verdict(&no_response(), &crumbs),
            "runtime_stalled",
            "at {age_ms}ms stale"
        );
    }
    // The other side: a threshold mutated *down* must also be caught.
    for age_ms in [0_u64, 1, 999, 1_000, 4_999, 5_000] {
        let crumbs = Breadcrumbs {
            runtime_tick_age_ms: Some(age_ms),
            ..crumbs()
        };
        assert_eq!(
            crumbs.runtime_liveness(),
            RuntimeLiveness::Scheduling { age_ms },
            "a beacon {age_ms}ms old is inside the 5000ms window and the \
             runtime is demonstrably scheduling"
        );
        assert_ne!(
            verdict(&no_response(), &crumbs),
            "runtime_stalled",
            "at {age_ms}ms stale"
        );
    }
}

#[test]
fn verdict_reports_a_live_runtime_with_a_dead_listener_as_listener_gone() {
    let connect_failed = HealthProbeOutcome::ConnectFailed {
        elapsed_ms: 1,
        error: "connection refused".to_string(),
    };
    assert_eq!(verdict(&connect_failed, &crumbs()), "listener_gone");
}

#[test]
fn verdict_without_a_beacon_concludes_nothing() {
    let failed = HealthProbeOutcome::NoResponse {
        elapsed_ms: 5_000,
        error: "timed out".to_string(),
    };
    let no_beacon = Breadcrumbs {
        runtime_ticks: 0,
        runtime_tick_age_ms: None,
        ..crumbs()
    };
    assert_eq!(
        verdict(&failed, &no_beacon),
        "undetermined_no_beacon",
        "a missing beacon is missing evidence, not evidence of a stall"
    );
}

/// Every cell of the [`verdict`] table.
#[test]
fn every_stage_and_liveness_combination_has_a_pinned_verdict() {
    // (label, breadcrumbs)
    let liveness = [
        (
            "no beacon",
            Breadcrumbs {
                runtime_ticks: 0,
                runtime_tick_age_ms: None,
                db_in_flight: 0,
                ..crumbs()
            },
        ),
        (
            "beacon stale",
            Breadcrumbs {
                runtime_tick_age_ms: Some(5_001),
                db_in_flight: 0,
                ..crumbs()
            },
        ),
        (
            "beacon live, db idle",
            Breadcrumbs {
                runtime_tick_age_ms: Some(120),
                db_in_flight: 0,
                ..crumbs()
            },
        ),
        (
            "beacon live, db busy",
            Breadcrumbs {
                runtime_tick_age_ms: Some(120),
                db_in_flight: 1,
                ..crumbs()
            },
        ),
    ];
    let stages = [
        HealthProbeOutcome::Responded { elapsed_ms: 3 },
        HealthProbeOutcome::ConnectFailed {
            elapsed_ms: 1,
            error: "connection refused".to_string(),
        },
        HealthProbeOutcome::RequestFailed {
            elapsed_ms: 2,
            error: "broken pipe".to_string(),
        },
        no_response(),
    ];
    // Rows are stages in the order above; columns are liveness states.
    let expected = [
        ["responsive", "responsive", "responsive", "responsive"],
        [
            "undetermined_no_beacon",
            "runtime_stalled",
            "listener_gone",
            "listener_gone",
        ],
        [
            "connection_reset_before_request",
            "connection_reset_before_request",
            "connection_reset_before_request",
            "connection_reset_before_request",
        ],
        [
            "undetermined_no_beacon",
            "runtime_stalled",
            "handler_slow_db_idle",
            "handler_blocked_on_db",
        ],
    ];

    for (stage, row) in stages.iter().zip(expected) {
        for ((label, crumbs), want) in liveness.iter().zip(row) {
            let got = verdict(stage, crumbs);
            assert_eq!(
                got,
                want,
                "stage={} with {label} must render verdict={want}, got {got}",
                stage.stage()
            );
            if stage.stage() == "request_failed" {
                assert!(
                    !got.starts_with("handler_"),
                    "request_failed means the write was refused, so the \
                     handler was never reached — it must never be blamed \
                     on the handler or the database ({label} -> {got})"
                );
            }
        }
    }

    // Every verdict the watchdog can print, in one place.
    let mut rendered: Vec<&str> = expected.iter().flatten().copied().collect();
    rendered.sort_unstable();
    rendered.dedup();
    assert_eq!(
        rendered,
        [
            "connection_reset_before_request",
            "handler_blocked_on_db",
            "handler_slow_db_idle",
            "listener_gone",
            "responsive",
            "runtime_stalled",
            "undetermined_no_beacon",
        ],
        "the verdict vocabulary is 7 values; anything added must be \
         documented in the module docs and in `spawn_watchdog`"
    );
}

#[test]
fn a_healthy_probe_is_responsive_whatever_the_breadcrumbs_say() {
    let responded = HealthProbeOutcome::Responded { elapsed_ms: 3 };
    let ugly = Breadcrumbs {
        db_in_flight: 8,
        runtime_tick_age_ms: Some(5_001),
        ..crumbs()
    };
    assert_eq!(verdict(&responded, &ugly), "responsive");
}

#[test]
fn liveness_threshold_is_inclusive_and_only_then_stalls() {
    let at = Breadcrumbs {
        runtime_tick_age_ms: Some(5_000),
        ..crumbs()
    };
    let past = Breadcrumbs {
        runtime_tick_age_ms: Some(5_001),
        ..crumbs()
    };
    assert_eq!(
        at.runtime_liveness(),
        RuntimeLiveness::Scheduling { age_ms: 5_000 },
        "5000ms is the threshold and the threshold is inclusive"
    );
    assert_eq!(
        past.runtime_liveness(),
        RuntimeLiveness::Stalled { age_ms: 5_001 },
        "one millisecond past 5000ms must stall"
    );
}

#[test]
fn recording_a_tick_advances_the_beacon() {
    let _serial = exclusive();
    let before = snapshot();
    record_runtime_tick();
    let after = snapshot();
    assert_eq!(
        after.runtime_ticks,
        before.runtime_ticks + 1,
        "every tick must be counted"
    );
    let age = after
        .runtime_tick_age_ms
        .expect("a tick must leave a timestamp");
    assert!(
        age <= 5_000,
        "a tick recorded just now must read as scheduling, got {age}ms"
    );
    assert!(matches!(
        after.runtime_liveness(),
        RuntimeLiveness::Scheduling { .. }
    ));
}

/// Time is paused, so the 5s window costs no wall clock.
#[test]
fn the_spawned_beacon_keeps_ticking() {
    let _serial = exclusive();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("paused runtime");
    runtime.block_on(async {
        let before = snapshot();
        let _armed = spawn_runtime_liveness_beacon();
        tokio::time::sleep(Duration::from_millis(5_100)).await;
        let after = snapshot();
        assert!(
            after.runtime_ticks >= before.runtime_ticks + 5,
            "the beacon must tick at least 5 times in the 5s the watchdog's \
         probe is willing to wait, otherwise a single missed tick is \
         indistinguishable from a stall: {} -> {}",
            before.runtime_ticks,
            after.runtime_ticks
        );
        // No age assertion: under `start_paused`, every tick lands within a
        // millisecond of real "now", so any age bound would pass vacuously.
        after
            .runtime_tick_age_ms
            .expect("a running beacon must leave a timestamp");
    });
}

#[tokio::test]
async fn arming_the_beacon_inside_a_runtime_reports_the_worker_count() {
    let _serial = exclusive();
    let armed = spawn_runtime_liveness_beacon();
    let workers = armed
        .workers()
        .expect("on a runtime the beacon must arm and see the worker count");
    assert!(
        workers > 0,
        "a runtime has at least one worker, got {workers}"
    );
    assert_eq!(
        snapshot().runtime_workers,
        workers,
        "arming must publish the same count the breadcrumbs render; \
         `runtime_workers=0` is the signal that reads as `the beacon never started`"
    );
    let report = armed
        .boot_report()
        .expect("an armed beacon reports success");
    assert!(report.contains("armed"), "{report}");
}

#[test]
fn arming_the_beacon_off_a_runtime_is_reported_not_fatal() {
    let armed = spawn_runtime_liveness_beacon();
    assert_eq!(
        armed.workers(),
        None,
        "without a runtime handle there is nothing to spawn onto"
    );
    let report = armed
        .boot_report()
        .expect_err("a beacon that did not arm must report an error, not a success");
    assert!(
        report.contains("NOT armed") && report.contains("undetermined_no_beacon"),
        "the failure line has to name the consequence, not just the fact: {report}"
    );
}

// The hang dump beside these fields is only readable if the release build
// keeps our symbols, which `[profile.release]` decides.

/// Returns the body of `[profile.release]` from the workspace manifest.
fn release_profile_section() -> String {
    let manifest = include_str!("../../../../Cargo.toml");
    let start = manifest
        .find("\n[profile.release]\n")
        .expect("Cargo.toml must define [profile.release]");
    let body = &manifest[start + "\n[profile.release]\n".len()..];
    match body.find("\n[") {
        Some(end) => body[..end].to_string(),
        None => body.to_string(),
    }
}

/// Truncates the value at `#`, so `strip = true # keep symbols` reads as
/// `true`; none of the values read here contain `#`.
fn release_profile_key(key: &str) -> Option<String> {
    release_profile_section()
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let (name, value) = line.split_once('=')?;
            let value = value.split('#').next().unwrap_or(value);
            (name.trim() == key).then(|| value.trim().to_string())
        })
}

#[test]
fn an_inline_comment_cannot_forge_a_release_profile_value() {
    let section = "strip = true # keep symbols\ndebug = 1\n";
    let value = section
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let (name, value) = line.split_once('=')?;
            let value = value.split('#').next().unwrap_or(value);
            (name.trim() == "strip").then(|| value.trim().to_string())
        });
    assert_eq!(value.as_deref(), Some("true"));
}

#[test]
fn release_profile_keeps_the_mach_o_symbol_table() {
    // `sample`/`atos` name frames from LC_SYMTAB or a UUID-matched .dSYM;
    // LC_SYMTAB is the one that cannot get separated from the binary.
    let strip = release_profile_key("strip")
        .expect("[profile.release] must state `strip` explicitly, not inherit it");
    assert!(
        strip != "true" && strip != "\"symbols\"",
        "[profile.release] strip = {strip} removes the Mach-O symbol table, leaving \
         a UUID-matched .dSYM as the only way to symbolicate a hang dump. When that \
         dSYM is missing or stale the dump again shows only raw offsets for our own \
         frames. Use `strip = \"debuginfo\"` (drops DWARF from the executable, keeps \
         LC_SYMTAB)."
    );
}

#[test]
fn release_profile_emits_a_dsym_for_line_numbers() {
    // File:line needs debug info, which `split-debuginfo = "packed"`
    // collects into the .dSYM.
    let debug = release_profile_key("debug")
        .expect("[profile.release] must set `debug` so a .dSYM has content");
    assert!(
        debug != "0" && debug != "false" && debug != "\"none\"",
        "[profile.release] debug = {debug} emits no debug info, so the .dSYM \
         would be empty and hang dumps would carry no file:line"
    );
    assert_eq!(
        release_profile_key("split-debuginfo").as_deref(),
        Some("\"packed\""),
        "[profile.release] must set split-debuginfo = \"packed\" so the build \
         produces target/release/agentdesk.dSYM that deploy-release.sh can ship"
    );
}

#[test]
fn an_expired_probe_deadline_never_opens_a_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let addr = listener.local_addr().expect("addr").to_string();
    let started = Instant::now();
    let observation = probe_health_once(&addr, "127.0.0.1", started);

    assert_eq!(observation.outcome.stage(), "connect_failed");
    assert!(
        observation
            .outcome
            .render()
            .contains("probe deadline reached")
    );
    assert!(observation.observed_at >= started);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "expired budget must return without a socket wait; 200ms allows OS scheduling"
    );
    assert_eq!(
        listener
            .accept()
            .expect_err("expired probe must not connect")
            .kind(),
        std::io::ErrorKind::WouldBlock,
    );
}
