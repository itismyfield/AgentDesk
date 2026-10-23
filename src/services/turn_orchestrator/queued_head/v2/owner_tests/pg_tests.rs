use super::*;

#[tokio::test]
async fn v2_real_owner_entry_and_persistent_notice_are_independent_pg() {
    let _root = TestRuntimeRootGuard::new();
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_v2_owner",
        "v2 actual entry",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(727270));
    handle
        .v2_adopt(path(727270), "scope-727270".into())
        .await
        .unwrap();
    let a = prepare(&handle, 100, "pane-pg").await;
    let count = Arc::new(AtomicUsize::new(0));
    let effect = count.clone();
    assert!(
        !handle
            .v2_effect(1000, move |frame| {
                assert_eq!(frame, "exact prepared frame");
                effect.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
            .await
            .unwrap()
    );
    assert!(
        handle.v2_effect(1001, |_| Ok(true)).await.is_err(),
        "a consumed permit cannot be issued again after writer completion"
    );
    handle
        .v2_request(|slot| slot.as_mut().unwrap().tick(31_000))
        .await
        .unwrap();
    let retained = snapshot(&handle).await["retained_inputs"][0].clone();
    let enqueue = ChannelMailboxHandle::v2_notice;
    let dead = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1:1/missing")
        .unwrap();
    dead.close().await;
    assert!(
        enqueue(&dead, "codex", 727270, "scope-727270", &retained)
            .await
            .is_err()
    );
    prepare(&handle, 200, "pane-pg").await;
    assert!(
        handle.v2_effect(32_000, |_| Ok(true)).await.unwrap(),
        "notice enqueue failure cannot block B"
    );
    let first = enqueue(&pool, "codex", 727270, "scope-727270", &retained)
        .await
        .unwrap();
    handle
        .v2_request(move |slot| slot.as_mut().unwrap().notice_enqueued(&a))
        .await
        .unwrap();
    handle
        .v2_adopt(path(727270), "scope-727270".into())
        .await
        .unwrap();
    let projected = snapshot(&handle).await["retained_inputs"][0].clone();
    assert_eq!(projected["notice"]["enqueued"], true);
    let second = enqueue(&pool, "codex", 727270, "scope-727270", &projected)
        .await
        .unwrap();
    assert_eq!(
        first, second,
        "notice crash replay must keep one stable outbox identity"
    );
    let rows: (i64, String) = sqlx::query_as(
        "SELECT count(*), min(content) FROM message_outbox WHERE source='retained_input_notice'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows.0, 1);
    assert!(
        rows.1.contains("원문을 보관"),
        "outbox preserves retained notice payload"
    );
    assert!(
        ChannelMailboxHandle::v2_notice_is_non_turn(&rows.1),
        "notice loopback admits zero new turns"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "retained A is never automatically retyped"
    );
    sqlx::query("UPDATE message_outbox SET status='failed' WHERE id=$1")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    prepare(&handle, 300, "pane-pg").await;
    assert!(
        handle.v2_effect(33_000, |_| Ok(true)).await.unwrap(),
        "failed notice delivery cannot block C"
    );
    pool.close().await;
    db.drop().await;
}
