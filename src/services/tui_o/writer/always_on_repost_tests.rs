use super::*;
use crate::services::tui_o::repost::io::probe::evidence::{FIRST_PASS_AFTER, SECOND_PASS_AFTER};

fn default_hook(harness: &Harness, pool: PgPool, io: &Arc<Io>) -> ChannelTurn<Io> {
    let runner = Runner::new(pool, BOT, run_scope());
    let io = Arc::clone(io);
    ChannelTurn::new(
        runner,
        Box::new(move || Some(Arc::clone(&io))),
        CHANNEL,
        sidecar(harness),
    )
}

async fn confirmation_case(found: bool, restart: bool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let harness = Harness::new();
    harness.gate.acquired();
    let io = Io::over(&harness.port);
    let hook = default_hook(&harness, pool.clone(), &io);
    let mut writer = harness.writer().with_repost(Some(Box::new(hook)));
    assert_eq!(
        writer.deliver(&piece("proof", "read permission")).await,
        Step::Done
    );
    harness.port.replies.lock().unwrap().push_back(if found {
        Reply::CreatedUnseen
    } else {
        Reply::Unsent
    });
    harness.port.unreadable.store(true, Ordering::SeqCst);
    assert_eq!(
        writer.deliver(&piece("answer", "one answer")).await,
        Step::Done
    );
    harness.port.unreadable.store(false, Ordering::SeqCst);

    tokio::time::pause();
    // Keep PG I/O from advancing the paused clock to its connection timeout.
    let clock = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    writer.repost_turn().await;
    let answer = key("answer");
    assert!(
        load(&pool, &answer).await.unwrap().is_some(),
        "default path admits the uncertain original"
    );
    assert_eq!(
        results(&pool, &answer).await,
        [Some(AttemptResult::Uncertain)]
    );
    assert_eq!(io.wire(), 0);
    let reads = io.reads.load(Ordering::SeqCst);
    tokio::time::advance(FIRST_PASS_AFTER - Duration::from_nanos(1)).await;
    writer.repost_turn().await;
    assert_eq!(
        io.reads.load(Ordering::SeqCst),
        reads,
        "first confirmation cannot start early"
    );
    tokio::time::advance(Duration::from_nanos(1)).await;
    writer.repost_turn().await;
    assert!(
        io.reads.load(Ordering::SeqCst) > reads,
        "first confirmation actually reads history"
    );
    assert_eq!(io.wire(), 0, "first pass never creates a duplicate");

    if restart {
        drop(writer);
        let hook = default_hook(&harness, pool.clone(), &io);
        writer = harness.writer().with_repost(Some(Box::new(hook)));
        writer.repost_turn().await;
        tokio::time::advance(FIRST_PASS_AFTER - Duration::from_nanos(1)).await;
        writer.repost_turn().await;
        assert_eq!(
            io.wire(),
            0,
            "restart cannot reuse the previous confirmation"
        );
        tokio::time::advance(Duration::from_nanos(1)).await;
        writer.repost_turn().await;
        assert_eq!(io.wire(), 0, "restart needs its own second pass");
    }
    tokio::time::advance(SECOND_PASS_AFTER - Duration::from_nanos(1)).await;
    writer.repost_turn().await;
    assert_eq!(io.wire(), 0, "second confirmation cannot grant early");
    tokio::time::advance(Duration::from_nanos(1)).await;
    writer.repost_turn().await;
    assert_eq!(
        io.wire(),
        usize::from(!found),
        "confirmed original means zero reposts; absent means exactly one"
    );
    assert!(io.reads.load(Ordering::SeqCst) > 0);
    assert_eq!(
        results(&pool, &answer).await.len(),
        if found { 1 } else { 2 }
    );
    assert_eq!(
        harness
            .port
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.content == "one answer")
            .count(),
        1
    );
    for _ in 0..3 {
        tokio::time::advance(FIRST_PASS_AFTER + SECOND_PASS_AFTER).await;
        writer.repost_turn().await;
        assert_eq!(
            writer.deliver(&piece("answer", "one answer")).await,
            Step::Done
        );
    }
    assert_eq!(
        io.wire(),
        usize::from(!found),
        "later turns and original replay cannot duplicate the answer"
    );
    let id = harness
        .port
        .history
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.content == "one answer")
        .unwrap()
        .id;
    assert!(
        is_receipt(&pool, &answer, id).await,
        "the successful path records a shared receipt"
    );
    drop(writer);
    clock.abort();
    tokio::time::resume();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn always_on_first_post_confirmed_never_reposts_pg() {
    confirmation_case(true, false).await;
}

#[tokio::test]
async fn always_on_missing_post_reposts_once_at_confirmation_boundary_pg() {
    confirmation_case(false, false).await;
}

#[tokio::test]
async fn always_on_restart_requires_fresh_confirmation_before_one_repost_pg() {
    confirmation_case(false, true).await;
}
