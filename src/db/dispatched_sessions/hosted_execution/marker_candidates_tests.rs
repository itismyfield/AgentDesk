use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[tokio::test]
async fn marker_candidates_include_other_tokens_providers_and_legacy_aliases_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let logical = "AgentDesk-codex-adm2-candidates";
    for (provider, token, channel, name) in [
        ("codex", "token-a", "80572021", logical),
        ("claude", "token-b", "80572022", logical),
        ("claude", "token-c", "80572023", "unrelated"),
    ] {
        sqlx::query("INSERT INTO sessions (session_key,provider,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,$2,'discord_channel',$3,$4,NULL)")
            .bind(format!("{provider}/{token}/local:{name}")).bind(provider).bind(token).bind(channel).execute(&pool).await.unwrap();
    }
    let id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE discord_token_hash='token-c'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO session_key_aliases (session_id,session_key) VALUES ($1,$2)")
        .bind(id)
        .bind(format!("claude/token-c/local:{logical}"))
        .execute(&pool)
        .await
        .unwrap();
    let rows = load_marker_owner_candidates_pg(&pool, logical)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.raw.is_none()));
    assert!(
        rows.iter()
            .any(|row| row.identity[0].as_deref() == Some("claude"))
    );
    assert!(rows.iter().any(|row| row.session_id == id && row.aliases.iter().any(|key| key.ends_with(logical))));
    assert!(load_marker_owner_candidates_pg(&pool, "").await.is_err());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn marker_missing_placeholder_replays_only_its_reserved_row_and_never_overwrites_a_conflict_pg()
 {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "marker-owner".into(),
        channel_id: "80572031".into(),
        logical_key: "AgentDesk-codex-adm2-missing".into(),
        owner_node: "node".into(),
        runtime_root: "/fixture".into(),
    };
    let key = format!("codex/marker-owner/local:{}", owner.logical_key);
    let reserved = reserve_marker_row_id_pg(&pool).await.unwrap();
    assert!(
        load_marker_owner_candidates_pg(&pool, &owner.logical_key)
            .await
            .unwrap()
            .is_empty()
    );
    let first = prepare_marker_missing_pg(&pool, &owner, &key, reserved)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.session_id, reserved);
    assert_eq!(first.record, HostedRecord::Legacy);
    assert_eq!(
        prepare_marker_missing_pg(&pool, &owner, &key, reserved)
            .await
            .unwrap(),
        Some(first.clone())
    );
    let different_reservation = reserve_marker_row_id_pg(&pool).await.unwrap();
    assert!(
        prepare_marker_missing_pg(&pool, &owner, &key, different_reservation)
            .await
            .unwrap()
            .is_none()
    );
    let mut other = owner.clone();
    other.discord_token_hash = "another-account".into();
    let other_key = format!("codex/another-account/local:{}", owner.logical_key);
    assert!(
        prepare_marker_missing_pg(&pool, &other, &other_key, reserved)
            .await
            .unwrap()
            .is_none()
    );
    let rows = load_marker_owner_candidates_pg(&pool, &owner.logical_key)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, reserved);
    assert!(rows[0].raw.is_none());
    let known = super::tests::pending(&owner, "already-created");
    sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE id=$2")
        .bind(serde_json::to_value(&known).unwrap())
        .bind(reserved)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        prepare_marker_missing_pg(&pool, &owner, &key, reserved)
            .await
            .unwrap()
            .is_none()
    );
    let raw: serde_json::Value =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE id=$1")
            .bind(reserved)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(raw, serde_json::to_value(known).unwrap());
    pool.close().await;
    db.drop().await;
}
