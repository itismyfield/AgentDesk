use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{InsertPendingPayload, insert_pending};

fn payload(node: &str, channel: &str) -> InsertPendingPayload {
    InsertPendingPayload {
        execution_requirements: json!({}),
        attachment_refs: json!([]),
        target_instance_id: node.into(),
        forwarded_by_instance_id: "leader".into(),
        provider: "claude".into(),
        required_labels: json!([]),
        channel_id: channel.into(),
        user_msg_id: format!("message-{channel}"),
        request_owner_id: "user".into(),
        request_owner_name: None,
        user_text: "test".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "agent".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execution_capacity_atomic_reservations_execution_and_fenced_expiry_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    for node in ["mac", "windows"] {
        sqlx::query("INSERT INTO worker_nodes(instance_id,last_heartbeat_at,capabilities) VALUES($1,NOW(),$2)")
            .bind(node).bind(json!({"execution_capacity":{"version":1,"slots":2}})).execute(&pool).await.unwrap();
    }
    let mut jobs = tokio::task::JoinSet::new();
    for n in 0..12 {
        let pool = pool.clone();
        jobs.spawn(async move {
            let channel = format!("{n}");
            let result = insert_pending(&pool, &payload("mac", &channel), 1, None).await;
            (channel, result)
        });
    }
    let mut admitted = Vec::new();
    while let Some(result) = jobs.join_next().await {
        let (channel, result) = result.unwrap();
        match result {
            Ok(_) => admitted.push(channel),
            Err(error) => assert!(is_exhausted(&error), "{error}"),
        }
    }
    assert_eq!(
        admitted.len(),
        2,
        "concurrent producers must share the same budget"
    );
    let channel = &admitted[0];
    let first = uuid::Uuid::new_v4();
    store::acquire(&pool, "mac", "claude", channel, first)
        .await
        .unwrap();
    let nodes = super::super::node_registry::list_worker_nodes(&pool, 30)
        .await
        .unwrap();
    let mac = nodes.iter().find(|n| n["instance_id"] == "mac").unwrap();
    assert_eq!(
        mac["execution_occupied"],
        json!(2),
        "delivery and execution must not double count"
    );
    assert_eq!(mac["execution_active"], json!(1));
    assert!(is_exhausted(
        &store::acquire(&pool, "mac", "claude", "overflow", uuid::Uuid::new_v4())
            .await
            .unwrap_err()
    ));
    let other = uuid::Uuid::new_v4();
    store::acquire(&pool, "windows", "claude", "win-turn", other)
        .await
        .unwrap();
    sqlx::query("DELETE FROM intake_outbox WHERE target_instance_id='mac'")
        .execute(&pool)
        .await
        .unwrap();
    // Delivery finished, but the provider is still running and occupies a slot.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM node_execution_occupancy('mac')")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert!(
        store::renew(&pool, "mac", "claude", channel, first)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE node_execution_leases SET expires_at=NOW()-INTERVAL '1 second' WHERE instance_id='mac'").execute(&pool).await.unwrap();
    assert!(
        !store::renew(&pool, "mac", "claude", channel, first)
            .await
            .unwrap()
    );
    let successor = uuid::Uuid::new_v4();
    store::acquire(&pool, "mac", "claude", channel, successor)
        .await
        .unwrap();
    store::release(&pool, "mac", "claude", channel, first)
        .await
        .unwrap();
    assert!(
        store::renew(&pool, "mac", "claude", channel, successor)
            .await
            .unwrap(),
        "old cleanup cannot release its successor"
    );
    store::release(&pool, "mac", "claude", channel, successor)
        .await
        .unwrap();
    assert!(
        store::renew(&pool, "windows", "claude", "win-turn", other)
            .await
            .unwrap(),
        "another node's lease is independent"
    );
    let semaphore = Arc::new(Semaphore::new(1));
    let cancel = Arc::new(CancelToken::new());
    let guard = tokio::task::spawn_blocking({
        let pool = pool.clone();
        let semaphore = semaphore.clone();
        let cancel = cancel.clone();
        move || {
            acquire_guard(
                pool,
                "windows".into(),
                "claude".into(),
                "guard".into(),
                cancel,
                semaphore.try_acquire_owned().unwrap(),
            )
        }
    })
    .await
    .unwrap()
    .unwrap();
    sqlx::query("UPDATE node_execution_leases SET expires_at=NOW()-INTERVAL '1 second' WHERE channel_id='guard'")
        .execute(&pool).await.unwrap();
    tokio::time::timeout(Duration::from_secs(17), async {
        while !cancel.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("lease loss must cancel provider");
    assert_eq!(
        semaphore.available_permits(),
        0,
        "lease expiry cannot free an executing local slot"
    );
    tokio::task::spawn_blocking(move || drop(guard))
        .await
        .unwrap();
    assert_eq!(semaphore.available_permits(), 1);
    // Normal release completes before another turn acquires the same identity.
    for _ in 0..2 {
        let pool = pool.clone();
        let semaphore = semaphore.clone();
        tokio::task::spawn_blocking(move || {
            let guard = acquire_guard(
                pool,
                "windows".into(),
                "claude".into(),
                "guard".into(),
                Arc::new(CancelToken::new()),
                semaphore.try_acquire_owned().unwrap(),
            )
            .unwrap();
            drop(guard);
        })
        .await
        .unwrap();
    }
    pool.close().await;
    fixture.drop().await;
}
