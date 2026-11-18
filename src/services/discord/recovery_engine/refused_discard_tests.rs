use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use uuid::Uuid;

/// One debt effect of `episode` in `state`, with a revoked grant and an attempt in `phase`.
async fn seed(pool: &PgPool, episode: &str, state: &str, phase: &str) {
    let (effect, grant, attempt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO o_herdr_debt_effects(id,provider,channel_id,episode,input_key,coverage,plan_sha,snapshots,provenance,payload,payload_sha,unit_key,piece_index,bot_id,state) VALUES($1,'claude','7',$2,'i','c','p','[]','{}','body','h','{\"kind\":\"body\"}',0,1,$3)")
        .bind(effect).bind(episode).bind(state).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO o_herdr_debt_grants(id,effect_id,episode,claim_core,state) VALUES($1,$2,$3,'{}','revoked')")
        .bind(grant).bind(effect).bind(episode).execute(pool).await.unwrap();
    let witness = (phase == "no_effect").then(|| {
        serde_json::json!({"version": 1, "kind": "transport_no_effect",
        "attempt": attempt.to_string(), "effect": effect.to_string(), "payload_sha": "h"})
    });
    sqlx::query("INSERT INTO o_herdr_debt_attempts(id,effect_id,grant_id,payload_sha,phase,no_effect_witness) VALUES($1,$2,$3,'h',$4,$5)")
        .bind(attempt).bind(effect).bind(grant).bind(phase).bind(witness).execute(pool).await.unwrap();
    sqlx::query("UPDATE o_herdr_debt_effects SET attempt_id=$2 WHERE id=$1")
        .bind(effect)
        .bind(attempt)
        .execute(pool)
        .await
        .unwrap();
}

async fn rows(pool: &PgPool, episode: &str) -> i64 {
    sqlx::query_scalar("SELECT (SELECT count(*) FROM o_herdr_debt_effects WHERE episode=$1) + (SELECT count(*) FROM o_herdr_debt_grants WHERE episode=$1) + (SELECT count(*) FROM o_herdr_debt_attempts a JOIN o_herdr_debt_effects e ON e.id=a.effect_id WHERE e.episode=$1)")
        .bind(episode).fetch_one(pool).await.unwrap()
}

// A ready effect whose attempt proved no effect goes with its attempt and grant while the reason
// row stays; an in-flight one keeps its whole episode and writes no reason.
#[tokio::test]
async fn a_refused_episode_debt_is_deleted_attempt_first_and_an_in_flight_one_stays_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key =
        |nonce| crate::services::tui_o::channel_policy::episode_key::identity("claude", 7, nonce);
    seed(&pool, "ready", "ready", "no_effect").await;
    seed(&pool, "flight", "reserved", "reserved").await;
    seed(&pool, "other", "ready", "no_effect").await;
    let ready = key("turn-ready").unwrap();
    discard_episode(&pool, "claude", 7, "ready", &ready)
        .await
        .unwrap();
    assert_eq!(rows(&pool, "ready").await, 0);
    assert!(disposition::record(&pool, &ready).await.unwrap().is_some());
    let flight = key("turn-flight").unwrap();
    assert!(
        discard_episode(&pool, "claude", 7, "flight", &flight)
            .await
            .is_err()
    );
    assert_eq!(
        (rows(&pool, "flight").await, rows(&pool, "other").await),
        (3, 3)
    );
    assert!(disposition::record(&pool, &flight).await.unwrap().is_none());
    pool.close().await;
    drop(db);
}

// The key follows the turn nonce alone: a later view of the same turn reuses the first reason.
#[test]
fn a_rows_reason_key_ignores_its_owner_message_and_generation() {
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        7,
        None,
        1,
        2,
        3,
        "prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = Some("turn-a".into());
    let first = identity(&ProviderKind::Claude, &row).unwrap();
    (
        row.request_owner_user_id,
        row.user_msg_id,
        row.born_generation,
    ) = (9, 8, 6);
    assert_eq!(identity(&ProviderKind::Claude, &row).as_ref(), Some(&first));
    row.turn_nonce = Some("turn-b".into());
    assert_ne!(identity(&ProviderKind::Claude, &row).as_ref(), Some(&first));
}
