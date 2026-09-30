//! A worker leaves an O channel's rows to its ready gateway and keeps draining other channels.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{InsertPendingPayload, insert_pending};
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};

const O: u64 = 4_380_001;
const LEGACY: u64 = 4_380_002;

async fn seed(pool: &PgPool, channel: u64, message: u64) -> i64 {
    let payload = InsertPendingPayload {
        target_instance_id: "worker-1".into(),
        forwarded_by_instance_id: "leader-1".into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: channel.to_string(),
        user_msg_id: message.to_string(),
        request_owner_id: "100".into(),
        request_owner_name: Some("Tester".into()),
        user_text: "hello".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "standard".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "agent-o".into(),
        provider: "claude".into(),
    };
    let id = insert_pending(pool, &payload, 1, None).await.unwrap();
    // Claims go oldest first; keep creation times apart.
    tokio::time::sleep(Duration::from_millis(15)).await;
    id
}

/// Status and claim owner; a row the O check held carries no failure.
async fn state(pool: &PgPool, id: i64) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT status::TEXT, claim_owner, last_error FROM intake_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn pending() -> (String, Option<String>, Option<String>) {
    ("pending".into(), None, None)
}

/// Past the O check a row reaches runtime resolution, which this test runtime cannot satisfy.
fn ran_past_o(state: (String, Option<String>, Option<String>)) -> bool {
    let error = state.2.unwrap_or_default();
    state.0 == "failed_pre_accept" && error.starts_with("runtime ownership")
}

#[tokio::test(flavor = "current_thread")]
async fn a_worker_leaves_o_rows_to_their_ready_gateway_and_drains_the_rest_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('agent-o', 'Test', 'claude', 'unused')")
        .execute(&pool)
        .await
        .unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let not_cancelled = || false;
    let tick = || run_intake_worker_tick(&pool, &shared, "worker-1", "claude", "o", &not_cancelled);

    let unread = test_probe::answers(&[]);
    let off_row = seed(&pool, O, 1).await;
    let selected = test_override::force_channels(&[(O, ClaudeTui)]);
    let off = test_override::force_off();
    assert_eq!(tick().await.unwrap(), TickOutcome::Processed);
    assert!(ran_past_o(state(&pool, off_row).await), "writer off");
    drop((off, selected, unread));

    let _selected = test_override::force_channels(&[(O, ClaudeTui)]);
    let (o_row, legacy_row) = (seed(&pool, O, 2).await, seed(&pool, LEGACY, 3).await);
    let not_ready = test_probe::answer_with(|_| false);
    assert_eq!(tick().await.unwrap(), TickOutcome::Processed);
    assert!(
        ran_past_o(state(&pool, legacy_row).await),
        "the later row is not stuck"
    );
    assert_eq!(tick().await.unwrap(), TickOutcome::QueueEmpty);
    assert_eq!(
        state(&pool, o_row).await,
        pending(),
        "never claimed off its gateway"
    );
    drop(not_ready);

    let lost_after_claim = test_probe::answers(&[true, false]);
    assert_eq!(tick().await.unwrap(), TickOutcome::Held);
    assert_eq!(
        state(&pool, o_row).await,
        pending(),
        "returned before accept"
    );
    drop(lost_after_claim);

    let _ready = test_probe::answer_with(|_| true);
    assert_eq!(tick().await.unwrap(), TickOutcome::Processed);
    assert!(
        ran_past_o(state(&pool, o_row).await),
        "the ready gateway takes it"
    );

    pool.close().await;
    fixture.drop().await;
}

/// Refuses any move to `accepted` in this test database, so an accept attempt fails the tick
/// before a turn could start.
async fn refuse_accepts(pool: &PgPool) {
    sqlx::query(
        "CREATE FUNCTION refuse_accept() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'accept attempted'; END $$ LANGUAGE plpgsql",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_accept BEFORE UPDATE ON intake_outbox FOR EACH ROW
         WHEN (NEW.status = 'accepted') EXECUTE FUNCTION refuse_accept()",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn readiness_lost_after_runtime_and_uploads_resolve_holds_the_row_at_the_last_check_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('agent-o', 'Test', 'claude', 'unused')")
        .execute(&pool)
        .await
        .unwrap();
    refuse_accepts(&pool).await;
    let owner = crate::services::discord::health::owner_runtime_for_tests::registered("claude");
    let (_registry, shared) = owner.await;
    let not_cancelled = || false;

    let _selected = test_override::force_channels(&[(O, ClaudeTui)]);
    let row = seed(&pool, O, 1).await;
    let _claim_then_first_check_then_lost = test_probe::answers(&[true, true, false]);
    let outcome = run_intake_worker_tick(&pool, &shared, "worker-1", "claude", "o", &not_cancelled);
    assert!(matches!(outcome.await, Ok(TickOutcome::Held)));
    let (status, owner, error): (String, Option<String>, Option<String>) = state(&pool, row).await;
    assert_eq!((status, owner, error), pending(), "returned before accept");
    let marks: (bool, bool, i32, i32) = sqlx::query_as(
        "SELECT accepted_at IS NULL, spawned_at IS NULL, retry_count, attempt_no
         FROM intake_outbox WHERE id = $1",
    )
    .bind(row)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        marks,
        (true, true, 0, 1),
        "no accept, spawn, retry or new attempt"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "no retry row was queued");

    pool.close().await;
    fixture.drop().await;
}

/// This process's writer readiness is global, so the case runs in a child of its own.
fn in_own_process(name: &str) -> bool {
    const CHILD: &str = "ADK_TEST_O_TOPOLOGY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    // Without PostgreSQL the child cannot run; fail here with the reason every PG test gives.
    crate::db::postgres::postgres_test_database_url_base()
        .expect("POSTGRES_TEST_DATABASE_URL_BASE required for db::auto_queue tests");
    let root = tempfile::tempdir().unwrap();
    let qualified = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env_remove(test_override::CHILD_ENV)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    false
}

/// The hook's answer for `channel` with routing disabled: run it here, or hold it.
async fn routed_locally(pool: &PgPool, channel: u64) -> bool {
    use crate::services::cluster::intake_router_hook::{
        IntakeRouterContext, IntakeRouterDecision, try_route_intake,
    };
    use crate::services::cluster::intake_routing_config::IntakeRoutingMode;
    let channel = channel.to_string();
    let ctx = IntakeRouterContext {
        mode: IntakeRoutingMode::Disabled,
        leader_instance_id: "leader-1",
        provider: "claude",
        channel_id: &channel,
        policy_channel_id: &channel,
        user_msg_id: "9999",
        request_owner_id: "100",
        request_owner_name: Some("Tester"),
        user_text: "hello",
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "foreground",
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        node_override_instance_id: None,
        has_nonportable_uploads: false,
        attachment_refs: &[],
    };
    match try_route_intake(pool, &ctx).await {
        IntakeRouterDecision::RanLocal { .. } => true,
        IntakeRouterDecision::Blocked { .. } => false,
        other => panic!("unexpected decision {other:?}"),
    }
}

fn accept_attempted(outcome: Result<TickOutcome, sqlx::Error>) -> bool {
    matches!(outcome, Err(error) if error.to_string().contains("accept attempted"))
}

#[tokio::test(flavor = "current_thread")]
async fn a_canary_row_runs_only_on_its_ready_gateway_while_a_legacy_row_runs_anywhere_pg() {
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
    use crate::services::tui_o::writer::host::{self, HostParts, test_io::TestHost};
    if !in_own_process(
        "a_canary_row_runs_only_on_its_ready_gateway_while_a_legacy_row_runs_anywhere_pg",
    ) {
        return;
    }
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('agent-o', 'Test', 'claude', 'unused')")
        .execute(&pool)
        .await
        .unwrap();
    refuse_accepts(&pool).await;
    let owner = crate::services::discord::health::owner_runtime_for_tests::registered("claude");
    let (_registry, shared) = owner.await;
    let not_cancelled = || false;
    let tick = || run_intake_worker_tick(&pool, &shared, "worker-1", "claude", "o", &not_cancelled);
    let _selected = test_override::force_channels(&[(O, ClaudeTui)]);

    // No writer is hosted here, as on a runner: the Legacy row runs, the canary row waits.
    let canary = seed(&pool, O, 1).await;
    seed(&pool, LEGACY, 2).await;
    assert!(!routed_locally(&pool, O).await && routed_locally(&pool, LEGACY).await);
    assert!(accept_attempted(tick().await), "the Legacy row is accepted");
    assert_eq!(tick().await.unwrap(), TickOutcome::QueueEmpty);
    assert_eq!(state(&pool, canary).await, pending());

    // The gateway hosts the canary's writer; it takes the row only once Owned and ready.
    let runtime = tempfile::tempdir().unwrap();
    let transcript = runtime.path().join("canary.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let io = TestHost::new([(O, source_id_for("e2e", &transcript).unwrap())]);
    let gate = Arc::new(OwnershipGate::default());
    let parts = || HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(runtime.path().to_path_buf()),
        gate: Arc::clone(&gate),
        readiness: host::process_readiness(),
    };
    let _hosts = host::start(ShadowProvider::Claude, true, parts);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!routed_locally(&pool, O).await, "not Owned yet");
    assert_eq!(tick().await.unwrap(), TickOutcome::QueueEmpty);
    gate.acquired();
    for _ in 0..100 {
        if host::channel_accepts(O) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(routed_locally(&pool, O).await, "the ready gateway runs it");
    assert!(accept_attempted(tick().await), "the canary row is accepted");

    // One open route per channel: clear the accepted row before the next message.
    sqlx::query("DELETE FROM intake_outbox WHERE id = $1")
        .bind(canary)
        .execute(&pool)
        .await
        .unwrap();
    gate.lost();
    let later = seed(&pool, O, 3).await;
    assert!(!routed_locally(&pool, O).await, "a lost gateway holds it");
    assert_eq!(tick().await.unwrap(), TickOutcome::QueueEmpty);
    assert_eq!(state(&pool, later).await, pending());
    assert_eq!(*io.alarms.0.lock().unwrap(), []);

    pool.close().await;
    fixture.drop().await;
}
