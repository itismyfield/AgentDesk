use super::super::*;
use crate::services::message_outbox::*;

type LegacyTextRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
);

async fn image_pool() -> (
    crate::dispatch::test_support::DispatchPostgresTestDb,
    PgPool,
) {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_outbox_image",
        "outbox image transport",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    (db, pool)
}

fn message(reason: &str) -> OutboxMessage<'_> {
    OutboxMessage {
        target: "channel:7001",
        content: "unchanged text 한국어",
        bot: "notify",
        source: "scheduled_message",
        reason_code: Some(reason),
        session_key: None,
    }
}

fn image() -> OutboxAttachment<'static> {
    OutboxAttachment {
        filename: "image.png",
        content_type: "image/png",
        data: b"\x89PNG\r\n\x1a\n\0\xffpreserved-binary",
    }
}

#[tokio::test]
async fn image_outbox_paths_preserve_retry_restart_and_dedupe_pg() {
    let (db, pool) = image_pool().await;
    let mut tx = pool.begin().await.unwrap();
    let persistent =
        enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("persistent"), Some(image()))
            .await
            .unwrap();
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let duplicate = enqueue_outbox_pg_with_attachment_on_tx(
        &mut tx,
        message("persistent"),
        Some(OutboxAttachment {
            data: b"replacement must not win",
            ..image()
        }),
    )
    .await
    .unwrap();
    assert_eq!(persistent, duplicate, "retry must reuse the durable row");
    let ttl = enqueue_outbox_pg_on_tx_with_ttl_and_attachment(
        &mut tx,
        message("ttl"),
        300,
        Some(image()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        enqueue_outbox_pg_on_tx_with_ttl_and_attachment(
            &mut tx,
            message("ttl"),
            300,
            Some(image())
        )
        .await
        .unwrap()
        .is_none()
    );
    tx.commit().await.unwrap();
    let exact = enqueue_outbox_pg_with_exact_dedupe_and_attachment(
        &pool,
        message("exact"),
        "image-exact-key",
        None,
        Some(image()),
    )
    .await
    .unwrap();
    let again = enqueue_outbox_pg_with_exact_dedupe_and_attachment(
        &pool,
        message("changed exact rendering"),
        "image-exact-key",
        None,
        Some(image()),
    )
    .await
    .unwrap();
    assert_eq!(exact, again);
    let enqueue_racer = || async {
        let mut tx = pool.begin().await.unwrap();
        let id =
            enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("concurrent"), Some(image()))
                .await
                .unwrap();
        tx.commit().await.unwrap();
        id
    };
    let (left, right) = tokio::join!(enqueue_racer(), enqueue_racer());
    assert_eq!(
        left, right,
        "competing image producers must reuse the existing UNIQUE dedupe winner"
    );

    let staged =
        stage_outbox_pg_with_ttl_and_attachment(&pool, message("staged"), 300, Some(image()))
            .await
            .unwrap();
    assert!(activate_staged_outbox_pg(&pool, staged).await.unwrap());
    let mut tx = pool.begin().await.unwrap();
    enqueue_outbox_pg_on_tx_with_attachment(&mut tx, message("plain"), Some(image()))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut claimed = Vec::new();
    for _ in 0..6 {
        let batch = claim_pending_message_outbox_batch_pg(&pool, "before-restart").await;
        assert_eq!(batch.len(), 1, "image claim batch must contain one payload");
        claimed.extend(batch);
    }
    assert_eq!(claimed.len(), 6);
    for row in &claimed {
        let attachment = row.attachment.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(
            attachment.data,
            image().data,
            "claim must preserve every binary byte"
        );
        assert_eq!(attachment.filename, image().filename);
        assert_eq!(attachment.content_type, image().content_type);
        let cloned = row.clone();
        assert!(
            Arc::ptr_eq(
                attachment,
                cloned.attachment.as_ref().unwrap().as_ref().unwrap()
            ),
            "retry bookkeeping must share the upload buffer"
        );
    }
    let first = claimed.iter().find(|r| r.id == persistent).unwrap();
    let ids = first.delivery_ids();
    schedule_message_outbox_retry_pg(
        &pool,
        first.id,
        "lost response",
        1,
        0,
        &first.claim_owner,
        first.claimed_at,
    )
    .await
    .unwrap();
    let retry = claim_pending_message_outbox_batch_pg(&pool, "after-restart").await;
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].id, persistent);
    assert_eq!(retry[0].delivery_ids(), ids);
    assert_eq!(
        retry[0].attachment.as_ref().unwrap().as_ref().unwrap().data,
        image().data
    );
    let mut lease_tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('agentdesk.outbox_image_consumer_v1', 'enabled', true)")
        .execute(&mut *lease_tx)
        .await
        .unwrap();
    sqlx::query("UPDATE message_outbox SET claimed_at=NOW()-INTERVAL '1 day' WHERE id=$1")
        .bind(persistent)
        .execute(&mut *lease_tx)
        .await
        .unwrap();
    lease_tx.commit().await.unwrap();
    let reclaimed = claim_pending_message_outbox_batch_pg(&pool, "stale-restart").await;
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].delivery_ids(), ids);
    assert!(matches!(
        mark_message_outbox_sent_pg(
            &pool,
            persistent,
            &retry[0].claim_owner,
            retry[0].claimed_at
        )
        .await,
        Err(MessageOutboxLeaseUpdateError::StaleLeaseLost { .. })
    ));
    let row = &reclaimed[0];
    mark_message_outbox_failed_pg(
        &pool,
        row.id,
        "retry exhausted",
        6,
        &row.claim_owner,
        row.claimed_at,
    )
    .await
    .unwrap();
    crate::services::message_outbox_recovery::redrive_failed_rows(
        &pool,
        &[persistent],
        "image-redrive",
        "test durable image replay",
        false,
    )
    .await
    .unwrap();
    let redriven = claim_pending_message_outbox_batch_pg(&pool, "redriven-worker").await;
    assert_eq!(redriven.len(), 1);
    assert_eq!(redriven[0].id, persistent);
    assert_eq!(
        redriven[0]
            .attachment
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .data,
        image().data
    );
    assert_eq!(redriven[0].delivery_ids(), ids);
    assert_ne!(persistent, ttl);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn partial_outbox_image_never_invokes_text_delivery_pg() {
    let (db, pool) = image_pool().await;
    let mut tx = pool.begin().await.unwrap();
    let id = enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("corrupt"), Some(image()))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("ALTER TABLE message_outbox DROP CONSTRAINT message_outbox_attachment_complete")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE message_outbox SET attachment_content_type=NULL WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = drain_message_outbox_batch_once(&pool, Some("partial-row"), {
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { ("200 OK".into(), "text only".into()) }
        }
    })
    .await;
    assert_eq!(count, 1, "the corrupt row must actually be claimed");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "partial image must never downgrade to text-only delivery"
    );
    let (status, error): (String, String) =
        sqlx::query_as("SELECT status,error FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert!(error.contains("incomplete or invalid binary attachment"));
    assert!(!error.contains("preserved-binary"));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn legacy_enqueue_wrappers_keep_null_attachment_bytes_pg() {
    let (db, pool) = image_pool().await;
    enqueue_outbox_pg_returning_id(&pool, message("default"))
        .await
        .unwrap();
    enqueue_outbox_pg_returning_id_with_persistent_dedupe(&pool, message("persistent"))
        .await
        .unwrap();
    enqueue_outbox_pg_returning_outcome_with_exact_dedupe_and_cancel(
        &pool,
        message("exact"),
        "legacy-exact",
        None,
    )
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    enqueue_outbox_pg_on_tx_with_ttl(&mut tx, message("tx-ttl"), 0)
        .await
        .unwrap();
    enqueue_outbox_pg_on_tx(&mut tx, message("tx-plain"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    stage_outbox_pg_with_ttl(&pool, message("legacy-stage"), 300)
        .await
        .unwrap();
    let rows: Vec<LegacyTextRow> = sqlx::query_as("SELECT target,content,bot,source,attachment_filename,attachment_content_type,attachment_data FROM message_outbox ORDER BY id").fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 6, "all compatibility wrappers must execute");
    for row in rows {
        assert_eq!(
            (&*row.0, &*row.1, &*row.2, &*row.3),
            (
                message("x").target,
                message("x").content,
                message("x").bot,
                message("x").source
            )
        );
        assert_eq!(
            (row.4, row.5, row.6),
            (None, None, None),
            "legacy enqueue must forward None and preserve text-only bytes"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn outbox_image_constraints_reject_partial_and_oversized_rows_pg() {
    let (db, pool) = image_pool().await;
    let id = enqueue_outbox_pg_returning_id_with_persistent_dedupe(&pool, message("constraints"))
        .await
        .unwrap();
    for mask in 1..7 {
        let error = sqlx::query("UPDATE message_outbox SET attachment_filename=$1,attachment_content_type=$2,attachment_data=$3 WHERE id=$4")
            .bind((mask&1!=0).then_some(image().filename)).bind((mask&2!=0).then_some(image().content_type)).bind((mask&4!=0).then_some(image().data)).bind(id)
            .execute(&pool).await.expect_err("partial outbox triplet must be rejected");
        assert_eq!(
            error.as_database_error().and_then(|e| e.constraint()),
            Some("message_outbox_attachment_complete")
        );
        let displayed = OutboxEnqueueError::from(error).to_string();
        assert!(
            !displayed.contains("Failing row") && !displayed.contains("89504e47"),
            "Display errors must redact PG DETAIL bytea prefixes"
        );
    }
    for size in [
        0,
        1,
        crate::db::scheduled_messages::MAX_IMAGE_BYTES,
        crate::db::scheduled_messages::MAX_IMAGE_BYTES + 1,
    ] {
        let result = sqlx::query("UPDATE message_outbox SET attachment_filename='image.png',attachment_content_type='image/png',attachment_data=$1 WHERE id=$2").bind(vec![42_u8;size]).bind(id).execute(&pool).await;
        assert_eq!(
            result.is_ok(),
            (1..=crate::db::scheduled_messages::MAX_IMAGE_BYTES).contains(&size),
            "outbox byte limit mismatch at {size}"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn image_prod_options_and_sent_cleanup_preserve_sentinel_pg() {
    let (db, pool) = image_pool().await;
    let mut tx = pool.begin().await.unwrap();
    let id =
        enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("sent-cleanup"), Some(image()))
            .await
            .unwrap();
    tx.commit().await.unwrap();
    let claimed = claim_pending_message_outbox_batch_pg(&pool, "image-options").await;
    assert_eq!(claimed.len(), 1);
    let row = &claimed[0];
    let expected = row.attachment.as_ref().unwrap().as_ref().unwrap();
    let options = outbox_actionable_delivery::delivery_options(row).unwrap();
    assert!(
        options
            .image_attachment
            .as_ref()
            .is_some_and(|image| Arc::ptr_eq(image, expected)),
        "prod delivery options must preserve the claimed image Arc"
    );
    assert!(options.trusted_internal_outbox);
    assert!(
        mark_message_outbox_sent_pg(&pool, id, "stale-owner", row.claimed_at)
            .await
            .is_err()
    );
    let before: bool =
        sqlx::query_scalar("SELECT attachment_data IS NOT NULL FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(before, "stale lease must retain image bytes");
    mark_message_outbox_sent_pg(&pool, id, &row.claim_owner, row.claimed_at)
        .await
        .unwrap();
    let cleared: bool = sqlx::query_scalar("SELECT status='sent' AND attachment_filename IS NULL AND attachment_content_type IS NULL AND attachment_data IS NULL AND dedupe_key IS NOT NULL AND dedupe_expires_at IS NULL FROM message_outbox WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert!(
        cleared,
        "sent image must clear its triplet and retain the permanent dedupe sentinel"
    );
    let repeated =
        enqueue_outbox_pg_returning_id_with_persistent_dedupe(&pool, message("sent-cleanup"))
            .await
            .unwrap();
    assert_eq!(id, repeated);
    let mut tx = pool.begin().await.unwrap();
    let failed_id =
        enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("failed-preserve"), Some(image()))
            .await
            .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("UPDATE message_outbox SET retry_count=100 WHERE id=$1")
        .bind(failed_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        drain_message_outbox_batch_once(&pool, Some("failed-image"), |_| async {
            ("500 Internal Server Error".into(), "retry exhausted".into())
        })
        .await,
        1
    );
    let retained: bool = sqlx::query_scalar(
        "SELECT status='failed' AND attachment_data=$2 FROM message_outbox WHERE id=$1",
    )
    .bind(failed_id)
    .bind(image().data)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(retained, "failed image must keep bytes for redrive");
    pool.close().await;
    db.drop().await;
}

/// Enqueues one row per kind (`i` image, `t` text) in id order.
async fn enqueue_kinds(pool: &PgPool, prefix: &str, kinds: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    for (index, kind) in kinds.chars().enumerate() {
        let reason = format!("{prefix}-{index}");
        let mut tx = pool.begin().await.unwrap();
        let attachment = (kind == 'i').then(image);
        ids.push(
            enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message(&reason), attachment)
                .await
                .unwrap(),
        );
        tx.commit().await.unwrap();
    }
    ids
}

async fn claim_ids(pool: &PgPool) -> Vec<i64> {
    claim_pending_message_outbox_batch_pg(pool, "order-owner")
        .await
        .iter()
        .map(|row| row.id)
        .collect()
}

#[tokio::test]
async fn claim_batches_are_id_prefixes_ending_before_second_image_pg() {
    let (db, pool) = image_pool().await;
    let ids = enqueue_kinds(&pool, "iit", "iit").await;
    assert_eq!(
        claim_ids(&pool).await,
        ids[..1],
        "text must not overtake img2"
    );
    assert_eq!(claim_ids(&pool).await, ids[1..]);
    let ids = enqueue_kinds(&pool, "titit", "titit").await;
    assert_eq!(claim_ids(&pool).await, ids[..3]);
    assert_eq!(claim_ids(&pool).await, ids[3..]);
    assert!(claim_ids(&pool).await.is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn claim_text_only_and_single_image_batches_keep_existing_shape_pg() {
    let (db, pool) = image_pool().await;
    for kinds in ["tit", "itt", "tti", "ttt"] {
        let ids = enqueue_kinds(&pool, kinds, kinds).await;
        assert_eq!(
            claim_ids(&pool).await,
            ids,
            "{kinds} claims every candidate"
        );
    }
    let ids = enqueue_kinds(&pool, "text-only", &"t".repeat(12)).await;
    assert_eq!(claim_ids(&pool).await, ids[..10]);
    assert_eq!(claim_ids(&pool).await, ids[10..]);
    assert!(claim_ids(&pool).await.is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn stale_processing_reclaim_follows_the_same_prefix_rule_pg() {
    let (db, pool) = image_pool().await;
    let ids = enqueue_kinds(&pool, "stale", "iit").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('agentdesk.outbox_image_consumer_v1', 'enabled', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE message_outbox SET status='processing', claim_owner='dead', claimed_at=NOW() - INTERVAL '1 day' WHERE id = ANY($1)")
        .bind(vec![ids[0], ids[2]])
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(claim_ids(&pool).await, ids[..1]);
    assert_eq!(claim_ids(&pool).await, ids[1..]);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn image_claim_batch_budget_and_old_consumer_barrier_pg() {
    let (db, pool) = image_pool().await;
    let mut tx = pool.begin().await.unwrap();
    let first =
        enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("budget-first"), Some(image()))
            .await
            .unwrap();
    enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("budget-second"), Some(image()))
        .await
        .unwrap();
    enqueue_outbox_pg_on_tx(&mut tx, message("budget-text-one"))
        .await
        .unwrap();
    enqueue_outbox_pg_on_tx(&mut tx, message("budget-text-two"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let error = sqlx::query("UPDATE message_outbox SET status='processing', claim_owner='old', claimed_at=NOW() WHERE id=$1").bind(first).execute(&pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55000"),
        "old outbox consumer must never claim an image as text"
    );
    let batch = claim_pending_message_outbox_batch_pg(&pool, "budget-new").await;
    assert_eq!(batch.len(), 1, "a second image ends the batch prefix");
    assert_eq!(batch[0].id, first);
    let second = claim_pending_message_outbox_batch_pg(&pool, "budget-next").await;
    assert_eq!(second.len(), 3);
    assert_eq!(
        second
            .iter()
            .filter(|row| row.attachment.as_ref().unwrap().is_some())
            .count(),
        1
    );
    let row = batch.iter().find(|r| r.id == first).unwrap();
    let error = sqlx::query(
        "UPDATE message_outbox SET claim_owner='old-reclaim', claimed_at=NOW() WHERE id=$1",
    )
    .bind(row.id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55000")
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn image_preflight_400_fails_first_attempt_pg() {
    let (db, pool) = image_pool().await;
    let mut tx = pool.begin().await.unwrap();
    let id =
        enqueue_outbox_pg_with_attachment_on_tx(&mut tx, message("permanent-error"), Some(image()))
            .await
            .unwrap();
    tx.commit().await.unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    assert_eq!(
        drain_message_outbox_batch_once(&pool, Some("invalid-preflight"), move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { ("400 Bad Request".into(), "invalid binary content".into()) }
        })
        .await,
        1
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let result: (String, i64, bool) = sqlx::query_as(
        "SELECT status,retry_count,attachment_data IS NOT NULL FROM message_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        result,
        ("failed".into(), 0, true),
        "image 400 must fail on the first attempt and preserve bytes"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn image_api_fire_outbox_authorized_wire_and_sent_cleanup_pg() {
    use axum::{Json, extract::State};
    use base64::Engine as _;
    use serde_json::json;
    let (db, pool) = image_pool().await;
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    let state = crate::server::routes::AppState {
        pg_pool: Some(pool.clone()),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    };
    let body=serde_json::from_value(json!({"content":"body", "targetChannelId":"7001", "scheduledAt":(chrono::Utc::now()-chrono::Duration::seconds(1)).to_rfc3339(),
        "imageAttachment":{"filename":"image.png","contentType":"image/png","dataBase64":base64::engine::general_purpose::STANDARD.encode(image().data)}
    })).unwrap();
    let (status, Json(response)) =
        crate::server::routes::scheduled_messages::create_scheduled_message(
            State(state),
            Ok(Json(body)),
        )
        .await
        .unwrap();
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let id = response["scheduledMessage"]["id"].as_str().unwrap();
    let mut fires = crate::db::scheduled_messages::claim_due_fires_pg(
        &pool,
        "image-e2e",
        true,
        10,
        300,
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(fires.len(), 1);
    crate::services::scheduled_messages::fire_claimed(
        &pool,
        None,
        fires.pop().unwrap(),
        chrono::Utc::now(),
    )
    .await;
    let parent: String = sqlx::query_scalar("SELECT status FROM scheduled_messages WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(parent, "sent");
    let claimed = claim_pending_message_outbox_batch_pg(&pool, "image-e2e-outbox").await;
    assert_eq!(claimed.len(), 1);
    let row = &claimed[0];
    let options = outbox_actionable_delivery::delivery_options(row).unwrap();
    let (correlation, semantic) = row.delivery_ids();
    let (status, _, payloads) =
        crate::services::discord::outbound::send_gate::deliver_options_on_wire(
            options,
            &row.content,
            &correlation,
            &semantic,
        )
        .await;
    assert_eq!(status, "200 OK");
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].0["enforce_nonce"], true);
    assert!(
        payloads[0]
            .1
            .windows(image().data.len())
            .any(|part| part == image().data),
        "API image must reach the authorized binary upload unchanged"
    );
    mark_message_outbox_sent_pg(&pool, row.id, &row.claim_owner, row.claimed_at)
        .await
        .unwrap();
    let cleared:bool=sqlx::query_scalar("SELECT status='sent' AND attachment_data IS NULL AND dedupe_key IS NOT NULL FROM message_outbox WHERE id=$1").bind(row.id).fetch_one(&pool).await.unwrap();
    assert!(cleared);
    pool.close().await;
    db.drop().await;
}
