use super::*;
use crate::services::discord::catch_up::classification_order_tests::i7441_tests::assert_aged_delivery_settled;
use crate::services::discord::outbound::completed_turn_ledger;

#[tokio::test]
async fn i7441_no_range_completion_uses_captured_input_when_current_row_changes() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 2);
    let (ctx, state) = driver.parts();
    let mut running = Box::pin(run_terminal_outcome_delivery(ctx, state));
    let mut entered = false;
    for _ in 0..64 {
        assert!(
            futures::poll!(running.as_mut()).is_pending(),
            "publish must suspend before completion"
        );
        if !driver.publish_entries().is_empty() {
            entered = true;
            break;
        }
    }
    assert!(
        entered,
        "actual replace must be entered before the row changes"
    );
    let mut newer = driver.inflight.clone();
    newer.user_msg_id = DRIVER_USER_MSG_ID + 10;
    newer.turn_nonce = Some("newer-input-episode".into());
    crate::services::discord::inflight::save_inflight_state(&newer).unwrap();
    let output = running.await;
    assert!(output.terminal_delivery_committed);
    let settled =
        completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, DRIVER_CHANNEL_ID);
    assert!(settled.contains(&DRIVER_USER_MSG_ID));
    assert!(
        !settled.contains(&newer.user_msg_id),
        "current row is not proof of the captured answer's input"
    );
}

fn use_headless_gateway(driver: &mut TerminalDeliveryDriver) {
    driver.gateway = Arc::new(DriverGateway {
        chain_locally: true,
        direct: false,
        marker: driver.marker.clone(),
        observations: driver.observations.clone(),
        completed_publications: driver.completed_publications.clone(),
        published_bodies: driver.published_bodies.clone(),
        replace: ReplaceBehaviour::Edited,
        yields_per_call: 0,
        check: driver.body_check.clone(),
    });
}

#[tokio::test]
async fn i7441_no_range_zero_and_synthetic_inputs_do_not_settle() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for input in [0, crate::services::discord::synthetic_headless_message_id::SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR] {
        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        driver.inflight.user_msg_id = input;
        crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
        let (mut ctx, state) = driver.parts();
        ctx.user_msg_id = (input != 0).then(|| MessageId::new(input));
        let output = run_terminal_outcome_delivery(ctx, state).await;
        assert!(output.terminal_delivery_committed);
        assert!(completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, DRIVER_CHANNEL_ID).is_empty());
    }
}

#[tokio::test]
async fn i7441_headless_cancelled_and_ambiguous_do_not_settle() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for cancelled in [true, false] {
        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        use_headless_gateway(&mut driver);
        let (ctx, state) = driver.parts();
        state
            .cancel_token
            .cancelled
            .store(cancelled, Ordering::SeqCst);
        let output = run_terminal_outcome_delivery(ctx, state).await;
        assert_eq!(output.terminal_delivery_committed, cancelled);
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
                .is_empty(),
            "cancelled={cancelled}"
        );
    }
}

#[tokio::test]
async fn i7441_o_delegated_completion_without_receipt_does_not_settle() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    driver.inflight.runtime_kind = Some(ClaudeTui);
    crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[(
        DRIVER_CHANNEL_ID,
        ClaudeTui,
    )]);
    let (ctx, state) = driver.parts();
    let output = run_terminal_outcome_delivery(ctx, state).await;
    assert!(output.terminal_delivery_committed);
    assert!(
        completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
            .is_empty()
    );
}

#[tokio::test]
async fn i7441_no_range_actual_response_is_settled_by_next_sweep() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for replace in [
        ReplaceBehaviour::Edited,
        ReplaceBehaviour::FallbackAfterEditFailure,
        ReplaceBehaviour::Failed,
    ] {
        let mut driver = TerminalDeliveryDriver::new(replace, 0);
        driver.inflight.turn_nonce = Some("no-range-captured-episode".into());
        crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
        completed_turn_ledger::record_merged_alias(
            &ProviderKind::Claude,
            DRIVER_CHANNEL_ID,
            DRIVER_USER_MSG_ID,
            "no-range-captured-episode",
            &[DRIVER_USER_MSG_ID - 1],
        );
        let (ctx, state) = driver.parts();
        let output = run_terminal_outcome_delivery(ctx, state).await;
        if replace != ReplaceBehaviour::Failed {
            assert_eq!(
                output.terminal_delivery_committed,
                replace == ReplaceBehaviour::Edited
            );
            assert_aged_delivery_settled(
                driver._temp.path(),
                &driver.shared,
                &ProviderKind::Claude,
                ChannelId::new(DRIVER_CHANNEL_ID),
                MessageId::new(DRIVER_USER_MSG_ID),
            )
            .await;
            assert!(
                completed_turn_ledger::settled_user_msg_ids(
                    &ProviderKind::Claude,
                    DRIVER_CHANNEL_ID
                )
                .contains(&(DRIVER_USER_MSG_ID - 1))
            );
        } else {
            assert!(!output.terminal_delivery_committed);
            assert!(
                completed_turn_ledger::settled_user_msg_ids(
                    &ProviderKind::Claude,
                    DRIVER_CHANNEL_ID
                )
                .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn i7441_headless_visible_outbox_is_settled_by_next_sweep_pg() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441_headless",
        "visible headless settlement",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    use_headless_gateway(&mut driver);
    let worker_pool = pool.clone();
    let worker = tokio::spawn(async move {
        tokio::time::timeout(DRIVER_TIMEOUT, async {
            loop {
                let id: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM message_outbox WHERE source='headless_turn' LIMIT 1",
                )
                .fetch_optional(&worker_pool)
                .await
                .unwrap();
                if let Some(id) = id {
                    sqlx::query("UPDATE message_outbox SET status='sent' WHERE id=$1")
                        .bind(id)
                        .execute(&worker_pool)
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("headless outbox must be enqueued");
    });
    let (ctx, state) = driver.parts();
    let output = run_terminal_outcome_delivery(ctx, state).await;
    worker.await.unwrap();
    assert!(output.terminal_delivery_committed);
    assert_aged_delivery_settled(
        driver._temp.path(),
        &driver.shared,
        &ProviderKind::Claude,
        ChannelId::new(DRIVER_CHANNEL_ID),
        MessageId::new(DRIVER_USER_MSG_ID),
    )
    .await;
    drop(driver);
    pool.close().await;
    db.drop().await;
}
