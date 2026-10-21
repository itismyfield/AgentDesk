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

// The bridge-handoff source contract in intake_turn.rs is a lexical adjacency/count guard
// on comment-free code; the PostgreSQL tests below cover writer behavior, not registration.
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

const INTAKE_TURN: &str = include_str!("../../intake_turn.rs");
const HANDLER: &str = "pub(super) async fn handle_text_message(";
const STAMP: &str = "dispatch_stamp::stamp_before_bridge_handoff";
const EXECUTED: &str = "return Ok(IntakeExecution::Executed);";
const DEFERRED: &str = "return Ok(IntakeExecution::deferred(channel_id));";
/// (start anchor, end anchor, the path's own return, its count): three inline
/// completions, the race-loss requeue and the hosted-TUI busy requeue.
const PATHS: [(&str, &str, &str, usize); 5] = [
    (
        "let (session_id, memento_context_loaded, current_path, auto_start_provider_isolated) =",
        "let add_turn_pending =",
        EXECUTED,
        2,
    ),
    (
        "let pre_admission_control::PreAdmission::Continue(turn_goal_kind) =",
        "let force_fresh_provider_session =",
        EXECUTED,
        1,
    ),
    (
        "if stale_dispatch_guard::abort_terminal_dispatch_at_turn_start(",
        "claim_bootstrap::bootstrap_claimed_turn(",
        EXECUTED,
        1,
    ),
    (
        "let Some(intake_runtime_transition) = runtime_transition::acquire_after_redirect_or_requeue(",
        "let (mut session_id, mut memento_context_loaded, current_path) =",
        DEFERRED,
        1,
    ),
    (
        "if let Some(diagnostic) = tui_busy_diagnostic {",
        "if recapture_offset_after_busy_wait {",
        DEFERRED,
        1,
    ),
];

/// Classification 2: the path runs between as-written anchors inside the intake
/// handler, comments excluded; returns are counted, not proven reachable.
fn handoff_path_gaps(
    source: &str,
    (start, end, expected, count): (&str, &str, &str, usize),
) -> Vec<String> {
    use crate::services::discord::router::source_guard_support::Lexed;
    let Some(handler) = Lexed::new(source).block(HANDLER) else {
        return vec![format!("missing {HANDLER}")];
    };
    let Some(path) = handler.find(start).and_then(|from| {
        let to = handler.find_after(from, end)?;
        Some(handler.slice(from..to))
    }) else {
        return vec![format!("missing path {start} … {end}")];
    };
    let mut gaps = Vec::new();
    if path.count("return Ok(") != count || path.count(expected) != count {
        gaps.push(format!(
            "the anchored path must keep {count} × {expected}: {start}"
        ));
    }
    if path.text().contains(STAMP) {
        gaps.push(format!(
            "the anchored path must end before dispatched stamping: {start}"
        ));
    }
    gaps
}

fn assert_path_returns_before_handoff(source: &str, path: (&str, &str, &str, usize)) {
    assert_eq!(handoff_path_gaps(source, path), Vec::<String>::new());
}

#[test]
fn handoff_path_guard_rejects_commented_anchors_and_coded_stamps() {
    use crate::services::discord::router::source_guard_support::forge;
    use crate::test_support::CommentForgery;
    for path in PATHS {
        assert_path_returns_before_handoff(INTAKE_TURN, path);
        for needle in [path.0, path.1, path.2] {
            for forgery in CommentForgery::ALL {
                let forged = forge(INTAKE_TURN, needle, forgery);
                assert!(forged.contains(needle));
                let gaps = handoff_path_gaps(&forged, path);
                assert!(!gaps.is_empty(), "{forgery:?} `{needle}` passes");
            }
        }
        let after_start =
            |insert: &str| INTAKE_TURN.replacen(path.0, &format!("{}{insert}", path.0), 1);
        let prose = after_start(&format!(" /* {STAMP} */ // {STAMP}\n"));
        assert_eq!(handoff_path_gaps(&prose, path), Vec::<String>::new());
        let coded = after_start(&format!("\n{STAMP}();\n"));
        assert_eq!(handoff_path_gaps(&coded, path).len(), 1, "{}", path.0);
    }
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
    for path in &PATHS[..3] {
        assert_path_returns_before_handoff(INTAKE_TURN, *path);
    }
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
    assert_path_returns_before_handoff(INTAKE_TURN, PATHS[3]);
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
    assert_path_returns_before_handoff(INTAKE_TURN, PATHS[4]);
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
