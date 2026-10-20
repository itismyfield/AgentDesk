use super::*;
use sqlx::Row;

async fn seed_bigint_receipt(
    pool: &PgPool,
    event: &TaskCardEvent,
    clients: &CardDeliveryClients,
    transport: &Transport,
) -> (u64, i64, String, String) {
    let count = i64::from(u32::MAX) + 1;
    let count_u64 = u64::try_from(count).unwrap();
    let rich = event
        .payload
        .render_with_completion_metadata(count_u64, &metadata());
    let clean = event.payload.render(count_u64);
    assert_ne!(rich, clean);
    let hash = content_hash(&rich);
    let claim = store::claim_card(
        Some(pool),
        &event.scope,
        "notify",
        &rich,
        &hash,
        StoreIntent::Observation,
    )
    .await
    .expect("claim new PG fixture card");
    let CardClaim::Owned(claim) = claim else {
        panic!("PG fixture must own the initial card POST")
    };
    let changed = sqlx::query(
        "UPDATE task_notification_card_state SET update_count = $5
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $6 AND delivery_state = 'posting'",
    )
    .bind(i64::try_from(event.scope.channel_id).unwrap())
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .bind(count)
    .bind(&claim.lease_owner)
    .execute(pool)
    .await
    .expect("seed owned BIGINT count before initial POST")
    .rows_affected();
    assert_eq!(changed, 1);
    let id = deliver_card_post_claim(
        Some(pool),
        transport,
        clients.preferred().unwrap(),
        &claim,
        &rich,
        &hash,
    )
    .await
    .expect("persist exact known PG card receipt");
    (id, count, rich, clean)
}

async fn assert_stored(
    pool: &PgPool,
    event: &TaskCardEvent,
    id: u64,
    count: i64,
    revision: i32,
    content: &str,
) {
    let row = sqlx::query(
        "SELECT discord_message_id, update_count, revision, rendered_content, content_hash,
                bot_key, lease_owner
         FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).unwrap())
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .fetch_one(pool)
    .await
    .expect("load exact PG metadata retirement receipt");
    assert_eq!(
        row.get::<i64, _>("discord_message_id"),
        i64::try_from(id).unwrap()
    );
    assert_eq!(row.get::<i64, _>("update_count"), count);
    assert_eq!(row.get::<i32, _>("revision"), revision);
    assert_eq!(row.get::<String, _>("rendered_content"), content);
    assert_eq!(row.get::<String, _>("content_hash"), content_hash(content));
    assert_eq!(row.get::<String, _>("bot_key"), "notify");
    assert!(row.get::<Option<String>, _>("lease_owner").is_none());
}

#[tokio::test]
async fn known_receipt_metadata_cleanup_decodes_bigint_count_pg() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "adk_tc_meta_clean",
        "task-card canonical metadata cleanup BIGINT count",
    )
    .await
    .expect("required A1 PG lane must provide an isolated PostgreSQL fixture");
    let pool = pg_db.connect_and_migrate().await;
    let event = event(581_971);
    let clients = clients();
    let transport = Transport::default();
    let (id, count, rich, clean) = seed_bigint_receipt(&pool, &event, &clients, &transport).await;
    let result = ensure_card_with_metadata(
        Some(&pool),
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
        None,
    )
    .await
    .expect("canonical metadata cleanup must decode the BIGINT count");
    assert_eq!(result.message_id, id);
    let calls = transport.calls.lock().unwrap().clone();
    assert!(
        matches!(calls.as_slice(), [Request::Post(_, original), Request::Edit(edited_id, visible)]
        if original == &rich && *edited_id == id && visible == &clean),
        "PG known-receipt cleanup must PATCH canonical body once after its original POST"
    );
    assert_stored(&pool, &event, id, count, 1, &clean).await;
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn fresh_missing_replacement_metadata_projection_decodes_bigint_count_pg() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "adk_tc_meta_replace",
        "task-card fresh replacement canonical metadata BIGINT count",
    )
    .await
    .expect("required A1 PG lane must provide an isolated PostgreSQL fixture");
    let pool = pg_db.connect_and_migrate().await;
    let event = event(581_972);
    let clients = clients();
    let transport = Transport::default();
    let (old_id, count, rich, clean) =
        seed_bigint_receipt(&pool, &event, &clients, &transport).await;
    let result = replace_confirmed_missing_card(Some(&pool), &clients, &transport, &event, old_id)
        .await
        .expect("fresh replacement must decode and project the BIGINT count before POST");
    assert_ne!(result.message_id, old_id);
    let calls = transport.calls.lock().unwrap().clone();
    assert!(
        matches!(calls.as_slice(), [Request::Post(old_nonce, original), Request::Post(new_nonce, visible)]
        if old_nonce != new_nonce && original == &rich && visible == &clean),
        "PG fresh replacement must begin its new nonce with canonical metadata-free bytes"
    );
    assert_stored(&pool, &event, result.message_id, count, 2, &clean).await;
    pool.close().await;
    pg_db.drop().await;
}
