use super::*;
use crate::services::cluster::intake_router_hook::owner_record::{self, snapshot::tests::Fixture};
use crate::services::discord::outbound::source_registry::{
    SendCallerClass, validate_send_source_for,
};

fn description() -> Description<'static> {
    Description {
        home: "mac-a",
        home_label: "device A",
        target_label: "device B",
        new_session: false,
        home_changed: false,
        bot: "notify",
    }
}
async fn acquire(f: &Fixture, id: &OwnerIdentity, target: &str) {
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, id).await.unwrap();
    assert!(matches!(
        owner_record::acquire_owner_in_tx(&mut tx, id, target, |_| true)
            .await
            .unwrap(),
        owner_record::AcquireOutcome::AcquiredLocal { generation: 0 }
    ));
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn routing_event_real_outbox_history_and_failed_pointer_reuse_pg() {
    let f = Fixture::new("73369301").await;
    let id = OwnerIdentity::new("claude", &f.channel);
    acquire(&f, &id, "mac-a").await;
    assert!(validate_send_source_for(SOURCE, SendCallerClass::LoopbackInternal).is_ok());
    assert!(validate_send_source_for(SOURCE, SendCallerClass::Cli).is_err());
    assert!(validate_send_source_for(SOURCE, SendCallerClass::Dashboard).is_err());
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        reserve_on_tx(&mut tx, &id, 0, description())
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
    let mut events = Vec::new();
    for (generation, from, to) in [
        (0, "mac-a", "mac-b"),
        (1, "mac-b", "mac-a"),
        (2, "mac-a", "mac-b"),
    ] {
        let mut tx = f.pool.begin().await.unwrap();
        custody::lock(&mut tx, &id).await.unwrap();
        assert_eq!(
            owner_record::transfer_owner_in_tx(&mut tx, &id, from, generation, to)
                .await
                .unwrap(),
            owner_record::TransferOutcome::Transferred {
                new_generation: generation + 1
            }
        );
        let event = reserve_on_tx(&mut tx, &id, generation + 1, description())
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(event.state, DeliveryState::Pending);
        assert!(event.post_started_at.is_none() && event.message_id.is_none());
        assert_eq!(event.event.from, from);
        assert_eq!(event.event.to, to);
        events.push(event);
    }
    assert_eq!(events[0].event.content, events[2].event.content);
    assert_ne!(
        events[0].outbox_id, events[2].outbox_id,
        "distinct generations need distinct actual outbox keys"
    );
    let ids: std::collections::BTreeSet<_> = events.iter().map(|r| r.outbox_id).collect();
    let nonces: std::collections::BTreeSet<_> =
        events.iter().map(|r| r.event.nonce.clone()).collect();
    assert_eq!((ids.len(), nonces.len()), (3, 3));
    for status in ["failed", "cancelled", "sent"] {
        sqlx::query("UPDATE message_outbox SET status=$2,cancelled_at=CASE WHEN $2='cancelled' THEN NOW() ELSE NULL END WHERE id=$1")
            .bind(events[2].outbox_id)
            .bind(status)
            .execute(&f.pool)
            .await
            .unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        let recovered = reserve_on_tx(&mut tx, &id, 3, description())
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            recovered, events[2],
            "failed/cancelled history must reuse its original pointer and nonce"
        );
    }
    let mut tx = f.pool.begin().await.unwrap();
    let mut changed = description();
    changed.home = "mac-b";
    changed.home_label = "changed after commit";
    changed.bot = "other";
    let historical = reserve_on_tx(&mut tx, &id, 1, changed)
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        historical, events[0],
        "superseded generations retain their original notice"
    );
    assert!(sqlx::query("UPDATE intake_session_owners SET routing_notice_delivery_state='sent' WHERE provider='claude' AND generation=1").execute(&f.pool).await.is_err(), "a historical event cannot claim sent without its start and message receipt");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
    sqlx::query("UPDATE message_outbox SET content='tampered' WHERE id=$1")
        .bind(events[0].outbox_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    assert!(reserve_on_tx(&mut tx, &id, 1, description()).await.is_err());
    tx.rollback().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn routing_event_transaction_rollback_and_first_peer_notification_pg() {
    let f = Fixture::new("73369302").await;
    let id = OwnerIdentity::new("claude", &f.channel);
    acquire(&f, &id, "mac-a").await;
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, &id).await.unwrap();
    owner_record::transfer_owner_in_tx(&mut tx, &id, "mac-a", 0, "mac-b")
        .await
        .unwrap();
    assert!(
        reserve_on_tx(&mut tx, &id, 1, description())
            .await
            .unwrap()
            .is_some()
    );
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let owner = owner_record::read_latest_owner_in_tx(&mut f.pool.begin().await.unwrap(), &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (owner.owner_instance_id.as_str(), owner.generation),
        ("mac-a", 0)
    );
    let first = OwnerIdentity::new("codex", "73369303");
    acquire(&f, &first, "mac-b").await;
    let mut tx = f.pool.begin().await.unwrap();
    let notice = reserve_on_tx(&mut tx, &first, 0, description())
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (notice.event.from.as_str(), notice.event.to.as_str()),
        ("mac-a", "mac-b")
    );
    sqlx::query("UPDATE intake_session_owners SET status='released' WHERE provider='codex'")
        .execute(&f.pool)
        .await
        .unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, &first).await.unwrap();
    owner_record::acquire_owner_in_tx(&mut tx, &first, "mac-b", |_| true)
        .await
        .unwrap();
    assert!(
        reserve_on_tx(&mut tx, &first, 1, description())
            .await
            .unwrap()
            .is_none(),
        "same-Mac reacquisition must not emit a routing notice"
    );
    tx.commit().await.unwrap();
    assert!(
        sqlx::query(
            "UPDATE intake_session_owners SET routing_notice_outbox_id=$1 WHERE provider='claude'"
        )
        .bind(notice.outbox_id)
        .execute(&f.pool)
        .await
        .is_err(),
        "a pointer without an event must fail closed"
    );
    let deliveries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        deliveries, 0,
        "notice reservation must not fabricate a delivery"
    );
    f.finish().await;
}
