use super::*;
use serde_json::json;

#[tokio::test]
async fn postgres_image_attachment_partial_triplets_are_rejected() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_image_partial",
        "scheduled image partial triplets",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    sqlx::query("INSERT INTO scheduled_messages (id, content, target_channel_id, scheduled_at) VALUES ('partial-update', 'unchanged text', '123456789', NOW())")
        .execute(&pool).await.expect("seed text row for direct SQL updates");
    for mask in 1..7 {
        let error = sqlx::query(
            "INSERT INTO scheduled_messages
             (id, content, target_channel_id, scheduled_at,
              image_filename, image_content_type, image_data)
             VALUES ($1, 'unchanged text', '123456789', NOW(), $2, $3, $4)",
        )
        .bind(format!("partial_{mask}"))
        .bind((mask & 1 != 0).then_some("picture.png"))
        .bind((mask & 2 != 0).then_some("image/png"))
        .bind((mask & 4 != 0).then_some(&b"image bytes"[..]))
        .execute(&pool)
        .await
        .expect_err("partial triplet must be rejected");
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("23514")
        );
        assert_eq!(
            error.as_database_error().and_then(|e| e.constraint()),
            Some("chk_smsg_image_attachment_complete")
        );
        let error = sqlx::query("UPDATE scheduled_messages SET image_filename = $1, image_content_type = $2, image_data = $3 WHERE id = 'partial-update'")
            .bind((mask & 1 != 0).then_some("picture.png"))
            .bind((mask & 2 != 0).then_some("image/png"))
            .bind((mask & 4 != 0).then_some(&b"image bytes"[..]))
            .execute(&pool).await.expect_err("direct SQL update must reject partial triplet");
        assert_eq!(
            error.as_database_error().and_then(|e| e.constraint()),
            Some("chk_smsg_image_attachment_complete")
        );
    }
    pool.close().await;
    pg_db.drop().await;
}

#[test]
fn image_create_json_preserves_optional_contract() {
    for value in [
        json!(null),
        json!({}),
        json!({"filename":"picture.png","data":"AAAA"}),
    ] {
        let parsed = serde_json::from_value::<crate::server::routes::scheduled_messages::CreateScheduledMessageBody>(json!({
            "content":"unchanged text", "scheduledAt":"2030-01-01T00:00:00Z", "imageAttachment":value
        })).expect("image parsing defers validation to the API");
        assert_eq!(parsed.image_attachment, (!value.is_null()).then_some(value));
    }
    serde_json::from_value::<
        crate::server::routes::scheduled_messages::CreateScheduledMessageBody,
    >(json!({
        "content": "unchanged text", "scheduledAt": "2030-01-01T00:00:00Z",
        "unrelatedFutureField": true
    }))
    .expect("text-only create JSON keeps accepting an absent image and unrelated unknown fields");
}

async fn image_test_pool(
    prefix: &str,
) -> (
    crate::dispatch::test_support::DispatchPostgresTestDb,
    PgPool,
) {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        prefix,
        "scheduled image storage",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    (db, pool)
}

fn new_image_message(data: Vec<u8>) -> NewScheduledMessage {
    NewScheduledMessage {
        content: "unchanged scheduled text".into(),
        discord_mention_user_ids: Vec::new(),
        title: None,
        target_channel_id: Some("123456789".into()),
        bot: "notify".into(),
        delivery_kind: KIND_PUSH.into(),
        agent_id: None,
        agent_instruction: None,
        on_agent_failure: "fail".into(),
        scheduled_at: Utc::now(),
        schedule: None,
        timezone: "UTC".into(),
        expires_at: None,
        source: "postgres_test".into(),
        created_by: None,
        dedupe_key: None,
        provider_targets: None,
        provider_target_summary: None,
        context_strategy: "fresh".into(),
        context_snapshot_id: None,
        on_context_failure: "fail".into(),
        image_attachment: Some(ScheduledMessageImageAttachment {
            filename: "picture.png".into(),
            content_type: "image/png".into(),
            data,
        }),
    }
}

#[tokio::test]
async fn postgres_image_attachment_storage_insert_patch_preserve_and_clear() {
    let (pg_db, pool) = image_test_pool("agentdesk_smsg_image_writes").await;
    let new = new_image_message(b"\x89PNG\r\n\x1a\n\x00\xfforiginal bytes".to_vec());
    let mut tx = pool.begin().await.expect("begin");
    let row = insert_scheduled_message_tx(&mut tx, &new)
        .await
        .expect("transactional insert");
    tx.commit().await.expect("commit");
    let row = assert_metadata_then_load(&pool, row).await;
    assert_eq!(
        row.image_attachment
            .as_ref()
            .and_then(|image| image.data.as_ref()),
        new.image_attachment.as_ref().map(|image| &image.data)
    );
    let row = update_scheduled_message_pg(
        &pool,
        &row.id,
        &ScheduledMessagePatch {
            content: Some("edited text".into()),
            ..Default::default()
        },
    )
    .await
    .expect("text patch")
    .expect("editable");
    let row = assert_metadata_then_load(&pool, row).await;
    assert_eq!(
        row.image_attachment
            .as_ref()
            .and_then(|image| image.data.as_ref()),
        new.image_attachment.as_ref().map(|image| &image.data)
    );
    let replacement = ScheduledMessageImageAttachment {
        filename: "next.gif".into(),
        content_type: "image/gif".into(),
        data: b"GIF89a\0replacement".to_vec(),
    };
    let row = update_scheduled_message_pg(
        &pool,
        &row.id,
        &ScheduledMessagePatch {
            image_attachment: Some(Some(replacement.clone())),
            ..Default::default()
        },
    )
    .await
    .expect("replace triplet")
    .expect("editable");
    let row = assert_metadata_then_load(&pool, row).await;
    let image = row.image_attachment.as_ref().expect("replaced image");
    assert_eq!(
        (&image.filename, &image.content_type, image.data.as_ref()),
        (
            &replacement.filename,
            &replacement.content_type,
            Some(&replacement.data)
        )
    );
    let row = update_scheduled_message_pg(
        &pool,
        &row.id,
        &ScheduledMessagePatch {
            image_attachment: Some(None),
            ..Default::default()
        },
    )
    .await
    .expect("clear triplet")
    .expect("editable");
    assert!(row.image_attachment.is_none());
    let all_null: bool = sqlx::query_scalar("SELECT image_filename IS NULL AND image_content_type IS NULL AND image_data IS NULL FROM scheduled_messages WHERE id = $1")
        .bind(&row.id).fetch_one(&pool).await.expect("stored NULL triplet");
    assert!(all_null);
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn postgres_image_attachment_limits_cover_sql_insert_patch_and_copy() {
    let (pg_db, pool) = image_test_pool("agentdesk_smsg_image_limits").await;
    for size in [1, 8_388_608] {
        let row = insert_scheduled_message_pg(&pool, &new_image_message(vec![0x89; size]))
            .await
            .expect("inclusive size boundary");
        assert_eq!(
            row.image_attachment.as_ref().expect("image").size_bytes,
            size as i64
        );
    }
    let row = insert_scheduled_message_pg(&pool, &new_image_message(vec![1]))
        .await
        .expect("seed");
    for size in [0, 8_388_609] {
        let new = new_image_message(vec![0x89; size]);
        let error = insert_scheduled_message_pg(&pool, &new)
            .await
            .expect_err("invalid size insert");
        assert_eq!(
            error.as_database_error().and_then(|e| e.constraint()),
            Some("chk_smsg_image_attachment_size")
        );
        let error = update_scheduled_message_pg(
            &pool,
            &row.id,
            &ScheduledMessagePatch {
                image_attachment: Some(new.image_attachment),
                ..Default::default()
            },
        )
        .await
        .expect_err("invalid size patch");
        assert_eq!(
            error.as_database_error().and_then(|e| e.constraint()),
            Some("chk_smsg_image_attachment_size")
        );
        let error = sqlx::query("UPDATE scheduled_messages SET image_data = $2 WHERE id = $1")
            .bind(&row.id)
            .bind(vec![0x89u8; size])
            .execute(&pool)
            .await
            .expect_err("direct SQL size guard");
        assert_eq!(
            error.as_database_error().and_then(|e| e.constraint()),
            Some("chk_smsg_image_attachment_size")
        );
    }
    let error = sqlx::query("INSERT INTO scheduled_messages (id, content, target_channel_id, scheduled_at, image_filename, image_content_type, image_data) SELECT id || '_copy', content, target_channel_id, scheduled_at, image_filename, NULL, image_data FROM scheduled_messages WHERE id = $1")
        .bind(&row.id).execute(&pool).await.expect_err("copy cannot create partial triplet");
    assert_eq!(
        error.as_database_error().and_then(|e| e.constraint()),
        Some("chk_smsg_image_attachment_complete")
    );
    sqlx::query("INSERT INTO agents (id, name) VALUES ('image-agent', 'Image Agent')")
        .execute(&pool)
        .await
        .expect("seed agent");
    let error = sqlx::query("UPDATE scheduled_messages SET delivery_kind = 'agent', agent_id = 'image-agent' WHERE id = $1")
        .bind(&row.id).execute(&pool).await.expect_err("agent image forbidden");
    assert_eq!(
        error.as_database_error().and_then(|e| e.constraint()),
        Some("chk_smsg_image_attachment_push_only")
    );
    let unchanged = get_scheduled_message_pg(&pool, &row.id)
        .await
        .expect("reload")
        .expect("row");
    assert_eq!(
        unchanged.to_api_json()["imageAttachment"],
        row.to_api_json()["imageAttachment"]
    );
    assert_eq!(
        unchanged
            .image_attachment
            .as_ref()
            .unwrap()
            .require_loaded()
            .unwrap(),
        &[1]
    );
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn postgres_image_attachment_list_redacts_payload_and_renders_metadata() {
    let (pg_db, pool) = image_test_pool("agentdesk_smsg_image_list").await;
    let row = insert_scheduled_message_pg(
        &pool,
        &new_image_message(b"private binary payload\0\xff".to_vec()),
    )
    .await
    .expect("seed image");
    let rows = list_scheduled_messages_pg(
        &pool,
        &ListFilters {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .expect("list");
    assert_eq!(rows.len(), 1);
    let image = rows[0].image_attachment.as_ref().expect("list metadata");
    assert!(
        image.data.is_none(),
        "list query must not select attachment bytes"
    );
    assert_eq!(
        rows[0].to_api_json()["imageAttachment"],
        json!({
            "filename":"picture.png", "contentType":"image/png", "sizeBytes":24
        })
    );
    assert_eq!(
        rows[0].to_api_json(),
        row.to_api_json(),
        "detail and list JSON both redact bytes"
    );
    let json = row.to_api_json().to_string();
    let debug = format!(
        "{row:?} {:?} {:?}",
        row.image_attachment,
        new_image_message(b"private binary payload\0\xff".to_vec())
    );
    assert!(!json.contains("private binary payload"));
    assert!(!debug.contains("private binary payload"));
    assert!(
        !debug.contains("112, 114, 105, 118"),
        "Debug must not emit byte vectors"
    );
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn postgres_image_attachment_detail_loads_exact_bytes() {
    let (pg_db, pool) = image_test_pool("agentdesk_smsg_image_detail").await;
    let new = new_image_message(b"\x89PNG\r\n\x1a\n\0\xffdurable\0bytes".to_vec());
    let row = insert_scheduled_message_pg(&pool, &new)
        .await
        .expect("insert image");
    let detail = get_scheduled_message_pg(&pool, &row.id)
        .await
        .expect("detail query")
        .expect("detail row");
    assert_eq!(
        detail
            .image_attachment
            .as_ref()
            .and_then(|image| image.data.as_ref()),
        new.image_attachment.as_ref().map(|image| &image.data),
        "detail must load original bytes"
    );
    assert!(
        row.image_attachment.as_ref().unwrap().data.is_none(),
        "insert RETURNING is metadata only"
    );
    assert_eq!(
        detail.to_api_json()["imageAttachment"],
        row.to_api_json()["imageAttachment"]
    );
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn postgres_image_attachment_legacy_text_rows_keep_api_shape() {
    let (pg_db, pool) = image_test_pool("agentdesk_smsg_image_legacy").await;
    sqlx::query("INSERT INTO scheduled_messages (id, content, target_channel_id, scheduled_at) VALUES ('legacy-text', 'unchanged text', '123456789', NOW())")
        .execute(&pool).await.expect("old writer omits attachment columns");
    let detail = get_scheduled_message_pg(&pool, "legacy-text")
        .await
        .expect("detail")
        .expect("old row");
    assert!(detail.image_attachment.is_none());
    let json = detail.to_api_json();
    let mut expected_keys = vec![
        "id",
        "content",
        "discordMentionUserIds",
        "title",
        "targetChannelId",
        "bot",
        "deliveryKind",
        "agentId",
        "agentInstruction",
        "onAgentFailure",
        "scheduledAt",
        "schedule",
        "timezone",
        "expiresAt",
        "status",
        "inFlightDeliveryId",
        "fireCount",
        "lastFiredAt",
        "lastError",
        "source",
        "createdBy",
        "dedupeKey",
        "providerTargets",
        "contextStrategy",
        "contextSnapshotId",
        "onContextFailure",
        "createdAt",
        "updatedAt",
    ];
    expected_keys.sort_unstable();
    assert_eq!(
        json.as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        expected_keys
    );
    let rows = list_scheduled_messages_pg(
        &pool,
        &ListFilters {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .expect("list");
    assert_eq!(
        serde_json::to_vec(&rows[0].to_api_json()).expect("list JSON bytes"),
        serde_json::to_vec(&json).expect("detail JSON bytes")
    );
    let claims = claim_due_fires_pg(&pool, "text-worker", true, 1, 60, Utc::now())
        .await
        .expect("legacy claim");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].message.content, "unchanged text");
    assert!(claims[0].message.image_attachment.is_none());
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn postgres_image_decoder_rejects_corrupt_metadata_projection() {
    let (db, pool) = image_test_pool("agentdesk_image_projection").await;
    let row = insert_scheduled_message_pg(&pool, &new_image_message(vec![1, 2, 3]))
        .await
        .unwrap();
    for projection in [
        "'image.png'::text AS image_filename, NULL::text AS image_content_type, NULL::bytea AS image_data, NULL::bigint AS image_size_bytes",
        "'image.png'::text AS image_filename, 'image/png'::text AS image_content_type, NULL::bytea AS image_data, 0::bigint AS image_size_bytes",
        "'image.png'::text AS image_filename, 'image/png'::text AS image_content_type, NULL::bytea AS image_data, 8388609::bigint AS image_size_bytes",
        "'image.png'::text AS image_filename, 'image/png'::text AS image_content_type, decode('0001','hex') AS image_data, 1::bigint AS image_size_bytes",
    ] {
        let sql = format!(
            "SELECT {DEFINITION_COLUMNS}, {projection} FROM scheduled_messages WHERE id=$1"
        );
        let result = sqlx::query_as::<_, ScheduledMessageRow>(AssertSqlSafe(sql))
            .bind(&row.id)
            .fetch_one(&pool)
            .await;
        assert!(result.is_err(), "corrupt image projection must fail closed");
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_image_list_cannot_enter_attachment_handoff() {
    let (db, pool) = image_test_pool("agentdesk_image_loaded").await;
    let row = insert_scheduled_message_pg(&pool, &new_image_message(vec![0, 255, 42]))
        .await
        .unwrap();
    let listed = list_scheduled_messages_pg(&pool, &ListFilters::default())
        .await
        .unwrap();
    let image = listed
        .iter()
        .find(|r| r.id == row.id)
        .unwrap()
        .image_attachment
        .as_ref()
        .unwrap();
    let result = crate::services::message_outbox::OutboxAttachment::try_from(image);
    assert!(
        result.is_err(),
        "metadata-only list row must not enter binary handoff"
    );
    let row = assert_metadata_then_load(&pool, row).await;
    let image = row.image_attachment.as_ref().unwrap();
    let loaded = crate::services::message_outbox::OutboxAttachment::try_from(image).unwrap();
    assert_eq!(loaded.data, &[0, 255, 42]);
    pool.close().await;
    db.drop().await;
}

#[test]
fn image_byte_limit_matches_immutable_definition_and_outbox_migrations() {
    let check = format!("BETWEEN 1 AND {}", MAX_IMAGE_BYTES);
    for migration in [
        include_str!("../../../migrations/postgres/0143_scheduled_message_image_attachments.sql"),
        include_str!("../../../migrations/postgres/0144_message_outbox_image_attachments.sql"),
    ] {
        assert!(
            migration.contains(&check),
            "immutable image checks must share MAX_IMAGE_BYTES"
        );
    }
}

async fn assert_metadata_then_load(pool: &PgPool, row: ScheduledMessageRow) -> ScheduledMessageRow {
    assert!(
        row.image_attachment.as_ref().unwrap().data.is_none(),
        "write RETURNING must not load image bytes"
    );
    get_scheduled_message_pg(pool, &row.id)
        .await
        .unwrap()
        .unwrap()
}
