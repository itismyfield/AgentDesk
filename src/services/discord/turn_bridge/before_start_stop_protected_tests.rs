//! Policy-close probes: each request reads only its own judged points.
use super::*;
use crate::services::turn_orchestrator::policy_close::test_hooks::Judged;

const ALL: [Point; 8] = [
    Point::SubmitAttempt,
    Point::SendAccepted,
    Point::Dequeued,
    Point::MailboxCommitted,
    Point::AckObserved,
    Point::QueueExit,
    Point::FinalizeEntered,
    Point::FinalizeReturned,
];

/// Two channels closing at once each see only their own request; a commit lost before its tail
/// reads as such, and a re-ask resumes that tail without asking the mailbox again.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_probes_and_counts_per_request() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let first = Close::open_in(17, 5_341_110, |_| {}).await;
    let second = Close::open_in(17, 5_341_111, |_| {}).await;
    let (queued_first, queued_second) = (
        first.other_turn_and_queue().await,
        second.other_turn_and_queue().await,
    );
    let (acked_first, acked_second) = (first.acked().await, second.acked().await);
    let (one, two) = (
        first.intent(acked_first.clone()),
        second.intent(acked_second),
    );
    let (probe_one, probe_two) = (one.request.trace.probe(), two.request.trace.probe());
    let (a, b) = tokio::join!(first.submit(one), second.submit(two));
    assert!(matches!(a, PolicyCloseOutcome::Settled), "{a:?}");
    assert!(matches!(b, PolicyCloseOutcome::Settled), "{b:?}");
    for (probe, close) in [(&probe_one, &first), (&probe_two, &second)] {
        assert_eq!(probe.channel, close.row.channel_id);
        assert_eq!(
            probe
                .stages()
                .iter()
                .filter(|s| **s == Stage::AfterCommit)
                .count(),
            1
        );
        let read = probe.snapshot().judge(probe.request, &ALL);
        assert_eq!(read, Ok(vec![1; ALL.len()]));
        assert_eq!(probe.snapshot().complete(probe.request), Ok(()));
    }
    assert_ne!(probe_one.request, probe_two.request);
    let foreign = probe_one
        .snapshot()
        .judge(probe_two.request, &[Point::Dequeued]);
    assert_eq!(foreign, Err(Judged::Foreign));
    let missing = probe_one.snapshot().unseen(Point::QueueExit);
    let missing = missing.judge(probe_one.request, &[Point::QueueExit]);
    assert_eq!(missing, Err(Judged::Missing(Point::QueueExit)));
    let totals = step::totals();
    assert_eq!(
        totals
            .clone()
            .with_run(totals.run + 1)
            .judge(totals.run, &[]),
        Err(Judged::Foreign)
    );
    assert_released(&first, &queued_first, "first").await;
    assert_released(&second, &queued_second, "second").await;
    // A commit whose tail never started: committed, its queue exit not yet run.
    let third = Close::open_in(17, 5_341_112, |_| {}).await;
    let queued = third.other_turn_and_queue().await;
    let acked = third.acked().await;
    let intent = third.intent(acked.clone());
    let gap = intent.request.trace.probe();
    panic_once_at(&gap, Stage::BeforeFinalize);
    let outcome = third.submit(intent).await;
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic)
        ),
        "{outcome:?}"
    );
    assert_released(&third, &queued, "gap").await;
    let points = [
        Point::MailboxCommitted,
        Point::QueueExit,
        Point::FinalizeEntered,
    ];
    assert_eq!(
        gap.snapshot().judge(gap.request, &points),
        Ok(vec![1, 0, 0])
    );
    assert_eq!(
        gap.snapshot().complete(gap.request),
        Err(Judged::Incomplete)
    );
    let intent = third.intent(acked);
    let asked = intent.request.trace.probe();
    let again = third.submit(intent).await;
    assert!(matches!(again, PolicyCloseOutcome::Settled), "{again:?}");
    // The zero is this request's own reading: the sealed ticket, not a second mailbox step.
    assert_eq!(
        asked.snapshot().judge(asked.request, &points),
        Ok(vec![0, 1, 1])
    );
    assert_eq!(asked.snapshot().complete(asked.request), Ok(()));
}
