use super::message_outbox_circuit_authority::{
    CircuitActivation, CircuitCoordinate, FreshVouchRevoke, activate_fenced,
    revoke_on_fresh_vouch, stage_held,
};
use sqlx::{PgPool, Row};

async fn seed_owner(pool: &PgPool, channel_id: &str, owner: &str, generation: i64) {
    sqlx::query(
        "INSERT INTO intake_session_owners
             (provider,raw_channel_id,owner_instance_id,generation,status)
         VALUES ('discord',$1,$2,$3,'active')",
    )
    .bind(channel_id)
    .bind(owner)
    .bind(generation)
    .execute(pool)
    .await
    .expect("seed current channel owner");
}

async fn stage(
    pool: &PgPool,
    channel_id: &str,
    open_generation: i64,
    authority_epoch: i64,
    dedupe_identity: &str,
) -> i64 {
    let target = format!("channel:{channel_id}");
    stage_held(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: "circuit alert",
            bot: "notify",
            source: "system",
            reason_code: Some(dedupe_identity),
            session_key: Some(channel_id),
        },
        &coordinate(channel_id, "episode-a", open_generation, authority_epoch),
        300,
    )
    .await
    .expect("stage circuit outbox row")
}

fn coordinate(
    channel_id: &str,
    episode_key: &str,
    open_generation: i64,
    authority_epoch: i64,
) -> CircuitCoordinate<'_> {
    CircuitCoordinate {
        provider: "discord",
        channel_id,
        owner_instance_id: "node-a",
        owner_generation: 7,
        episode_key,
        baseline_relay_offset: 10,
        open_generation,
        authority_epoch,
    }
}

async fn setup(name: &str) -> Option<(crate::dispatch::test_support::DispatchPostgresTestDb, PgPool)> {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(name, name).await?;
    let pool = db.connect_and_migrate().await;
    Some((db, pool))
}

#[tokio::test]
async fn vouch_before_activation_blocks_activation_pg() {
    let Some((db, pool)) = setup("outbox_circuit_vouch_before_activation").await else { return };
    seed_owner(&pool, "461501", "node-a", 7).await;
    let id = stage(&pool, "461501", 1, 1, "circuit-vouch-before").await;

    assert_eq!(
        revoke_on_fresh_vouch(&pool, &coordinate("461501", "episode-a", 1, 1), "fresh-vouch").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    assert_eq!(
        activate_fenced(&pool, id, &coordinate("461501", "episode-a", 1, 1)).await.unwrap(),
        CircuitActivation::Stale
    );
    let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "cancelled");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn activation_then_vouch_before_claim_cancels_pending_pg() {
    let Some((db, pool)) = setup("outbox_circuit_activate_then_vouch").await else { return };
    seed_owner(&pool, "461502", "node-a", 7).await;
    let id = stage(&pool, "461502", 2, 1, "circuit-activate-vouch").await;

    assert_eq!(
        activate_fenced(&pool, id, &coordinate("461502", "episode-a", 2, 1)).await.unwrap(),
        CircuitActivation::Activated
    );
    assert_eq!(
        revoke_on_fresh_vouch(&pool, &coordinate("461502", "episode-a", 2, 1), "fresh-vouch").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    let row = sqlx::query("SELECT status,cancelled_at,dedupe_key FROM message_outbox WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "cancelled");
    assert!(row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("cancelled_at").unwrap().is_some());
    assert!(row.try_get::<Option<String>, _>("dedupe_key").unwrap().is_none());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn cancelled_row_releases_dedupe_for_new_row_pg() {
    let Some((db, pool)) = setup("outbox_circuit_cancelled_dedupe").await else { return };
    seed_owner(&pool, "461503", "node-a", 7).await;
    let first = stage(&pool, "461503", 3, 1, "circuit-cancel-dedupe").await;
    revoke_on_fresh_vouch(&pool, &coordinate("461503", "episode-a", 3, 1), "fresh-vouch").await.unwrap();
    let second = stage(&pool, "461503", 4, 2, "circuit-cancel-dedupe").await;
    assert_ne!(first, second);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn authority_epoch_orders_episode_transitions_pg() {
    let Some((db, pool)) = setup("outbox_circuit_episode_epoch").await else { return };
    seed_owner(&pool, "461505", "node-a", 7).await;
    let first = stage(&pool, "461505", 1, 1, "episode-one").await;
    assert_eq!(
        activate_fenced(&pool, first, &coordinate("461505", "episode-a", 1, 1)).await.unwrap(),
        CircuitActivation::Activated
    );

    let second = stage(&pool, "461505", 1, 2, "episode-two").await;
    assert_eq!(
        activate_fenced(&pool, second, &coordinate("461505", "episode-b", 1, 2)).await.unwrap(),
        CircuitActivation::Stale
    );
    sqlx::query("UPDATE message_outbox SET circuit_episode_key='episode-b' WHERE id=$1")
        .bind(second).execute(&pool).await.unwrap();
    assert_eq!(
        activate_fenced(&pool, second, &coordinate("461505", "episode-b", 1, 2)).await.unwrap(),
        CircuitActivation::Activated
    );
    assert_eq!(
        activate_fenced(&pool, first, &coordinate("461505", "episode-a", 1, 1)).await.unwrap(),
        CircuitActivation::Stale
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn newer_epoch_reopens_after_vouch_and_old_activation_stays_stale_pg() {
    let Some((db, pool)) = setup("outbox_circuit_reopen_epoch").await else { return };
    seed_owner(&pool, "461506", "node-a", 7).await;
    let old = stage(&pool, "461506", 1, 1, "reopen-old").await;
    assert_eq!(
        revoke_on_fresh_vouch(&pool, &coordinate("461506", "episode-a", 1, 1), "fresh-vouch").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    let next = stage(&pool, "461506", 2, 2, "reopen-next").await;
    assert_eq!(
        activate_fenced(&pool, next, &coordinate("461506", "episode-a", 2, 2)).await.unwrap(),
        CircuitActivation::Activated
    );
    assert_eq!(
        activate_fenced(&pool, old, &coordinate("461506", "episode-a", 1, 1)).await.unwrap(),
        CircuitActivation::Stale
    );
    let epoch: i64 = sqlx::query_scalar(
        "SELECT authority_epoch FROM message_outbox_circuit_authority WHERE provider='discord' AND channel_id='461506'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(epoch, 2);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn staging_stamps_exact_coordinate_pg() {
    let Some((db, pool)) = setup("outbox_circuit_stage_stamp").await else { return };
    let id = stage(&pool, "461507", 9, 12, "stamp-exact").await;
    let row = sqlx::query(
        "SELECT status,circuit_provider,circuit_channel_id,circuit_episode_key,
                circuit_baseline_relay_offset,circuit_open_generation,circuit_authority_epoch,
                circuit_owner_instance_id,circuit_owner_generation
           FROM message_outbox WHERE id=$1",
    ).bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "held");
    assert_eq!(row.try_get::<String, _>("circuit_provider").unwrap(), "discord");
    assert_eq!(row.try_get::<String, _>("circuit_channel_id").unwrap(), "461507");
    assert_eq!(row.try_get::<String, _>("circuit_episode_key").unwrap(), "episode-a");
    assert_eq!(row.try_get::<i64, _>("circuit_baseline_relay_offset").unwrap(), 10);
    assert_eq!(row.try_get::<i64, _>("circuit_open_generation").unwrap(), 9);
    assert_eq!(row.try_get::<i64, _>("circuit_authority_epoch").unwrap(), 12);
    assert_eq!(row.try_get::<String, _>("circuit_owner_instance_id").unwrap(), "node-a");
    assert_eq!(row.try_get::<i64, _>("circuit_owner_generation").unwrap(), 7);
    assert!(!super::message_outbox::activate_staged_outbox_pg(&pool, id).await.unwrap());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn non_owner_activation_and_revoke_fail_closed_pg() {
    let Some((db, pool)) = setup("outbox_circuit_non_owner").await else { return };
    seed_owner(&pool, "461504", "node-b", 8).await;
    let id = stage(&pool, "461504", 4, 1, "circuit-non-owner").await;

    assert_eq!(activate_fenced(&pool, id, &coordinate("461504", "episode-a", 4, 1)).await.unwrap(), CircuitActivation::NotOwner);
    assert_eq!(revoke_on_fresh_vouch(&pool, &coordinate("461504", "episode-a", 4, 1), "fresh-vouch").await.unwrap(), FreshVouchRevoke::NotOwner);
    let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "held");
    pool.close().await;
    db.drop().await;
}
