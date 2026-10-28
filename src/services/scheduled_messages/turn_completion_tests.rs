use super::*;
use crate::db::scheduled_messages as db;

/// One agent delivery whose `turn_id` crossed the launch barrier, as the
/// scheduler leaves it right before the agent turn runs.
pub(crate) struct LaunchedDelivery {
    pub(crate) message_id: String,
    pub(crate) delivery_id: String,
}

/// Seed a due agent message (a snapshot one when `snapshot`), claim it and
/// commit `turn_id`'s launch. `agent_id` must be unique per database.
pub(crate) async fn launch_agent_delivery(
    pool: &PgPool,
    agent_id: &str,
    turn_id: &str,
    snapshot: bool,
    on_agent_failure: &str,
) -> LaunchedDelivery {
    sqlx::query("INSERT INTO agents (id, name, discord_channel_id) VALUES ($1, $1, '123456789')")
        .bind(agent_id)
        .execute(pool)
        .await
        .expect("seed scheduled agent");
    let snapshot_id = if snapshot {
        let id = format!("snapshot-{agent_id}");
        sqlx::query(
            "INSERT INTO scheduled_message_context_snapshots
                 (id, source_channel_id, transcript_frontier, rendered_context, pair_count, content_digest)
             VALUES ($1, '123456789', 0, 'frozen context', 0, $2)",
        )
        .bind(&id)
        .bind("0".repeat(64))
        .execute(pool)
        .await
        .expect("seed context snapshot");
        Some(id)
    } else {
        None
    };
    let message = db::insert_scheduled_message_pg(
        pool,
        &db::NewScheduledMessage {
            image_attachment: None,
            content: format!("scheduled payload for {agent_id}"),
            discord_mention_user_ids: Vec::new(),
            title: None,
            target_channel_id: Some("123456789".to_string()),
            bot: "announce".to_string(),
            delivery_kind: db::KIND_AGENT.to_string(),
            agent_id: Some(agent_id.to_string()),
            agent_instruction: None,
            on_agent_failure: on_agent_failure.to_string(),
            scheduled_at: chrono::Utc::now() - chrono::Duration::minutes(1),
            schedule: None,
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "turn_completion_test".to_string(),
            created_by: None,
            dedupe_key: None,
            provider_targets: None,
            provider_target_summary: None,
            context_strategy: if snapshot { "snapshot" } else { "fresh" }.to_string(),
            context_snapshot_id: snapshot_id,
            on_context_failure: "fail".to_string(),
        },
    )
    .await
    .expect("insert scheduled agent message");
    let mut claimed = db::claim_due_fires_pg(
        pool,
        agent_id,
        true,
        10,
        super::super::LEASE_SECS,
        chrono::Utc::now(),
    )
    .await
    .expect("claim scheduled fire");
    assert_eq!(claimed.len(), 1, "only the seeded message is due");
    let fire = claimed.pop().unwrap();
    let ids = (&message.id, &fire.delivery_id, &fire.claim_token);
    assert!(
        db::record_delivery_agent_turn_intent_pg(pool, ids.0, ids.1, ids.2, turn_id)
            .await
            .unwrap()
    );
    let lease = super::super::LEASE_SECS;
    assert!(
        db::commit_delivery_agent_launch_pg(pool, ids.0, ids.1, ids.2, turn_id, lease)
            .await
            .unwrap()
    );
    assert!(
        db::mark_delivery_agent_turn_started_pg(pool, ids.0, ids.1, ids.2, turn_id, lease)
            .await
            .unwrap()
    );
    assert!(
        db::release_agent_delivery_to_poller_pg(pool, ids.1, ids.2)
            .await
            .unwrap()
    );
    LaunchedDelivery {
        message_id: message.id,
        delivery_id: fire.delivery_id,
    }
}

/// The outcome stored on the delivery that launched `turn_id`.
pub(crate) async fn stored_outcome(pool: &PgPool, turn_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT turn_outcome FROM scheduled_message_deliveries WHERE turn_id = $1")
        .bind(turn_id)
        .fetch_optional(pool)
        .await
        .expect("read stored turn outcome")
        .flatten()
}

async fn fixture() -> (crate::db::auto_queue::test_support::TestPostgresDb, PgPool) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    (db, pool)
}

#[tokio::test]
async fn record_binds_the_first_outcome_to_the_launched_running_turn_only_pg() {
    let (db, pool) = fixture().await;
    launch_agent_delivery(&pool, "record-owner", "turn-owned", false, "fail").await;

    assert_eq!(
        record(&pool, "turn-owned", ScheduledTurnOutcome::Delivered)
            .await
            .unwrap(),
        RecordOutcome::Recorded
    );
    assert_eq!(
        record(&pool, "turn-owned", ScheduledTurnOutcome::ProviderError)
            .await
            .unwrap(),
        RecordOutcome::AlreadyRecorded(ScheduledTurnOutcome::Delivered),
        "a second completer of the same turn is evidence, not an overwrite"
    );
    assert_eq!(
        stored_outcome(&pool, "turn-owned").await.as_deref(),
        Some("delivered")
    );
    assert_eq!(
        record(&pool, "discord:1:2", ScheduledTurnOutcome::Delivered)
            .await
            .unwrap(),
        RecordOutcome::NotOwned,
        "a turn no delivery launched owns no outcome"
    );

    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn a_late_record_on_a_canceled_delivery_revives_nothing_pg() {
    let (db, pool) = fixture().await;
    let launched =
        launch_agent_delivery(&pool, "record-canceled", "turn-canceled", false, "fail").await;
    db::cancel_scheduled_message_pg(&pool, &launched.message_id)
        .await
        .expect("cancel the running delivery");
    let status_before: String =
        sqlx::query_scalar("SELECT status FROM scheduled_message_deliveries WHERE id = $1")
            .bind(&launched.delivery_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status_before, db::DELIVERY_INTERRUPTED);

    assert_eq!(
        record(&pool, "turn-canceled", ScheduledTurnOutcome::Delivered)
            .await
            .unwrap(),
        RecordOutcome::NotOwned
    );
    let status_after: String =
        sqlx::query_scalar("SELECT status FROM scheduled_message_deliveries WHERE id = $1")
            .bind(&launched.delivery_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status_after, status_before);
    assert_eq!(stored_outcome(&pool, "turn-canceled").await, None);
    let parent = db::get_scheduled_message_pg(&pool, &launched.message_id)
        .await
        .unwrap()
        .expect("canceled parent");
    assert_eq!(
        parent.status, "canceled",
        "a late completer never re-arms it"
    );

    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn legacy_snapshot_lookup_reads_the_launching_definition_pg() {
    let (db, pool) = fixture().await;
    launch_agent_delivery(&pool, "legacy-snapshot", "turn-snapshot", true, "fail").await;
    launch_agent_delivery(&pool, "legacy-fresh", "turn-fresh", false, "fail").await;

    assert_eq!(
        legacy_snapshot_turn(&pool, "turn-snapshot").await.unwrap(),
        Some(true)
    );
    assert_eq!(
        legacy_snapshot_turn(&pool, "turn-fresh").await.unwrap(),
        Some(false)
    );
    assert_eq!(
        legacy_snapshot_turn(&pool, "discord:1:2").await.unwrap(),
        None
    );

    pool.close().await;
    db.drop().await;
}
