use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::mark_done;
use crate::db::intake_outbox_delivery_proof::{
    IntakeSettlementSource, settle_intake_done_from_receipt,
};
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use crate::services::discord::runtime_bootstrap::intake_delivery_capability::{
    SettlementCapabilities, SettlementCapabilityCache,
};

const READY: SettlementCapabilities = SettlementCapabilities {
    stamp_dispatched: true,
    settle_and_sweep: true,
};
const LOWERED: SettlementCapabilities = SettlementCapabilities {
    stamp_dispatched: false,
    settle_and_sweep: true,
};

async fn seed_spawned(pool: &sqlx::PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO public.intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id,
            user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
            status, claim_owner, spawned_at
         ) VALUES (
            'worker', 'leader', $1, $1, 'user', 'hello', 'standard', 'agent',
            'spawned', 'dispatch-worker', NOW()
         ) RETURNING id",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("seed spawned intake row")
}

async fn status(pool: &sqlx::PgPool, id: i64) -> IntakeOutboxStatus {
    sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read intake status")
}

fn shared_with(pool: &sqlx::PgPool, capabilities: SettlementCapabilities) -> Arc<SharedData> {
    crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
        Some(pool.clone()),
        SettlementCapabilityCache::for_test(capabilities),
    )
}

fn assert_return_precedes_handoff(source: &str, marker: &str) {
    let marker_at = source
        .find(marker)
        .expect("representative path marker exists");
    let return_at = source[marker_at..]
        .find("return Ok(());")
        .map(|offset| marker_at + offset)
        .expect("representative path has an inline return");
    let stamp_at = source
        .find("dispatch_stamp::stamp_before_bridge_handoff(shared, intake_outbox_id).await;")
        .expect("bridge handoff stamp exists");
    assert!(
        marker_at < return_at && return_at < stamp_at,
        "representative inline path must return before the handoff stamp: {marker}"
    );
}

async fn assert_worker_closes_spawned(pool: &sqlx::PgPool, key: &str) {
    let id = seed_spawned(pool, key).await;
    assert!(mark_done(pool, id, "dispatch-worker").await.unwrap());
    assert_eq!(status(pool, id).await, IntakeOutboxStatus::Done);
}

#[tokio::test]
async fn inline_completed_paths_never_reach_dispatched_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let source = include_str!("../../intake_turn.rs");
    for marker in [
        "No active session. Use `/start <path>` first.",
        "if let GoalCommandKind::Lifecycle(command) = turn_goal_kind {",
        "if stale_dispatch_guard::abort_terminal_dispatch_at_turn_start(",
    ] {
        assert_return_precedes_handoff(source, marker);
    }
    for key in ["inline-no-session", "inline-goal", "inline-stale-dispatch"] {
        assert_worker_closes_spawned(&pool, key).await;
    }
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn race_loss_requeue_leaves_row_in_spawned_and_worker_closes_it_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    assert_return_precedes_handoff(
        include_str!("../../intake_turn.rs"),
        "runtime_transition::acquire_after_redirect_or_requeue(",
    );
    assert_worker_closes_spawned(&pool, "race-loss-requeue").await;
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn hosted_tui_busy_pre_submit_requeue_leaves_row_in_spawned_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    assert_return_precedes_handoff(
        include_str!("../../intake_turn.rs"),
        "Claude TUI busy follow-up queued before prompt submission",
    );
    assert_worker_closes_spawned(&pool, "hosted-tui-busy").await;
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn stamp_is_skipped_when_capability_not_ready_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    sqlx::query(
        "ALTER TABLE public.intake_outbox
         DROP CONSTRAINT intake_outbox_dispatched_requires_clock",
    )
    .execute(&pool)
    .await
    .expect("make the migrated schema fail the capability probe");
    let capabilities =
        crate::services::discord::runtime_bootstrap::intake_delivery_capability::bootstrap_for_test(
            Some(pool.clone()),
            crate::config::IntakeDeliverySettlementStage::Enforce,
        )
        .await;
    assert!(!capabilities.current().stamp_dispatched);
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool.clone()),
            capabilities,
        );
    let id = seed_spawned(&pool, "stamp-off").await;

    stamp_before_bridge_handoff(&shared, None).await;
    stamp_before_bridge_handoff(&shared, Some(id)).await;
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);

    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn stamp_is_written_when_ready_capability_is_injected_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let shared = shared_with(&pool, READY);
    let id = seed_spawned(&pool, "stamp-on").await;

    stamp_before_bridge_handoff(&shared, Some(id)).await;
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Dispatched);

    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn enforce_downgrade_stops_stamping_but_keeps_settling_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let before_downgrade = seed_spawned(&pool, "before-downgrade").await;
    stamp_before_bridge_handoff(&shared_with(&pool, READY), Some(before_downgrade)).await;
    assert_eq!(
        status(&pool, before_downgrade).await,
        IntakeOutboxStatus::Dispatched
    );

    let after_downgrade = seed_spawned(&pool, "after-downgrade").await;
    stamp_before_bridge_handoff(&shared_with(&pool, LOWERED), Some(after_downgrade)).await;
    assert_eq!(
        status(&pool, after_downgrade).await,
        IntakeOutboxStatus::Spawned
    );
    let mut transaction = pool.begin().await.unwrap();
    assert!(
        settle_intake_done_from_receipt(
            &mut transaction,
            after_downgrade,
            IntakeSettlementSource::Committed,
        )
        .await
        .unwrap()
    );
    transaction.commit().await.unwrap();
    assert_eq!(
        status(&pool, after_downgrade).await,
        IntakeOutboxStatus::Done
    );

    pool.close().await;
    database.drop().await;
}
