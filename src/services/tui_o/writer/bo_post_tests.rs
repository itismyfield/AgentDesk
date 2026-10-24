//! The production hosted constructor and both request paths share one measured lifetime.

use super::*;
use crate::services::tui_o::writer::deliver::posts_in_flight;

async fn hosted(harness: &Harness) -> Writer {
    ChannelWriter::hosted(
        harness.channel(),
        Arc::clone(&harness.gate),
        Arc::clone(&harness.port),
        Arc::clone(&harness.lease),
        harness.alarms.clone(),
        None,
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn bo_legacy_hosted_post_is_counted_before_the_http_future_exists() {
    let _hosts = crate::config::session_hosts::force_for_test(Some("node-a"), &[]);
    let harness = Harness::new();
    harness.gate.acquired();
    assert_eq!(posts_in_flight(CHANNEL), None);
    let mut writer = hosted(&harness).await;
    assert_eq!(posts_in_flight(CHANNEL), Some(0));
    *harness.port.on_post.lock().unwrap() = Some(Box::new(|| {
        assert_eq!(
            posts_in_flight(CHANNEL),
            Some(1),
            "Running starts before DiscordPort::post constructs its HTTP future"
        );
    }));
    let (release, hold) = tokio::sync::oneshot::channel();
    harness.port.lazy.store(true, Ordering::SeqCst);
    *harness.port.hold.lock().unwrap() = Some(hold);
    let posted = tokio::spawn(async move { writer.deliver(&piece("bo-hosted", "piece")).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(harness.port.started.lock().unwrap().as_slice(), ["http"]);
    assert_eq!(posts_in_flight(CHANNEL), Some(1));
    release.send(()).unwrap();
    assert_eq!(posted.await.unwrap(), Step::Done);
    assert_eq!(posts_in_flight(CHANNEL), Some(0));
}

#[tokio::test]
async fn bo_legacy_hosted_repost_counter_survives_writer_recreation_pg() {
    let _hosts = crate::config::session_hosts::force_for_test(Some("node-a"), &[]);
    let never_created = u64::MAX - 7308;
    assert_eq!(posts_in_flight(never_created), None);
    assert_eq!(
        posts_in_flight(never_created),
        None,
        "a lookup does not allocate a counter"
    );
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let harness = Harness::new();
    harness.gate.acquired();
    assert_eq!(posts_in_flight(CHANNEL), None);
    let mut writer = hosted(&harness).await;
    assert_eq!(posts_in_flight(CHANNEL), Some(0));
    writer.set_live(true);
    let key = admitted(&pool, "bo-hosted-count", "piece").await;
    let mut runner = Runner::new(pool.clone(), BOT, run_scope());
    let admission = runner.try_next_dispatch(&mut writer, evidence(&pool, &key).await);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("the actual PostgreSQL grant admits the re-post");
    };
    let io = Io::over(&harness.port);
    io.hold.store(true, Ordering::SeqCst);
    let started = dispatch::start(&mut writer, permit, lease, &*io).await;
    io.waiting.notified().await;
    assert_eq!(io.wire(), 1, "the actual bounded transport request started");
    assert_eq!(posts_in_flight(CHANNEL), Some(1));
    drop((started, writer));
    let _replacement = hosted(&harness).await;
    assert_eq!(
        posts_in_flight(CHANNEL),
        Some(1),
        "the request outlives both old owners"
    );
    io.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while posts_in_flight(CHANNEL) != Some(0) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the released HTTP request drops its Running guard");
    assert_eq!(io.wire(), 1);
    assert!(
        harness.port.posts().is_empty(),
        "the re-post used its bounded transport"
    );
    pool.close().await;
    db.drop().await;
}
