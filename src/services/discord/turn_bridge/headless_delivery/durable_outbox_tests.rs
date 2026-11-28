use crate::services::discord::turn_bridge::headless_delivery::durable_outbox::{
    enqueue_headless_outbox, enqueue_headless_outbox_with_origin,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::MessageId;
use std::sync::Arc;

#[tokio::test]
async fn output_origin_delayed_producer_consumer_restore_and_old_format_pg() {
    use crate::server::outbox_worker::{drain_message_outbox_once, origin_fixture::Discord};
    use crate::services::cluster::intake_router_hook::owner_record::output::{
        self,
        tests::{fixture, revoke},
    };
    let (_db, pool, origin) = fixture("72280700").await;
    let artifact =
        output::source_artifact(Some(&pool), "claude", "72280700", "90001", None, "answer")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(artifact, origin.seal("answer"));
    let message = crate::services::message_outbox::OutboxMessage {
        target: "channel:72280700",
        content: "answer",
        bot: "notify",
        source: "headless_turn",
        reason_code: Some("headless.delivery"),
        session_key: None,
    };
    let id = match enqueue_headless_outbox(
        &pool,
        message,
        Some(MessageId::new(90001)),
        &ProviderKind::Claude,
        1,
        None,
        (Some(&artifact), None),
    )
    .await
    .unwrap()
    {
        crate::services::message_outbox::OutboxEnqueueOutcome::Enqueued { id } => id,
        other => panic!("real producer must enqueue: {other:?}"),
    };
    let stored: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT output_origin FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored,
        Some(serde_json::to_value(&artifact).unwrap()),
        "the first insert must durably carry the original stamp"
    );
    let mock = Discord::new().await;
    let registry = mock.registry().await;
    let pool = Arc::new(pool);
    assert_eq!(
        drain_message_outbox_once(&pool, &registry, "origin-consumer").await,
        1
    );
    let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(status, "sent");
    assert_eq!(mock.posts().len(), 1);
    revoke(&pool, &origin).await;
    for stamp in [
        stored,
        None,
        Some(serde_json::json!({"old":"unrecognized"})),
    ] {
        sqlx::query("UPDATE message_outbox SET status='pending',output_origin=$2,claimed_at=NULL,claim_owner=NULL,next_attempt_at=NULL WHERE id=$1")
            .bind(id).bind(stamp).execute(pool.as_ref()).await.unwrap();
        assert_eq!(
            drain_message_outbox_once(&pool, &registry, "restored-consumer").await,
            1
        );
        let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(
            status, "held",
            "revoked, legacy, or malformed artifacts remain held"
        );
        assert_eq!(
            mock.posts().len(),
            1,
            "restoring old output must not emit another POST"
        );
    }
    sqlx::query("UPDATE message_outbox SET status='pending',claimed_at=NULL,claim_owner=NULL,next_attempt_at=NULL WHERE id=$1").bind(id).execute(pool.as_ref()).await.unwrap();
    sqlx::query("ALTER TABLE message_outbox RENAME COLUMN output_origin TO unavailable_origin")
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        drain_message_outbox_once(&pool, &registry, "lookup-error-consumer").await,
        1
    );
    assert_eq!(
        mock.posts().len(),
        1,
        "PG stamp read error must emit zero raw effects"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_ne!(status, "sent");
    sqlx::query("ALTER TABLE message_outbox RENAME COLUMN unavailable_origin TO output_origin")
        .execute(pool.as_ref())
        .await
        .unwrap();
    // The explicit rollout branch must also keep its existing dedupe authority and original stamp.
    let result = enqueue_headless_outbox_with_origin(
        &pool,
        message,
        Some(MessageId::new(90001)),
        &ProviderKind::Claude,
        2,
        None,
        (true, Some(&artifact), None),
    )
    .await
    .unwrap();
    let id = match result {
        crate::services::message_outbox::OutboxEnqueueOutcome::Enqueued { id } => id,
        other => panic!("{other:?}"),
    };
    let saved: serde_json::Value =
        sqlx::query_scalar("SELECT output_origin FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(saved, serde_json::to_value(&artifact).unwrap());
    let changed_message = crate::services::message_outbox::OutboxMessage {
        content: "changed answer",
        ..message
    };
    let changed_artifact = origin.seal(changed_message.content);
    let reused = enqueue_headless_outbox_with_origin(
        &pool,
        changed_message,
        Some(MessageId::new(90001)),
        &ProviderKind::Claude,
        2,
        None,
        (true, Some(&changed_artifact), None),
    )
    .await
    .unwrap();
    assert_eq!(
        reused,
        crate::services::message_outbox::OutboxEnqueueOutcome::Enqueued { id }
    );
    let stored: (String, serde_json::Value) =
        sqlx::query_as("SELECT content,output_origin FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(stored, (message.content.to_string(), saved));
    let mut wrong = serde_json::to_value(&artifact).unwrap();
    wrong["origin"]["nonce"] = serde_json::json!("different-episode");
    let wrong: output::Artifact = serde_json::from_value(wrong).unwrap();
    assert!(
        enqueue_headless_outbox_with_origin(
            &pool,
            message,
            Some(MessageId::new(90001)),
            &ProviderKind::Claude,
            2,
            None,
            (true, Some(&wrong), None),
        )
        .await
        .is_err()
    );
}
