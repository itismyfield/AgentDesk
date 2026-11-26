use super::{
    BOUND, CLAUDE_DRAFT, CLAUDE_IDLE, CLAUDE_MODAL, Capture, Logs, OLD_MESSAGE, RETRY, Rig,
    WitnessSample, step, three_looks, until_async,
};
use crate::config::TestRuntimeRootGuard;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::turn_completion_events;
use crate::services::provider::ProviderKind;
use serenity::model::id::ChannelId;

async fn notice_rows(pool: &sqlx::PgPool, channel: ChannelId) -> Vec<(i64, String, String, bool)> {
    sqlx::query_as(
        "SELECT id, reason_code, content, dedupe_key IS NOT NULL AND dedupe_expires_at IS NULL
         FROM message_outbox WHERE target = $1
         AND reason_code IN ('hold_bound.waiting', 'hold_bound.released') ORDER BY id",
    )
    .bind(format!("channel:{channel}"))
    .fetch_all(pool)
    .await
    .unwrap()
}

#[test]
fn hold_bound_target_restart_waiting_notice_persistent_dedupe_pg() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let channel = 84_342;
    let capture = Capture::new(channel, WitnessSample::at("herdr", CLAUDE_DRAFT));
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };

    let first = runtime();
    let (db, first_waiting) = first.block_on(async {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let rig = Rig::open(channel, ProviderKind::Claude, Some(pool.clone()), true).await;
        let logs = Logs::default();
        let _dispatch = tracing::dispatcher::set_default(&logs.dispatch());
        rig.lease(Some(OLD_MESSAGE)).await;

        tokio::time::pause();
        rig.hold().await;
        step(BOUND).await;
        rig.kept().await;
        assert_eq!(capture.reads(), 1);
        tokio::time::resume();

        until_async(
            || async {
                !notice_rows(&pool, rig.channel).await.is_empty()
                    && logs.read().contains("input_hold_bound_waiting")
            },
            "the first waiting notice did not enqueue",
        )
        .await;
        let waiting = notice_rows(&pool, rig.channel).await;
        assert_eq!(waiting.len(), 1, "{waiting:?}");
        assert_eq!(waiting[0].1, "hold_bound.waiting");
        assert!(waiting[0].2.contains("화면 초안"), "{waiting:?}");
        assert!(waiting[0].3, "{waiting:?}");
        drop(rig);
        pool.close().await;
        (db, waiting)
    });
    drop(first);

    capture.next([WitnessSample::at("herdr", CLAUDE_MODAL)]);
    runtime().block_on(async {
        let pool = db.connect_and_migrate().await;
        let rig = Rig::open(channel, ProviderKind::Claude, Some(pool.clone()), false).await;
        let logs = Logs::default();
        let _dispatch = tracing::dispatcher::set_default(&logs.dispatch());
        rig.lease(Some(OLD_MESSAGE)).await;
        let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);

        tokio::time::pause();
        rig.hold().await;
        step(BOUND).await;
        rig.kept().await;
        assert_eq!(capture.reads(), 2);
        tokio::time::resume();

        until_async(
            || async {
                !notice_rows(&pool, rig.channel).await.is_empty()
                    && logs.read().lines().any(|line| {
                        line.contains("input_hold_bound_waiting") && line.contains("modal")
                    })
            },
            "the restarted actor did not enqueue the modal waiting notice",
        )
        .await;
        assert_eq!(notice_rows(&pool, rig.channel).await, first_waiting);
        assert!(events.try_recv().is_err());

        capture.next([WitnessSample::at("herdr", CLAUDE_IDLE)]);
        tokio::time::pause();
        step(RETRY).await;
        three_looks().await;
        rig.accept(&mut events).await;
        assert_eq!(capture.reads(), 5);
        tokio::time::resume();

        until_async(
            || async { notice_rows(&pool, rig.channel).await.len() >= 2 },
            "the released notice did not enqueue",
        )
        .await;
        let rows = notice_rows(&pool, rig.channel).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], first_waiting[0]);
        assert_ne!(rows[1].0, rows[0].0);
        assert_eq!(rows[1].1, "hold_bound.released");
        assert!(rows[1].3, "{rows:?}");
        assert!(events.try_recv().is_err());
        drop(rig);
        pool.close().await;
        db.drop().await;
    });
}
