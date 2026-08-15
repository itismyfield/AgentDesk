use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::mark_done;
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::runtime_bootstrap::intake_delivery_capability::{
    SettlementCapabilities, SettlementCapabilityCache, bootstrap_from_receiver_for_test,
};
use crate::services::discord::turn_bridge::intake_settlement::{
    BridgeTurnDisposition, settle_intake_row_at_bridge_exit,
};
use crate::services::provider::ProviderKind;

// The #10 bridge-handoff source contract in intake_turn.rs is intentionally a
// lexical adjacency/count guard. As with the S-W3 wiring precedent, a matching
// token in a comment or string could satisfy it; the PostgreSQL tests below
// cover writer behavior separately, not bridge registration success.
const READY: SettlementCapabilities = SettlementCapabilities {
    stamp_dispatched: true,
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

fn assert_path_returns_before_handoff(
    source: &str,
    start_anchor: &str,
    end_anchor: &str,
    expected_returns: usize,
) {
    let path = source
        .split_once(start_anchor)
        .unwrap_or_else(|| panic!("path start anchor exists: {start_anchor}"))
        .1
        .split_once(end_anchor)
        .unwrap_or_else(|| panic!("path end anchor exists after start: {end_anchor}"))
        .0;
    assert_eq!(
        path.matches("return Ok(());").count(),
        expected_returns,
        "the anchored path must retain its own inline return(s): {start_anchor}"
    );
    assert!(
        !path.contains("dispatch_stamp::stamp_before_bridge_handoff"),
        "the anchored path must end before dispatched stamping: {start_anchor}"
    );
}

async fn assert_worker_closes_spawned(pool: &sqlx::PgPool, key: &str) {
    let id = seed_spawned(pool, key).await;
    assert!(mark_done(pool, id, "dispatch-worker").await.unwrap());
    assert_eq!(status(pool, id).await, IntakeOutboxStatus::Done);
}

fn state_for(id: i64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        Some("dispatch-stamp-test".to_owned()),
        7,
        8,
        9,
        "hello".to_owned(),
        None,
        Some("AgentDesk-claude-adk-dispatch-stamp-test".to_owned()),
        None,
        None,
        0,
    );
    state.adopt_intake_outbox(Some(id));
    state
}

async fn wait_for_generation(cache: &SettlementCapabilityCache, generation: u64) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while cache.generation_for_test() < generation {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("capability reload must publish a new generation");
}

#[tokio::test]
async fn inline_completed_paths_never_reach_dispatched_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let source = include_str!("../../intake_turn.rs");
    assert_path_returns_before_handoff(
        source,
        "let (session_id, memento_context_loaded, current_path, auto_start_provider_isolated) =",
        "let turn_start_attempt =",
        2,
    );
    assert_path_returns_before_handoff(
        source,
        "let turn_goal_kind = if !dispatch_reset_provider_state && !dispatch_recreate_tmux {",
        "let force_fresh_provider_session =",
        1,
    );
    assert_path_returns_before_handoff(
        source,
        "if stale_dispatch_guard::abort_terminal_dispatch_at_turn_start(",
        "claim_bootstrap::bootstrap_claimed_turn(",
        1,
    );
    // The Discord/session fixtures needed to drive these handler branches do
    // not carry a worker-owned PostgreSQL intake row. The source assertions
    // above therefore pin each marker-to-return segment, while the three
    // independent rows below verify only the worker's spawned-to-done close.
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
    // The production requeue helper requires live Discord mailbox and runtime
    // transition state but has no PostgreSQL intake-row parameter. This test
    // therefore combines a bounded source-order check with an independent
    // worker mark_done row; it is not an end-to-end requeue invocation.
    assert_path_returns_before_handoff(
        include_str!("../../intake_turn.rs"),
        "let Some(intake_runtime_transition) = runtime_transition::acquire_after_redirect_or_requeue(",
        "let (mut session_id, mut memento_context_loaded, current_path) =",
        1,
    );
    assert_worker_closes_spawned(&pool, "race-loss-requeue").await;
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn hosted_tui_busy_pre_submit_requeue_leaves_row_in_spawned_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    // As above, the live TUI/Discord queue path cannot be coupled to the test
    // database row through the repository's current interfaces. The bounded
    // source check and independent worker close are the declared substitute.
    assert_path_returns_before_handoff(
        include_str!("../../intake_turn.rs"),
        "if let Some(diagnostic) = tui_busy_diagnostic {",
        "if recapture_offset_after_busy_wait {",
        1,
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
async fn enforce_ready_capability_resolution_writes_dispatched_stamp_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let capabilities =
        crate::services::discord::runtime_bootstrap::intake_delivery_capability::bootstrap_for_test(
            Some(pool.clone()),
            crate::config::IntakeDeliverySettlementStage::Enforce,
        )
        .await;
    assert_eq!(capabilities.current(), READY);
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool.clone()),
            capabilities,
        );
    let id = seed_spawned(&pool, "resolved-stamp-on").await;

    stamp_before_bridge_handoff(&shared, Some(id)).await;
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Dispatched);

    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn enforce_downgrade_stops_stamping_but_keeps_settling_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let mut config = crate::config::Config::default();
    config.runtime.intake_delivery_settlement =
        crate::config::IntakeDeliverySettlementStage::Enforce;
    let (updates, receiver) = tokio::sync::watch::channel(Some(Arc::new(config.clone())));
    let capabilities = bootstrap_from_receiver_for_test(Some(pool.clone()), receiver).await;
    assert_eq!(capabilities.current(), READY);
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool.clone()),
            Arc::clone(&capabilities),
        );
    let before_downgrade = seed_spawned(&pool, "before-downgrade").await;
    stamp_before_bridge_handoff(&shared, Some(before_downgrade)).await;
    let turn_snapshot = capabilities.take_bridge_turn_snapshot(Some(before_downgrade));
    assert_eq!(
        status(&pool, before_downgrade).await,
        IntakeOutboxStatus::Dispatched
    );

    config.runtime.intake_delivery_settlement =
        crate::config::IntakeDeliverySettlementStage::Observe;
    updates
        .send(Some(Arc::new(config)))
        .expect("send Observe downgrade");
    wait_for_generation(&capabilities, 2).await;
    assert_eq!(capabilities.current(), SettlementCapabilities::default());

    let after_downgrade = seed_spawned(&pool, "after-downgrade").await;
    stamp_before_bridge_handoff(&shared, Some(after_downgrade)).await;
    assert_eq!(
        status(&pool, after_downgrade).await,
        IntakeOutboxStatus::Spawned
    );
    settle_intake_row_at_bridge_exit(
        &shared,
        &state_for(before_downgrade),
        BridgeTurnDisposition::Committed,
        turn_snapshot,
    )
    .await;
    assert_eq!(
        status(&pool, before_downgrade).await,
        IntakeOutboxStatus::Done
    );

    drop(updates);
    pool.close().await;
    database.drop().await;
}
