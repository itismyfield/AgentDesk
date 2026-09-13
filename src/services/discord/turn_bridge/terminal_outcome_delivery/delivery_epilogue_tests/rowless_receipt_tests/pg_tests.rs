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
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor".into());
        successor.turn_start_offset = Some(0);
        inflight::save_inflight_state(&successor).unwrap();
        ctx.codex_tui_terminal_range = None;
        let output = run(ctx, state).await;
        let TerminalOutcomeDeliveryOutcome::DeferredToOutbox { outbox_id } = &output.outcome else {
            panic!("must retain a real outbox row");
        };
        ids.push(*outbox_id);
        assert!(!output.terminal_delivery_committed && !output.bridge_skip_holder_owns_inflight);
        run_postlude(&driver, output, false, false).await;
        let fresh =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap();
        assert_eq!(fresh.turn_nonce, successor.turn_nonce);
        assert_eq!(fresh.turn_start_offset, successor.turn_start_offset);
        assert!(!fresh.terminal_delivery_committed);
    }
    assert_eq!(
        ids[0], ids[1],
        "the same episode retries the same durable obligation"
    );
    let (content, source, session_key, reason, status): (
        String,
        String,
        Option<String>,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT content, source, session_key, reason_code, status FROM message_outbox WHERE id=$1",
    )
    .bind(ids[0])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(content, DRIVER_BODY);
    assert_eq!(source, "headless_turn");
    assert_eq!(status, "pending");
    assert!(
        session_key.is_none(),
        "never rebind the successor session marker"
    );
    let identity: serde_json::Value = serde_json::from_str(&reason).unwrap();
    assert_eq!(identity["turn_nonce"], "receipt-nonce");
    assert_eq!(identity["turn_start_offset"], 0);
    assert!(
        identity["source"].is_null(),
        "unknown range remains unknown"
    );
    assert!(driver.observations().is_empty());
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
async fn exact_receipt_rowless_terminal_insert_failure_falls_back_without_touching_successor_5521()
{
    for post_fails in [false, true] {
        let mut driver = TerminalDeliveryDriver::new(
            if post_fails {
                ReplaceBehaviour::FailedPost
            } else {
                ReplaceBehaviour::Edited
            },
            1,
        );
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "receipt_insert_failure",
            "terminal outbox insert failure",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
        sqlx::query(
            "ALTER TABLE message_outbox ADD CONSTRAINT reject_receipt_fixture CHECK (false)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor".into());
        successor.turn_start_offset = Some(64);
        inflight::save_inflight_state(&successor).unwrap();
        ctx.codex_tui_terminal_range = None;
        let output = run(ctx, state).await;
        assert_eq!(output.terminal_delivery_committed, !post_fails);
        assert!(
            !output.preserve_inflight_for_cleanup_retry && !output.bridge_skip_holder_owns_inflight
        );
        if post_fails {
            assert!(
                matches!(&output.outcome, TerminalOutcomeDeliveryOutcome::Unresolved { error } if error.contains("reject_receipt_fixture") && error.contains("POST failed"))
            );
        }
        run_postlude(&driver, output, false, false).await;
        assert!(
            driver
                .observations()
                .iter()
                .all(|o| o.call == DriverCall::Send)
        );
        let fresh =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap();
        assert_eq!(fresh.turn_nonce, successor.turn_nonce);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        pool.close().await;
        db.drop().await;
    }
}
