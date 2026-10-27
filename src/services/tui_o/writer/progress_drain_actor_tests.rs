//! A long drain publishes each delivered piece, so health sees it move while it runs.

use std::time::Duration;

use super::*;
use crate::services::discord::health::o_verdict_for_tests;
use crate::services::tui_o::writer::progress;

/// What one drain left: the POSTs started, the debt still published, and health's verdict.
struct Drained {
    posts: usize,
    debt: usize,
    verdict: &'static str,
}

/// A startup binding log for `channel` naming every source its writer was switched over.
fn startup_log_on(channel: u64, writer: &mut Writer) -> Arc<FakeBindings> {
    let bindings = Arc::new(FakeBindings::new());
    let sources: Vec<SourceId> = writer.store().cursors().map(|c| c.source.clone()).collect();
    for (seq, source) in (1..).zip(sources) {
        let target = BindingTarget::Source(source);
        let mut event = bound(seq, None, target, BindingCause::Startup, None);
        event.channel_id = channel;
        bindings.commit(event);
    }
    bindings
}

/// 400 units owed behind a busy lease until health first read that debt, then 1 s POSTs if
/// `release`; health reads again 302 s after its first read.
async fn drain(channel: u64, release: bool) -> Drained {
    let (harness, path, _) = switched_over_on(channel, &row("m0", "before"));
    harness.gate.acquired();
    harness.lease.busy.store(true, Ordering::SeqCst);
    *harness.port.delay.lock().unwrap() = Some(Duration::from_secs(1));
    let mut writer = harness.writer();
    let bindings = startup_log_on(channel, &mut writer);
    let (stop, task) = spawn_with(writer, ShadowProvider::Claude, bindings);
    let rows: Vec<u8> = (1..=400)
        .flat_map(|n| row(&format!("m{n}"), &format!("piece {n}")))
        .collect();
    append(&path, &rows);
    for _ in 0..100 {
        if progress::read(channel).is_some_and(|at| at.debt >= 399) {
            break;
        }
        polls(1).await;
    }
    let first = progress::read(channel).unwrap_or_else(|| {
        let alarms = harness.alarms.taken();
        panic!(
            "no position: ended {} alarms {alarms:?}",
            task.is_finished()
        )
    });
    assert!(first.debt >= 399, "{first:?}");
    assert_eq!(o_verdict_for_tests(channel, 900), "none", "first seen now");
    let seen = tokio::time::Instant::now();
    harness.lease.busy.store(!release, Ordering::SeqCst);
    tokio::time::sleep_until(seen + Duration::from_secs(302)).await;
    let debt = progress::read(channel).expect("the actor still runs").debt;
    let verdict = o_verdict_for_tests(channel, 900);
    let posts = harness.port.posts().len();
    halt(stop, task).await;
    Drained {
        posts,
        debt,
        verdict,
    }
}

/// Every POST of the drain succeeds, so 300 s into it the position has moved all along.
#[tokio::test(start_paused = true)]
async fn a_drain_posting_every_second_for_300_s_is_no_delivery_stall() {
    let drained = drain(7_333_601, true).await;
    assert!(drained.posts > 300, "300 answered: {}", drained.posts);
    assert!(drained.debt > 0, "the drain still runs");
    assert_eq!(drained.verdict, "none");
}

/// The same backlog behind a lease held for 300 s never moves the ledger: that is the stall.
#[tokio::test(start_paused = true)]
async fn a_drain_held_on_a_busy_lease_for_300_s_is_a_delivery_stall() {
    let drained = drain(7_333_602, false).await;
    assert_eq!(drained.posts, 0);
    assert_eq!(drained.verdict, "o_delivery_stalled");
}
