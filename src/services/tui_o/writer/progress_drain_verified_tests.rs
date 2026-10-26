//! A long verified drain publishes each settled work, so health sees it move while it runs.

use std::time::Duration;

use super::*;
use crate::services::discord::health::o_verdict_for_tests;
use crate::services::tui_o::writer::progress;

/// A permitted turn of 400 bodies behind a busy lease until health first read that debt, then 1 s
/// POSTs if `release`; returns the POSTs, debt and verdict 302 s after that first read.
async fn drain(channel: u64, release: bool) -> (usize, usize, &'static str) {
    let _gate = Gate::open(channel, true);
    let (_runtime, store, a) = switched_on(channel);
    let (mut writer, port) = owed_writer(opened(&store));
    writer.store().init_codex_provenance().unwrap();
    let bodies: Vec<String> = (1..=400).map(|n| format!("p{n}")).collect();
    let bodies: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let _permit = turn(writer.store(), &a, 1, &bodies, true);
    LEASE_BUSY.set(true);
    *port.delay.lock().unwrap() = Some(Duration::from_secs(1));
    let bindings = Bindings::on(channel);
    bindings.bind(None, &a, BindingCause::Startup);
    let (stop, stopped) = watch::channel(false);
    let codex = ShadowProvider::Codex;
    let task = tokio::spawn(run_channel(
        writer,
        codex,
        bindings,
        stopped,
        watch::channel(false).0,
    ));
    for _ in 0..100 {
        if progress::read(channel).is_some_and(|at| at.debt >= 400) {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let first = progress::read(channel).expect("the poll published its position");
    assert!(first.debt >= 400, "{first:?}");
    assert_eq!(o_verdict_for_tests(channel, 900), "none", "first seen now");
    let seen = tokio::time::Instant::now();
    LEASE_BUSY.set(!release);
    tokio::time::sleep_until(seen + Duration::from_secs(302)).await;
    let debt = progress::read(channel).expect("the actor still runs").debt;
    let verdict = o_verdict_for_tests(channel, 900);
    let posts = port.posts().len();
    LEASE_BUSY.set(false);
    stop.send(true).unwrap();
    task.await.unwrap();
    (posts, debt, verdict)
}

/// Every permitted POST succeeds, so 300 s into the drain the position has moved all along.
#[tokio::test(start_paused = true)]
async fn a_verified_drain_posting_every_second_for_300_s_is_no_delivery_stall() {
    let (posts, debt, verdict) = drain(7_333_611, true).await;
    assert!(posts > 300, "300 answered: {posts}");
    assert!(debt > 0, "the drain still runs");
    assert_eq!(verdict, "none");
}

/// The same permitted backlog behind a lease held for 300 s never moves the ledger.
#[tokio::test(start_paused = true)]
async fn a_verified_drain_held_on_a_busy_lease_for_300_s_is_a_delivery_stall() {
    let (posts, _, verdict) = drain(7_333_612, false).await;
    assert_eq!(posts, 0);
    assert_eq!(verdict, "o_delivery_stalled");
}
