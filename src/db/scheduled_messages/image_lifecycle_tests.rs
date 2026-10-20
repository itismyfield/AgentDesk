use super::*;
use serde_json::json;

#[tokio::test]
async fn postgres_image_rollout_includes_offline_members_and_outbox_consumers() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "image_rollout",
        "image rollout safety",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(2).await;
    assert!(!image_rollout_ready_pg(&pool).await.unwrap());
    let caps =
        crate::services::cluster::intake_worker_capabilities::capabilities_with_runtime_state(
            &json!({}),
        );
    assert_eq!(caps["scheduled_messages"]["image_consumer_v1"], true);
    assert_eq!(caps["message_outbox"]["image_consumer_v1"], true);
    sqlx::query("INSERT INTO worker_nodes (instance_id,capabilities) VALUES ('book',$1)")
        .bind(&caps)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !image_rollout_ready_pg(&pool).await.unwrap(),
        "one advertised node must not enable a two-node rollout"
    );
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id,status,capabilities) VALUES ('mini','offline','{}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !image_rollout_ready_pg(&pool).await.unwrap(),
        "stopping an old worker must not open the image gate"
    );
    sqlx::query("UPDATE worker_nodes SET capabilities=$1 WHERE instance_id='mini'")
        .bind(&caps)
        .execute(&pool)
        .await
        .unwrap();
    assert!(image_rollout_ready_pg(&pool).await.unwrap());
    sqlx::query("UPDATE worker_nodes SET capabilities=capabilities-'message_outbox' WHERE instance_id='mini'").execute(&pool).await.unwrap();
    assert!(
        !image_rollout_ready_pg(&pool).await.unwrap(),
        "outbox consumer capability is mandatory"
    );
    sqlx::query("INSERT INTO scheduled_messages (id,content,target_channel_id,scheduled_at,image_filename,image_content_type,image_data) VALUES ('old-claim','body','7001',NOW(),'image.png','image/png',$1)").bind(&b"image"[..]).execute(&pool).await.unwrap();
    let error = sqlx::query("UPDATE scheduled_messages SET status='firing' WHERE id='old-claim'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55000"),
        "old scheduled consumer must be rejected by the database"
    );
    let fires = claim_due_fires_pg(&pool, "new-image-worker", true, 10, 300, Utc::now())
        .await
        .unwrap();
    assert_eq!(fires.len(), 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_terminal_image_retention_reclaims_one_shots_and_preserves_recurring() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "image_retention",
        "image retention boundaries",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(2).await;
    let cutoff = Utc::now() - chrono::Duration::days(7);
    for (id, status, schedule, updated) in [
        (
            "old-one-shot",
            "sent",
            None,
            cutoff - chrono::Duration::microseconds(1),
        ),
        (
            "old-failed",
            "failed",
            None,
            cutoff - chrono::Duration::microseconds(1),
        ),
        ("exact-boundary", "sent", None, cutoff),
        (
            "fresh-one-shot",
            "sent",
            None,
            cutoff + chrono::Duration::microseconds(1),
        ),
        (
            "recurring",
            "sent",
            Some("0 9 * * *"),
            cutoff - chrono::Duration::days(1),
        ),
        (
            "active",
            "scheduled",
            None,
            cutoff - chrono::Duration::days(1),
        ),
        ("firing", "firing", None, cutoff - chrono::Duration::days(1)),
    ] {
        sqlx::query("INSERT INTO scheduled_messages (id,content,target_channel_id,scheduled_at,status,schedule,updated_at,image_filename,image_content_type,image_data) VALUES ($1,'body','7001',NOW(),$2,$3,$4,'image.png','image/png',$5)")
            .bind(id).bind(status).bind(schedule).bind(updated).bind(&b"image"[..]).execute(&pool).await.unwrap();
    }
    assert_eq!(
        reclaim_terminal_images_pg(&pool, cutoff, true)
            .await
            .unwrap(),
        2
    );
    let before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_messages WHERE image_data IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, 7, "dry run must retain bytes");
    reclaim_terminal_images_pg(&pool, cutoff, false)
        .await
        .unwrap();
    for id in ["old-one-shot", "old-failed"] {
        let cleared:bool=sqlx::query_scalar("SELECT image_filename IS NULL AND image_content_type IS NULL AND image_data IS NULL FROM scheduled_messages WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert!(cleared, "aged terminal one-shot image must be reclaimed");
    }
    let recurring: bool = sqlx::query_scalar(
        "SELECT image_data IS NOT DISTINCT FROM $1 FROM scheduled_messages WHERE id='recurring'",
    )
    .bind(&b"image"[..])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(recurring, "recurring image bytes must survive retention");
    let kept: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_messages WHERE image_data IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept, 5,
        "active, firing and exact/newer boundary rows must survive"
    );
    assert_eq!(
        reclaim_terminal_images_pg(&pool, cutoff, false)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_image_retention_job_wiring_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "image_retention_job",
        "image retention job wiring",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(2).await;
    for (id, schedule) in [("one-shot", None), ("recurring", Some("0 9 * * *"))] {
        sqlx::query("INSERT INTO scheduled_messages (id,content,target_channel_id,scheduled_at,status,schedule,updated_at,image_filename,image_content_type,image_data) VALUES ($1,'body','7001',NOW(),'sent',$2,NOW()-INTERVAL '8 days','image.png','image/png',$3)")
            .bind(id).bind(schedule).bind(&b"image"[..]).execute(&pool).await.unwrap();
    }
    let dry = crate::services::maintenance::jobs::db_retention::db_retention_job(&pool, true)
        .await
        .unwrap();
    assert_eq!(
        dry.get("scheduled_messages", "reclaim_image_bytes_would")
            .unwrap()
            .rows_affected,
        1
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_messages WHERE image_data IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
    let live = crate::services::maintenance::jobs::db_retention::db_retention_job(&pool, false)
        .await
        .unwrap();
    assert_eq!(
        live.get("scheduled_messages", "reclaim_image_bytes")
            .unwrap()
            .rows_affected,
        1
    );
    let rows: Vec<(String, bool)> =
        sqlx::query_as("SELECT id,image_data IS NOT NULL FROM scheduled_messages ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![("one-shot".into(), false), ("recurring".into(), true)],
        "retention pass must reclaim only the aged one-shot"
    );
    pool.close().await;
    db.drop().await;
}
