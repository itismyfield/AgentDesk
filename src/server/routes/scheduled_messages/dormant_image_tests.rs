use super::super::*;
use serde_json::json;

#[tokio::test]
async fn postgres_image_attachment_patch_input_is_dormant() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_image_api",
        "scheduled image PATCH stays dormant",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    sqlx::query("INSERT INTO scheduled_messages (id, content, target_channel_id, scheduled_at) VALUES ('dormant-image', 'unchanged text', '123456789', NOW())")
        .execute(&pool).await.expect("seed legacy text row");
    let row = db::get_scheduled_message_pg(&pool, "dormant-image")
        .await
        .expect("load")
        .expect("row");
    for value in [
        json!(null),
        json!({}),
        json!({"filename":"picture.png","data":"AAAA"}),
    ] {
        let body = json!({"content":"edited text", "imageAttachment":value});
        let error = build_patch(&pool, body.as_object().expect("object"), &row, &[])
            .await
            .expect_err("image PATCH must fail");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error.to_json_value()["error"],
            "imageAttachment is not enabled"
        );
    }
    let stored = db::get_scheduled_message_pg(&pool, &row.id)
        .await
        .expect("reload")
        .expect("row");
    assert_eq!(stored.to_api_json(), row.to_api_json());
    let body = json!({"content":"edited text", "unrelatedFutureField":true});
    let patch = build_patch(&pool, body.as_object().expect("object"), &row, &[])
        .await
        .expect("ordinary text patch remains accepted");
    assert_eq!(patch.content.as_deref(), Some("edited text"));
    assert!(patch.image_attachment.is_none());
    pool.close().await;
    pg_db.drop().await;
}
