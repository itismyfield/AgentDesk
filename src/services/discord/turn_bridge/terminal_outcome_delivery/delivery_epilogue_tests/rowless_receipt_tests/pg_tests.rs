//! PostgreSQL-backed terminal obligation and cancellation witnesses.

use super::*;

#[tokio::test]
async fn exact_receipt_rowless_terminal_unknown_foreign_anchor_preserves_retry_5521() {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "receipt_handoff",
        "rowless terminal durable retry",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    let mut keys = Vec::new();
    for _ in 0..2 {
        let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor".into());
        inflight::save_inflight_state(&successor).unwrap();
        ctx.codex_tui_terminal_range = None;
        let output = run(ctx, state).await;
        let TerminalOutcomeDeliveryOutcome::DeferredToCustody { key } = &output.outcome else {
            panic!("actual file obligation required")
        };
        keys.push(key.clone());
        run_postlude(&driver, output, false, false).await;
        let fresh =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap();
        assert_eq!(fresh.turn_nonce, successor.turn_nonce);
    }
    assert_eq!(keys[0], keys[1]);
    let records = custody_records(&driver);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["payload"]["full_response"], DRIVER_BODY);
    assert_eq!(
        records[0]["payload"]["local"]["turn_nonce"],
        "receipt-nonce"
    );
    assert!(
        records[0]["payload"]["admitted"].is_null(),
        "NoRange remains unknown"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "async outbox transport must not escape the source lease"
    );
    assert!(driver.observations().is_empty());
    assert_eq!(drain_custody(&driver).await.unwrap(), 1);
    assert_eq!(driver.completed_publications(), 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_cancellation_settles_work_before_postlude_5521() {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "receipt_cancel",
        "rowless receipt cancellation",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    let dispatch_id = "receipt-cancel-5521";
    crate::dispatch::test_support::seed_pg_dispatch(&pool, dispatch_id, "receipt cancellation")
        .await;
    sqlx::query("INSERT INTO sessions (session_key, provider, status) VALUES ('receipt-parent', 'codex', 'turn_active')").execute(&pool).await.unwrap();
    let child = crate::db::session_observability::insert_background_child_pg(
        &pool,
        &crate::db::session_observability::BackgroundChildSpawn {
            parent_session_key: "receipt-parent".into(),
            provider: Some("codex".into()),
            tool_name: "Task".into(),
            tool_input: "{}".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let (mut ctx, mut state, source) = receipt_parts(&driver, ProviderKind::Codex);
    ctx.cancelled = true;
    ctx.entry_was_rowless = true;
    state.dispatch_id = Some(dispatch_id.into());
    state.active_background_child_session_ids.push(child);
    inflight::save_inflight_state(&state.inflight_state).unwrap();
    dr::record_current_pinned_delivery(&source, DRIVER_CURRENT_MSG_ID).unwrap();
    let output = run(ctx, state).await;
    assert!(output.active_background_child_session_ids.is_empty());
    assert!(!output.preserve_inflight_for_cleanup_retry);
    run_postlude(&driver, output, false, true).await;
    let status: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id=$1")
        .bind(dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
    let child_status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE id=$1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(child_status, "aborted");
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID).is_none()
    );
    assert!(driver.observations().is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_custody_ack_survives_dispatch_failure_5521() {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "receipt_cleanup_retry",
        "custody transport ack survives dispatch failure",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    let dispatch_id = "receipt-custody-cancel-5521";
    crate::dispatch::test_support::seed_pg_dispatch(&pool, dispatch_id, "custody cancel").await;
    let (mut ctx, mut state, _) = receipt_parts(&driver, ProviderKind::Codex);
    ctx.cancelled = true;
    ctx.codex_tui_terminal_range = None;
    state.dispatch_id = Some(dispatch_id.into());
    let mut successor = state.inflight_state.clone();
    successor.turn_nonce = Some("successor".into());
    inflight::save_inflight_state(&successor).unwrap();
    let output = run(ctx, state).await;
    run_postlude(&driver, output, false, true).await;
    sqlx::query("ALTER TABLE task_dispatches RENAME TO custody_dispatches_unavailable")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(drain_custody(&driver).await.unwrap(), 0);
    let records = custody_records(&driver);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["payload"]["delivery_receipts"][0],
        DRIVER_FALLBACK_ANCHOR_MSG_ID
    );
    assert_eq!(driver.completed_publications(), 1);
    assert_eq!(drain_custody(&driver).await.unwrap(), 0);
    assert_eq!(
        driver.completed_publications(),
        1,
        "cleanup retry must not POST again"
    );
    sqlx::query("ALTER TABLE custody_dispatches_unavailable RENAME TO task_dispatches")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(drain_custody(&driver).await.unwrap(), 1);
    assert_eq!(driver.completed_publications(), 1);
    let status: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id=$1")
        .bind(dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
    let fresh =
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID).unwrap();
    assert_eq!(fresh.turn_nonce, successor.turn_nonce);
    assert!(
        driver
            .observations()
            .iter()
            .all(|o| o.call == DriverCall::Send)
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn exact_receipt_custody_retains_failed_child_ids_until_pg_close_5521() {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "receipt_child_retry",
        "custody preserves failed child closure identities",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    sqlx::query("INSERT INTO sessions (session_key, provider, status) VALUES ('child-retry-parent', 'codex', 'turn_active')").execute(&pool).await.unwrap();
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            crate::db::session_observability::insert_background_child_pg(
                &pool,
                &crate::db::session_observability::BackgroundChildSpawn {
                    parent_session_key: "child-retry-parent".into(),
                    provider: Some("codex".into()),
                    tool_name: "Task".into(),
                    tool_input: "{}".into(),
                },
            )
            .await
            .unwrap()
            .unwrap(),
        );
    }
    let failed_child = children[0];
    sqlx::query(&format!("CREATE FUNCTION fail_child_close() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id = {failed_child} AND NEW.closed_at IS NOT NULL THEN RAISE EXCEPTION 'fixture child close unavailable'; END IF; RETURN NEW; END $$"))
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_child_close BEFORE UPDATE ON sessions FOR EACH ROW EXECUTE FUNCTION fail_child_close()")
        .execute(&pool).await.unwrap();
    let (mut ctx, mut state, _) = receipt_parts(&driver, ProviderKind::Codex);
    ctx.cancelled = true;
    ctx.codex_tui_terminal_range = None;
    state.active_background_child_session_ids = children.clone();
    let mut successor = state.inflight_state.clone();
    successor.turn_nonce = Some("successor-B".into());
    inflight::save_inflight_state(&successor).unwrap();
    let output = run(ctx, state).await;
    assert!(matches!(
        output.outcome,
        TerminalOutcomeDeliveryOutcome::DeferredToCustody { .. }
    ));
    for _ in 0..2 {
        assert_eq!(drain_custody(&driver).await.unwrap(), 0);
        let records = custody_records(&driver);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0]["payload"]["children"],
            serde_json::json!([failed_child])
        );
        assert_eq!(
            driver.completed_publications(),
            1,
            "child retry never republishes acknowledged transport"
        );
    }
    let closed: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM sessions WHERE id = ANY($1) AND closed_at IS NOT NULL")
            .bind(&children)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        closed,
        vec![children[1]],
        "only confirmed PG closure releases a child ID"
    );
    sqlx::query("DROP TRIGGER fail_child_close ON sessions")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(drain_custody(&driver).await.unwrap(), 1);
    assert!(custody_records(&driver).is_empty());
    assert_eq!(driver.completed_publications(), 1);
    let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE id = $1")
        .bind(failed_child)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "aborted");
    assert_eq!(
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
            .unwrap()
            .turn_nonce,
        successor.turn_nonce
    );
    pool.close().await;
    db.drop().await;
}
