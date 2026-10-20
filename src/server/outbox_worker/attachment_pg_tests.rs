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
    let claimed = claim_pending_message_outbox_batch_pg(&pool, "before-restart").await;
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
    sqlx::query("UPDATE message_outbox SET claimed_at=NOW()-INTERVAL '1 day' WHERE id=$1")
        .bind(persistent)
        .execute(&pool)
        .await
        .unwrap();
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
