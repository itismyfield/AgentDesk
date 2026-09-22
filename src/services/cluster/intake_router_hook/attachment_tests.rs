use super::pg_tests::{
    ctx_for_channel, seed_agent_with_preference, seed_worker_node_with_capabilities,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::*;
use crate::services::cluster::attachment_transfer::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_route_requires_consumer_and_preserves_bytes_through_retry_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed_agent_with_preference(&pool, "attachment-agent", "8220", json!(["worker"])).await;
    let caps = json!({"intake_worker":{"enabled":true,"providers":["claude"],"features":["preserve_on_cancel_v1"]}});
    seed_worker_node_with_capabilities(&pool, "win", json!(["worker"]), "online", caps.clone())
        .await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "8220");
    ctx.user_msg_id = "8221";
    ctx.node_override_instance_id = Some("win");
    let identity = AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: "8220".into(),
        user_msg_id: "8221".into(),
    };
    let bundle = validate_attachment_bundle_v1(
        AttachmentBundleV1 {
            version: 1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![AttachmentEntryV1 {
                filename: "image.png".into(),
                bytes: b"png fixture".to_vec(),
                sha256: attachment_sha256_hex(b"png fixture"),
            }],
        },
        &identity,
    )
    .unwrap();
    let refs = vec![store::put(&pool, &bundle).await.unwrap()];
    ctx.attachment_refs = &refs;
    assert!(
        matches!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Blocked { .. }
        ),
        "a type-only legacy worker must not receive files"
    );
    let mut caps = caps;
    caps["intake_worker"]["features"]
        .as_array_mut()
        .unwrap()
        .push(json!(CAPABILITY));
    sqlx::query("UPDATE worker_nodes SET capabilities=$1 WHERE instance_id='win'")
        .bind(caps)
        .execute(&pool)
        .await
        .unwrap();
    let id = match try_route_intake(&pool, &ctx).await {
        IntakeRouterDecision::Forwarded { outbox_id, .. } => outbox_id,
        other => panic!("unexpected admission: {other:?}"),
    };
    let row = claim_pending_for_target(&pool, "win", "claude", "test-owner")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.attachment_refs, json!(refs));
    let uploads = worker_uploads(&pool, &row).await.unwrap();
    assert_eq!(
        materialize::prepare(&uploads, Some(&pool))
            .await
            .unwrap()
            .records
            .len(),
        1
    );
    assert!(
        mark_failed_pre_accept(&pool, id, "test-owner", "transient test failure")
            .await
            .unwrap()
    );
    let retry = sweep_failed_pre_accept_once(&pool, "leader-1", 4, 60, None)
        .await
        .unwrap();
    assert!(matches!(retry, FailedPreAcceptSweepOutcome::Retried { .. }));
    let child = claim_pending_for_target(&pool, "win", "claude", "retry-owner")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.attachment_refs, row.attachment_refs);
    assert!(mark_accepted(&pool, child.id, "retry-owner").await.unwrap());
    assert!(mark_spawned(&pool, child.id, "retry-owner").await.unwrap());
    let manual = crate::db::intake_outbox_force_fail::force_fail_and_retry_as_new(
        &pool,
        child.id,
        "manual retry",
    )
    .await
    .unwrap();
    let _ = manual;
    let operator_retry = claim_pending_for_target(&pool, "win", "claude", "operator-owner")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operator_retry.attachment_refs, row.attachment_refs);
    sqlx::query("UPDATE intake_attachment_bundles SET expires_at=NOW()-INTERVAL '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        worker_uploads(&pool, &operator_retry).await.is_err(),
        "expiry never turns an attachment request into text only"
    );
    pool.close().await;
    fixture.drop().await;
}
