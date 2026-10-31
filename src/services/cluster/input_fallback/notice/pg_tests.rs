use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::message_outbox::{
    LIFECYCLE_NOTIFIER_SOURCE, OutboxMessage,
    enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx,
};

#[tokio::test]
async fn routing_notice_real_outbox_dedupe_reuses_event_and_separates_switches_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let identity = OwnerIdentity::new("claude", "7001");
    let mut ids = Vec::new();
    for (generation, from, to) in [(1, "A", "B"), (2, "B", "A"), (3, "A", "B"), (1, "A", "B")] {
        let notice = prepare(
            &Transition {
                identity: &identity,
                generation,
                from,
                to,
                home: "A",
            },
            Labels { home: "A", to },
            false,
            false,
        )
        .unwrap()
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let id = enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
            &mut tx,
            OutboxMessage {
                target: &notice.target,
                content: "동일한 문구",
                bot: "notify",
                source: LIFECYCLE_NOTIFIER_SOURCE,
                reason_code: Some(notice.reason_code),
                session_key: Some(&notice.session_key),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let key: String = sqlx::query_scalar("SELECT dedupe_key FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(key, notice.dedupe_key);
        ids.push(id);
    }
    assert_eq!(ids[0], ids[3]);
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(ids[1], ids[2]);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE target='channel:7001'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 3);
    pool.close().await;
    db.drop().await;
}
