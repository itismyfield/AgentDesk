use super::*;

#[tokio::test]
async fn monitor_receipt_retention_is_bounded_and_outlives_card_retention_pg() {
    let (_database, pool) = database("bounded Monitor receipt retention").await;
    sqlx::query(
        "INSERT INTO task_notification_monitor_receipt
         (channel_id,provider,session_key,event_key,payload_fingerprint,discord_message_id,bot_key,observed_at)
         SELECT $1,'claude','retention','old',md5('old:' || n::text),$2,'provider:claude',
                NOW() - INTERVAL '29 days' FROM generate_series(1,80) AS n",
    )
    .bind(CHANNEL as i64)
    .bind(LEGACY_MESSAGE as i64)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_notification_monitor_receipt
         (channel_id,provider,session_key,event_key,payload_fingerprint,discord_message_id,bot_key,observed_at)
         VALUES ($1,'claude','retention','recent',md5('recent'),$2,'provider:claude',NOW()),
                ($1,'claude','retention','past-card-gc',md5('past-card-gc'),$2,'provider:claude',
                 NOW() - INTERVAL '8 days')",
    )
    .bind(CHANNEL as i64)
    .bind(LEGACY_MESSAGE as i64)
    .execute(&pool)
    .await
    .unwrap();
    store::cleanup_old_rows_pg_checked(&pool).await.unwrap();
    let old: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_monitor_receipt WHERE event_key='old'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        old, 16,
        "one cleanup deletes only the existing 64-row batch"
    );
    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_monitor_receipt WHERE event_key <> 'old'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        retained, 2,
        "recent receipts outlive the seven-day card GC window"
    );
    store::cleanup_old_rows_pg_checked(&pool).await.unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_notification_monitor_receipt")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 2,
        "expired receipts are eventually removed; recent receipts survive"
    );
}

#[tokio::test]
async fn monitor_receipt_many_payloads_preserve_new_event_and_replay_pg() {
    let (_database, pool) = database("indexed Monitor receipt candidate lookup").await;
    let transport = Transport::default();
    let first = monitor(Some("indexed-first"), 0);
    deliver(&pool, &transport, &first).await;
    sqlx::query(
        "INSERT INTO task_notification_monitor_receipt
         (channel_id,provider,session_key,event_key,payload_fingerprint,discord_message_id,bot_key)
         SELECT $1,$2,$3,$4,md5('noise:' || n::text),$5,'provider:claude'
         FROM generate_series(1,1000) AS n",
    )
    .bind(CHANNEL as i64)
    .bind(&first.scope.provider)
    .bind(&first.scope.session_key)
    .bind(&first.scope.event_key)
    .bind(LEGACY_MESSAGE as i64)
    .execute(&pool)
    .await
    .unwrap();
    let next = monitor(Some("indexed-next"), 1);
    deliver(&pool, &transport, &next).await;
    assert_eq!(snapshot(&pool, &next).await.count, 2);
    assert_eq!(transport.counts(), (1, 1));
    deliver(&pool, &transport, &first).await;
    assert_eq!(
        transport.counts(),
        (1, 1),
        "indexed receipt replay makes no transport call"
    );
    assert_eq!(snapshot(&pool, &next).await.count, 2);
    pool.close().await;
    drop(_database);
    {
        let _database = DispatchPostgresTestDb::try_create(
            "adk_inline_tc",
            "exact receipt lookup bypasses legacy candidates",
        )
        .await
        .expect("receipt lock fixture requires isolated PostgreSQL");
        let pool = _database.connect_and_migrate_with_max_connections(2).await;
        assert_eq!(
            pool.options().get_max_connections(),
            2,
            "the blocker and receipt replay must own distinct connections"
        );
        let transport = Transport::default();
        for (source, number) in [(Some("short-circuit-source"), 8), (None, 9)] {
            let observed = monitor(source, number);
            deliver(&pool, &transport, &observed).await;
            let before = transport.counts();
            let candidate: i64 = sqlx::query_scalar(
                "INSERT INTO task_notification_monitor_receipt
                 (channel_id,provider,session_key,event_key,payload_fingerprint,
                  discord_message_id,bot_key,legacy_render_count)
                 VALUES ($1,$2,$3,$4,md5($5),$6,'notify',1) RETURNING id",
            )
            .bind(CHANNEL as i64)
            .bind(&observed.scope.provider)
            .bind(&observed.scope.session_key)
            .bind(&observed.scope.event_key)
            .bind(format!("unrelated-legacy-{number}"))
            .bind(LEGACY_MESSAGE as i64)
            .fetch_one(&pool)
            .await
            .unwrap();
            let mut blocker = pool.begin().await.unwrap();
            sqlx::query("SELECT id FROM task_notification_monitor_receipt WHERE id=$1 FOR UPDATE")
                .bind(candidate)
                .fetch_one(&mut *blocker)
                .await
                .unwrap();
            let replay = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                deliver(&pool, &transport, &observed),
            )
            .await;
            blocker.rollback().await.unwrap();
            assert!(
                replay.is_ok(),
                "source/payload receipt hit must bypass legacy candidate queries and their row locks: {source:?}"
            );
            assert_eq!(
                transport.counts(),
                before,
                "exact receipt replay must make no transport call"
            );
        }
    }
}
