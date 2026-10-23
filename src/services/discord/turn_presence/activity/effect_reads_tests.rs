//! An effect read stops deferring the observer as soon as it ends, and on its own after the bound.
use super::*;

#[tokio::test]
async fn ended_effect_read_stops_deferring_at_once() {
    const COMPLETED: u64 = 72_530_001;
    let finished = tokio::spawn(async {
        let _read = begin(COMPLETED);
        tokio::task::yield_now().await;
        assert!(pending_at(COMPLETED, Instant::now()));
    });
    finished.await.unwrap();
    assert!(!pending_at(COMPLETED, Instant::now()), "completed read");

    const CANCELLED: u64 = 72_530_002;
    let (started, running) = tokio::sync::oneshot::channel();
    let cancelled = tokio::spawn(async move {
        let _read = begin(CANCELLED);
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    running.await.unwrap();
    assert!(pending_at(CANCELLED, Instant::now()));
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    assert!(!pending_at(CANCELLED, Instant::now()), "cancelled read");

    const UNWOUND: u64 = 72_530_003;
    let unwound = std::panic::catch_unwind(|| {
        let _read = begin(UNWOUND);
        panic!("effect read unwinds");
    });
    assert!(unwound.is_err());
    assert!(!pending_at(UNWOUND, Instant::now()), "unwound read");

    const OVERLAPPING: u64 = 72_530_004;
    let first = begin(OVERLAPPING);
    let second = begin(OVERLAPPING);
    drop(first);
    assert!(
        pending_at(OVERLAPPING, Instant::now()),
        "one read still runs"
    );
    drop(second);
    assert!(!pending_at(OVERLAPPING, Instant::now()), "last read ended");
}

#[test]
fn running_effect_read_defers_only_within_its_own_bound() {
    const CHANNEL: u64 = 72_530_005;
    let start = Instant::now();
    let _first = begin_at(CHANNEL, start);
    assert!(pending_at(
        CHANNEL,
        start + DEFER_AT_MOST - Duration::from_nanos(1)
    ));
    assert!(!pending_at(CHANNEL, start + DEFER_AT_MOST), "bound reached");
    let _later = begin_at(CHANNEL, start + Duration::from_secs(20));
    assert!(
        pending_at(CHANNEL, start + DEFER_AT_MOST),
        "a later overlapping read defers on its own bound"
    );
}
