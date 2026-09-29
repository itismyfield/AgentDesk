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
