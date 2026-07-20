use super::message_outbox_circuit_authority::{
    CircuitActivation, CircuitCoordinate, FreshVouchRevoke, activate_fenced,
    revoke_on_fresh_vouch,
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

async fn stage(pool: &PgPool, channel_id: &str, generation: i64, dedupe: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO message_outbox
             (target,content,bot,source,status,dedupe_key,
              circuit_provider,circuit_channel_id,circuit_episode_key,
              circuit_baseline_relay_offset,circuit_open_generation,
              circuit_owner_instance_id,circuit_owner_generation)
         VALUES ($1,'circuit alert','notify','system','held',$2,
                 'discord',$3,'episode-a',10,$4,'node-a',7)
         RETURNING id",
    )
    .bind(format!("channel:{channel_id}"))
    .bind(dedupe)
    .bind(channel_id)
    .bind(generation)
    .fetch_one(pool)
    .await
    .expect("stage circuit outbox row")
}

fn coordinate(channel_id: &str, open_generation: i64) -> CircuitCoordinate<'_> {
    CircuitCoordinate {
        provider: "discord",
        channel_id,
        owner_instance_id: "node-a",
        owner_generation: 7,
        episode_key: "episode-a",
        baseline_relay_offset: 10,
        open_generation,
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
    let id = stage(&pool, "461501", 1, "circuit-vouch-before").await;

    assert_eq!(
        revoke_on_fresh_vouch(&pool, &coordinate("461501", 1), "fresh-vouch").await.unwrap(),
        FreshVouchRevoke::Revoked
    );
    assert_eq!(
        activate_fenced(&pool, id, &coordinate("461501", 1)).await.unwrap(),
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
    let id = stage(&pool, "461502", 2, "circuit-activate-vouch").await;

    assert_eq!(
        activate_fenced(&pool, id, &coordinate("461502", 2)).await.unwrap(),
        CircuitActivation::Activated
    );
    assert_eq!(
        revoke_on_fresh_vouch(&pool, &coordinate("461502", 2), "fresh-vouch").await.unwrap(),
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
    let first = stage(&pool, "461503", 3, "circuit-cancel-dedupe").await;
    revoke_on_fresh_vouch(&pool, &coordinate("461503", 3), "fresh-vouch").await.unwrap();
    let second = stage(&pool, "461503", 4, "circuit-cancel-dedupe").await;
    assert_ne!(first, second);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn non_owner_activation_and_revoke_fail_closed_pg() {
    let Some((db, pool)) = setup("outbox_circuit_non_owner").await else { return };
    seed_owner(&pool, "461504", "node-b", 8).await;
    let id = stage(&pool, "461504", 4, "circuit-non-owner").await;

    assert_eq!(activate_fenced(&pool, id, &coordinate("461504", 4)).await.unwrap(), CircuitActivation::NotOwner);
    assert_eq!(revoke_on_fresh_vouch(&pool, &coordinate("461504", 4), "fresh-vouch").await.unwrap(), FreshVouchRevoke::NotOwner);
    let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "held");
    pool.close().await;
    db.drop().await;
}
