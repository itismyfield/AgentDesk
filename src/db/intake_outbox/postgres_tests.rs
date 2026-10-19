use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox_force_fail::{ForceFailError, force_fail_and_retry_as_new};
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use serde_json::json;

fn payload(channel: &str, msg: &str) -> InsertPendingPayload {
    InsertPendingPayload {
        execution_requirements: json!({}),
        attachment_refs: serde_json::json!([]),
        target_instance_id: "worker-1".to_string(),
        forwarded_by_instance_id: "leader-1".to_string(),
        provider: "claude".to_string(),
        required_labels: json!(["unreal"]),
        channel_id: channel.to_string(),
        user_msg_id: msg.to_string(),
        replay_source_message_ids: None,
        request_owner_id: "user-100".to_string(),
        request_owner_name: Some("Tester".to_string()),
        user_text: "hello world".to_string(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "standard".to_string(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "agent-x".to_string(),
        home_epoch: None,
    }
}

/// Idempotently seed `agent-x` with provider='claude' so the
/// `claim_pending_for_target` provider-JOIN finds a match. Phase 5
/// codex P0 #1: all helper tests that exercise the claim path call
/// this once near the top.
async fn seed_default_test_agent(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-x', 'Test', 'claude', 'unused-channel')
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed default test agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_pending_round_trips_payload_and_returns_id() -> Result<(), sqlx::Error> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let id = insert_pending(&pool, &payload("ch-1", "msg-1"), 1, None)
        .await
        .expect("insert pending");
    assert!(id > 0);

    let row: IntakeOutboxRow = sqlx::query_as("SELECT * FROM intake_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch row");
    assert_eq!(row.channel_id, "ch-1");
    assert_eq!(row.user_msg_id, "msg-1");
    assert_eq!(row.attempt_no, 1);
    assert_eq!(row.status, IntakeOutboxStatus::Pending);
    assert!(row.parent_outbox_id.is_none());
    assert_eq!(row.required_labels, json!(["unreal"]));
    assert_eq!(row.preserve_on_cancel, Some(false));

    for status in IntakeOutboxStatus::ALL {
        let (raw_status, typed_status): (String, IntakeOutboxStatus) =
            match sqlx::query_as("SELECT $1::TEXT, $1::TEXT")
                .bind(status)
                .fetch_one(&pool)
                .await
            {
                Ok(decoded) => decoded,
                Err(error) => {
                    pool.close().await;
                    pg_db.drop().await;
                    return Err(error);
                }
            };
        assert_eq!(raw_status, status.as_str());
        assert_eq!(typed_status, status);
    }

    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preserve_on_cancel_round_trips_true_false_and_legacy_null() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let mut human = payload("ch-preserve-human", "msg-human");
    human.preserve_on_cancel = true;
    let human_id = insert_pending(&pool, &human, 1, None)
        .await
        .expect("insert human row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] postgres module
    let automation_id = insert_pending(
        &pool,
        &payload("ch-preserve-automation", "msg-automation"),
        1,
        None,
    )
    .await
    .expect("insert automation row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] postgres module
    let legacy_id: i64 = sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, required_labels,
            channel_id, user_msg_id, request_owner_id, user_text, turn_kind,
            agent_id, provider, status, attempt_no
         ) VALUES (
            'worker-1', 'legacy-leader', '[]'::JSONB,
            'ch-preserve-legacy', 'msg-legacy', 'user-legacy', 'legacy', 'standard',
            'agent-x', 'claude', 'pending', 1
         ) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert legacy row without preserve_on_cancel"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] postgres module

    for (id, expected) in [
        (human_id, Some(true)),
        (automation_id, Some(false)),
        (legacy_id, None),
    ] {
        let row: IntakeOutboxRow = sqlx::query_as("SELECT * FROM intake_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read preservation row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] postgres module
        assert_eq!(row.preserve_on_cancel, expected);
    }

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn family_max_attempt_returns_zero_when_no_rows() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let max = family_max_attempt(&pool, "ch-empty", "msg-empty")
        .await
        .expect("read max");
    assert_eq!(max, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn family_max_attempt_tracks_highest_attempt_in_family() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    // Drive first attempt to terminal so we can stack a second.
    let id1 = insert_pending(&pool, &payload("ch-fam", "msg-fam"), 1, None)
        .await
        .expect("seed attempt 1");
    sqlx::query("UPDATE intake_outbox SET status='failed_pre_accept' WHERE id=$1")
        .bind(id1)
        .execute(&pool)
        .await
        .expect("terminate attempt 1");

    let id2 = insert_pending(&pool, &payload("ch-fam", "msg-fam"), 2, Some(id1))
        .await
        .expect("seed attempt 2");
    assert!(id2 > id1);

    let max = family_max_attempt(&pool, "ch-fam", "msg-fam")
        .await
        .expect("read max");
    assert_eq!(max, 2);

    pool.close().await;
    pg_db.drop().await;
}

/// Parameter types of each claim candidate statement the server prepared on the connection.
async fn prepared_claim_parameters(pool: &PgPool) -> Vec<Vec<String>> {
    let mut prepared: Vec<Vec<String>> = sqlx::query_scalar(
        "SELECT parameter_types::TEXT[] FROM pg_prepared_statements
         WHERE statement LIKE '%SKIP LOCKED%' AND statement NOT LIKE '%pg_prepared%'",
    )
    .fetch_all(pool)
    .await
    .expect("read prepared statements");
    prepared.sort();
    prepared
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_holding_no_channel_runs_the_original_two_parameter_statement() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(1).await;
    seed_default_test_agent(&pool).await;
    let older = insert_pending(&pool, &payload("ch-held", "msg-A"), 1, None)
        .await
        .expect("seed row 1");
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let newer = insert_pending(&pool, &payload("ch-open", "msg-B"), 1, None)
        .await
        .expect("seed row 2");
    let (text, texts) = (["text", "text"], ["text", "text", "text[]"]);

    let held = ["ch-held".to_string()];
    let claimed = claim_pending_for_target_except(&pool, "worker-1", "claude", "o", &held);
    assert_eq!(claimed.await.expect("claim").map(|row| row.id), Some(newer));
    assert_eq!(prepared_claim_parameters(&pool).await, [texts]);
    let claimed = claim_pending_for_target_except(&pool, "worker-1", "claude", "o", &[]);
    assert_eq!(claimed.await.expect("claim").map(|row| row.id), Some(older));
    assert_eq!(
        prepared_claim_parameters(&pool).await,
        [text.as_slice(), texts.as_slice()],
        "with nothing held the original statement runs with its two bindings"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_pending_for_target_picks_oldest_pending_and_promotes_to_claimed() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let id1 = insert_pending(&pool, &payload("ch-claim-1", "msg-A"), 1, None)
        .await
        .expect("seed row 1");
    // Slight delay so the second row's created_at is strictly later.
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let _id2 = insert_pending(&pool, &payload("ch-claim-2", "msg-B"), 1, None)
        .await
        .expect("seed row 2");

    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "worker-1.local")
        .await
        .expect("claim")
        .expect("must claim something");
    assert_eq!(claimed.id, id1);
    assert_eq!(claimed.status, IntakeOutboxStatus::Claimed);
    assert_eq!(claimed.claim_owner.as_deref(), Some("worker-1.local"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_pending_for_target_returns_none_when_queue_empty() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "worker-1.local")
        .await
        .expect("claim ok");
    assert!(claimed.is_none());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_pending_for_target_filters_by_target_instance_id() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let mut other = payload("ch-other", "msg-other");
    other.target_instance_id = "worker-OTHER".to_string();
    insert_pending(&pool, &other, 1, None)
        .await
        .expect("seed for other worker");

    // Worker-1 polls — must NOT receive the row destined for worker-OTHER.
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "worker-1.local")
        .await
        .expect("claim ok");
    assert!(claimed.is_none(), "must not steal cross-target rows");

    // worker-OTHER polls — picks it up.
    let claimed = claim_pending_for_target(&pool, "worker-OTHER", "claude", "worker-OTHER.local")
        .await
        .expect("claim ok")
        .expect("worker-OTHER must claim");
    assert_eq!(claimed.target_instance_id, "worker-OTHER");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_state_machine_pending_to_done_advances_correctly() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let _id = insert_pending(&pool, &payload("ch-full", "msg-full"), 1, None)
        .await
        .expect("insert pending");
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim")
        .expect("row");
    assert_eq!(claimed.status, IntakeOutboxStatus::Claimed);

    let advanced = mark_accepted(&pool, claimed.id, "owner-1")
        .await
        .expect("mark_accepted");
    assert!(advanced, "claimed→accepted must advance");
    let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(claimed.id)
        .fetch_one(&pool)
        .await
        .expect("read status after accept");
    assert_eq!(status, "accepted");

    let advanced = mark_spawned(&pool, claimed.id, "owner-1")
        .await
        .expect("mark_spawned");
    assert!(advanced, "accepted→spawned must advance");
    let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(claimed.id)
        .fetch_one(&pool)
        .await
        .expect("read status after spawn");
    assert_eq!(status, "spawned");

    let advanced = mark_done(&pool, claimed.id, "owner-1")
        .await
        .expect("mark_done");
    assert!(advanced, "spawned→done must advance");
    let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(claimed.id)
        .fetch_one(&pool)
        .await
        .expect("read status after done");
    assert_eq!(status, "done");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_accepted_rejects_wrong_claim_owner() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    insert_pending(&pool, &payload("ch-owner", "msg-owner"), 1, None)
        .await
        .expect("insert");
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-A")
        .await
        .expect("claim")
        .expect("row");

    // Wrong owner — must be a no-op AND `Ok(false)`.
    let advanced = mark_accepted(&pool, claimed.id, "owner-WRONG")
        .await
        .expect("mark_accepted ok (no-op for wrong owner)");
    assert!(!advanced, "wrong owner must NOT advance state");
    let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(claimed.id)
        .fetch_one(&pool)
        .await
        .expect("read status");
    assert_eq!(status, "claimed", "wrong owner must NOT advance state");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_owned_claim_returns_exact_row_to_pending_without_failure_pollution() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    insert_pending(&pool, &payload("ch-cancel-owned", "msg-owned"), 1, None)
        .await
        .expect("insert owned row"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    let owned = claim_pending_for_target(&pool, "worker-1", "claude", "owner-cancelled")
        .await
        .expect("claim owned row") // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
        .expect("owned row"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    insert_pending(&pool, &payload("ch-cancel-other", "msg-other"), 1, None)
        .await
        .expect("insert other row"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    let other = claim_pending_for_target(&pool, "worker-1", "claude", "owner-other")
        .await
        .expect("claim other row") // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
        .expect("other row"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    assert!(
        !return_claimed_to_pending(&pool, owned.id, "owner-wrong")
            .await
            .expect("wrong owner is a no-op"), // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
        "a different worker must not release this claim"
    );
    assert!(
        return_claimed_to_pending(&pool, owned.id, "owner-cancelled")
            .await
            .expect("release owned claim"), // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
        "the cancelling worker must return its exact row to pending"
    );

    let released: (String, Option<String>, bool, i32, Option<String>, bool) = sqlx::query_as(
        "SELECT status, claim_owner, claimed_at IS NULL, retry_count, last_error,
                    completed_at IS NULL
             FROM intake_outbox WHERE id = $1",
    )
    .bind(owned.id)
    .fetch_one(&pool)
    .await
    .expect("read released row"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    assert_eq!(released.0, "pending");
    assert_eq!(released.1, None);
    assert!(released.2, "claimed_at must be cleared");
    assert_eq!(released.3, 0, "restart cancellation is not a retry");
    assert_eq!(released.4, None, "restart cancellation is not an error");
    assert!(released.5, "restart cancellation is not completion");

    let other_state: (String, Option<String>) =
        sqlx::query_as("SELECT status, claim_owner FROM intake_outbox WHERE id = $1")
            .bind(other.id)
            .fetch_one(&pool)
            .await
            .expect("read other row"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    assert_eq!(other_state.0, "claimed", "another row must not be reset");
    assert_eq!(other_state.1.as_deref(), Some("owner-other"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_5057_pr1_skips_oldest_unsupported_family_for_capable_family() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id,status,labels,capabilities,last_heartbeat_at)
         VALUES ('worker-codex','online','[\"unreal\"]',$1,NOW())",
    )
    .bind(json!({"intake_worker":{"enabled":true,"providers":["codex"]}}))
    .execute(&pool)
    .await
    .expect("seed codex-only worker"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    let unsupported_id = insert_pending(&pool, &payload("ch-old", "msg-old"), 1, None)
        .await
        .expect("seed unsupported family"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    sqlx::query(
        "UPDATE intake_outbox
            SET status='failed_pre_accept', admission_kind='local', provider='claude', last_error='unsupported',
                updated_at=NOW()-INTERVAL '2 hours'
          WHERE id=$1",
    )
    .bind(unsupported_id)
    .execute(&pool)
    .await
    .expect("fail unsupported family"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    let mut capable = payload("ch-new", "msg-new");
    capable.provider = "codex".to_string();
    let capable_id = insert_pending(&pool, &capable, 1, None)
        .await
        .expect("seed capable family"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    sqlx::query(
        "UPDATE intake_outbox
            SET status='failed_pre_accept', admission_kind='local', last_error='retryable',
                updated_at=NOW()-INTERVAL '1 hour'
          WHERE id=$1",
    )
    .bind(capable_id)
    .execute(&pool)
    .await
    .expect("fail capable family"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    let outcome = sweep_failed_pre_accept_once(&pool, "leader-1", 5, 60, None)
        .await
        .expect("sweep capable family"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    let FailedPreAcceptSweepOutcome::Retried {
        source_id,
        new_id,
        attempt_no,
    } = outcome
    else {
        panic!("expected capable retry, got {outcome:?}"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    };
    assert_eq!(source_id, capable_id);
    assert_eq!(attempt_no, 2);
    let child: (String, i32, Option<i64>) = sqlx::query_as(
        "SELECT target_instance_id,attempt_no,parent_outbox_id FROM intake_outbox WHERE id=$1",
    )
    .bind(new_id)
    .fetch_one(&pool)
    .await
    .expect("read child"); // agentdesk-audit: allow-unwrap — test assertion
    assert_eq!(child, ("worker-codex".into(), 2, Some(capable_id)));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_5057_pr1_capability_loss_rotates_source_then_next_tick_progresses() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id,status,labels,capabilities,last_heartbeat_at)
         VALUES ('worker-shared','online','[\"unreal\"]',$1,NOW())",
    )
    .bind(json!({
        "intake_worker": {
            "enabled": true,
            "providers": ["claude", "codex"]
        }
    }))
    .execute(&pool)
    .await
    .expect("seed initially capable worker"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    let rotated_id = insert_pending(&pool, &payload("ch-rotate", "msg-rotate"), 1, None)
        .await
        .expect("seed family whose capability disappears"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    sqlx::query(
        "UPDATE intake_outbox
            SET status='failed_pre_accept', admission_kind='local', last_error='preserve-me',
                updated_at=NOW()-INTERVAL '2 hours'
          WHERE id=$1",
    )
    .bind(rotated_id)
    .execute(&pool)
    .await
    .expect("fail family whose capability disappears"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    let mut later = payload("ch-later", "msg-later");
    later.provider = "codex".to_string();
    let later_id = insert_pending(&pool, &later, 1, None)
        .await
        .expect("seed later capable family"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    sqlx::query(
        "UPDATE intake_outbox
            SET status='failed_pre_accept', admission_kind='local', last_error='later',
                updated_at=NOW()-INTERVAL '1 hour'
          WHERE id=$1",
    )
    .bind(later_id)
    .execute(&pool)
    .await
    .expect("fail later capable family"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    let before: (String, Option<String>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT status,last_error,updated_at FROM intake_outbox WHERE id=$1")
            .bind(rotated_id)
            .fetch_one(&pool)
            .await
            .expect("read source before capability race"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    // Hold an uncommitted worker snapshot update. The sweep's first statement
    // sees the prior capable version, then its FOR SHARE re-check waits here.
    let mut capability_update = pool.begin().await.expect("begin capability update"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    let capability_update_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *capability_update)
        .await
        .expect("read capability update backend pid"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    sqlx::query(
        "UPDATE worker_nodes
            SET capabilities=$1, last_heartbeat_at=NOW()
          WHERE instance_id='worker-shared'",
    )
    .bind(json!({"intake_worker":{"enabled":true,"providers":["codex"]}}))
    .execute(&mut *capability_update)
    .await
    .expect("remove claude capability without committing"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module

    let sweep_pool = pool.clone();
    let sweep = tokio::spawn(async move {
        sweep_failed_pre_accept_once(&sweep_pool, "leader-1", 5, 60, None).await
    });
    // The old FOR UPDATE NOWAIT probe could lock rotated_id, making the sweep's
    // FOR UPDATE ... SKIP LOCKED pick later_id. Observe the held worker row instead.
    let wait_for_worker_lock = async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                     WHERE $1 = ANY(pg_blocking_pids(pid))
                 )",
            )
            .bind(capability_update_pid)
            .fetch_one(&pool)
            .await
            .expect("observe sweep waiting on capability update"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(10), wait_for_worker_lock)
        .await
        .is_err()
    {
        capability_update
            .rollback()
            .await
            .expect("release worker after missing lock wait"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
        let sweep_result = sweep.await.expect("join sweep after missing lock wait"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
        panic!("sweep never waited for capability update; result={sweep_result:?}"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    }
    capability_update
        .commit()
        .await
        .expect("publish capability loss"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    assert_eq!(
        sweep
            .await
            .expect("join capability-race sweep") // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
            .expect("sweep"), // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
        FailedPreAcceptSweepOutcome::NoCapableTarget {
            source_id: rotated_id
        }
    );
    let after: (String, Option<String>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT status,last_error,updated_at FROM intake_outbox WHERE id=$1")
            .bind(rotated_id)
            .fetch_one(&pool)
            .await
            .expect("read rotated source"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    assert_eq!(after.0, before.0, "terminal status must be preserved");
    assert_eq!(after.1, before.1, "failure detail must be preserved");
    assert!(
        after.2 > before.2,
        "fairness rotation must advance updated_at"
    );

    let next = sweep_failed_pre_accept_once(&pool, "leader-1", 5, 60, None)
        .await
        .expect("run next sweep tick"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] postgres module
    assert!(
        matches!(next, FailedPreAcceptSweepOutcome::Retried { source_id, .. } if source_id == later_id),
        "next tick must progress the later capable family, got {next:?}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_5057_pr1_leader_maintenance_inputs_route_to_current_owner() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id,status,labels,capabilities,last_heartbeat_at)
         VALUES ('stale-worker','online','[\"unreal\"]',$1,NOW()-INTERVAL '30 seconds'),
                ('fresh-owner','online','[\"unreal\"]',$1,NOW()-INTERVAL '2 seconds')",
    )
    .bind(json!({"intake_worker":{"enabled":true,"providers":["claude"]}}))
    .execute(&pool)
    .await
    .expect("seed maintenance workers"); // agentdesk-audit: allow-unwrap — PG production-entry regression
    let source = insert_pending(&pool, &payload("ch-entry", "msg-entry"), 1, None)
        .await
        .expect("production insert"); // agentdesk-audit: allow-unwrap — PG production-entry regression
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "legacy-worker")
        .await
        .expect("legacy claim") // agentdesk-audit: allow-unwrap — PG production-entry regression
        .expect("claim row"); // agentdesk-audit: allow-unwrap — PG production-entry regression
    assert!(
        mark_failed_pre_accept(&pool, claimed.id, "legacy-worker", "retryable")
            .await
            .expect("fail pre-accept") // agentdesk-audit: allow-unwrap — PG production-entry regression
    );
    assert_eq!(
        crate::services::cluster::node_registry::run_leader_intake_retry_maintenance_once(
            &pool,
            "leader-1",
            10,
            5,
            || None
        )
        .await
        .expect("observe maintenance"), // agentdesk-audit: allow-unwrap — GC-without-retry regression
        None
    );
    let observe: (String, String, i64) = sqlx::query_as("SELECT (SELECT status FROM worker_nodes WHERE instance_id='stale-worker'),status,(SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id=$1) FROM intake_outbox WHERE id=$1")
        .bind(source).fetch_one(&pool).await.expect("observe state"); // agentdesk-audit: allow-unwrap — enforce gate regression
    assert_eq!(observe, ("offline".into(), "failed_pre_accept".into(), 0));
    let outcome =
        crate::services::cluster::node_registry::run_leader_intake_retry_maintenance_once(
            &pool,
            "leader-1",
            10,
            5,
            || Some((5, 300)),
        )
        .await
        .expect("maintenance") // agentdesk-audit: allow-unwrap — PG production-entry regression
        .unwrap(); // agentdesk-audit: allow-unwrap — PG production-entry regression
    sqlx::query("INSERT INTO intake_session_owners (provider,raw_channel_id,owner_instance_id,generation,status) VALUES ('claude','ch-entry','fresh-owner',3,'active')")
        .execute(&pool).await.expect("seed active owner"); // agentdesk-audit: allow-unwrap — owner-transfer regression

    let FailedPreAcceptSweepOutcome::Retried { new_id, .. } = outcome else {
        panic!("retry required") // agentdesk-audit: allow-unwrap — PG production-entry regression
    };
    let child: (String, String, Option<String>, Option<i64>) = sqlx::query_as("SELECT target_instance_id,admission_kind,owner_instance_id,owner_generation FROM intake_outbox WHERE id=$1")
        .bind(new_id).fetch_one(&pool).await.expect("read child"); // agentdesk-audit: allow-unwrap — PG production-entry regression
    assert_eq!(child.0, "fresh-owner");
    assert_eq!(
        (child.1, child.2, child.3),
        ("forwarded".into(), None, None)
    );
    assert!(
        claim_pending_for_target(&pool, "fresh-owner", "claude", "legacy-child")
            .await
            .expect("claim retry child") // agentdesk-audit: allow-unwrap — production worker compatibility
            .is_some()
    );
    let stale: String =
        sqlx::query_scalar("SELECT status FROM worker_nodes WHERE instance_id='stale-worker'")
            .fetch_one(&pool)
            .await
            .expect("read stale"); // agentdesk-audit: allow-unwrap — PG production-entry regression
    assert_eq!(stale, "offline");
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(new_id)
        .execute(&pool)
        .await
        .expect("close child"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    let race_id = insert_pending(&pool, &payload("ch-entry", "msg-race"), 1, None)
        .await
        .expect("race source"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    sqlx::query("UPDATE intake_outbox SET status='failed_pre_accept',admission_kind='forwarded',owner_instance_id='fresh-owner',owner_generation=NULL,completed_at=NOW() WHERE id=$1").bind(race_id).execute(&pool).await.expect("partial stamp"); // agentdesk-audit: allow-unwrap — malformed owner regression
    assert_eq!(
        sweep_failed_pre_accept_once(&pool, "leader", 5, 5, Some(300))
            .await
            .expect("partial sweep"), // agentdesk-audit: allow-unwrap — malformed owner regression
        FailedPreAcceptSweepOutcome::NoCapableTarget { source_id: race_id }
    );
    sqlx::query("UPDATE intake_outbox SET owner_generation=3 WHERE id=$1")
        .bind(race_id)
        .execute(&pool)
        .await
        .expect("complete stamp"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    let identity = crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new(
        "claude", "ch-entry",
    );
    let mut transfer = pool.begin().await.expect("begin transfer"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *transfer)
        .await
        .expect("lock transfer"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    crate::services::cluster::intake_router_hook::owner_record::transfer_owner_in_tx(
        &mut transfer,
        &identity,
        "fresh-owner",
        3,
        "stale-worker",
    )
    .await
    .expect("transfer"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    let race_pool = pool.clone();
    let race = tokio::spawn(async move {
        sweep_failed_pre_accept_once(&race_pool, "leader", 5, 5, Some(300)).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!race.is_finished());
    transfer.commit().await.expect("commit transfer"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    assert_eq!(
        race.await.expect("join").expect("sweep"), // agentdesk-audit: allow-unwrap — owner-transfer regression
        FailedPreAcceptSweepOutcome::NoCapableTarget { source_id: race_id }
    ); // agentdesk-audit: allow-unwrap — owner-transfer regression
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id=$1"
        )
        .bind(race_id)
        .fetch_one(&pool)
        .await
        .expect("children"), // agentdesk-audit: allow-unwrap — owner-transfer regression
        0
    ); // agentdesk-audit: allow-unwrap — owner-transfer regression
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(race_id)
        .execute(&pool)
        .await
        .expect("close race"); // agentdesk-audit: allow-unwrap — owner-transfer regression
    let expired = insert_pending(&pool, &payload("ch-expired", "msg-expired"), 1, None)
        .await
        .expect("seed expired source"); // agentdesk-audit: allow-unwrap — PG production-entry regression
    sqlx::query("UPDATE intake_outbox SET status='failed_pre_accept',admission_kind='local',completed_at=NOW()-INTERVAL '301 seconds' WHERE id=$1")
        .bind(expired).execute(&pool).await.expect("expire source"); // agentdesk-audit: allow-unwrap — PG production-entry regression
    assert_eq!(
        sweep_failed_pre_accept_once(&pool, "leader-1", 5, 5, Some(300))
            .await
            .expect("bounded sweep"), // agentdesk-audit: allow-unwrap — PG production-entry regression
        FailedPreAcceptSweepOutcome::Empty
    );
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_accepted_returns_false_when_sweep_already_reset_the_claim() {
    // Race: leader sweep reset the row to `pending` between claim
    // and accept. Worker MUST see `Ok(false)` so it can abort
    // instead of spawning a turn behind a no-longer-owned row.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    insert_pending(&pool, &payload("ch-race", "msg-race"), 1, None)
        .await
        .expect("insert");
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-fast")
        .await
        .expect("claim")
        .expect("row");

    // Simulate sweep racing in: revert to pending.
    sqlx::query(
        "UPDATE intake_outbox SET status='pending', claim_owner=NULL, claimed_at=NULL
         WHERE id = $1",
    )
    .bind(claimed.id)
    .execute(&pool)
    .await
    .expect("simulate sweep");

    let advanced = mark_accepted(&pool, claimed.id, "owner-fast")
        .await
        .expect("mark_accepted ok");
    assert!(
        !advanced,
        "lost claim must report Ok(false) so worker aborts"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_insert_pending_error_distinguishes_the_two_unique_violations() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    // First INSERT — succeeds. Drive it terminal so the partial
    // unique index does not mask the 3-tuple violation we want.
    let id = insert_pending(&pool, &payload("ch-classify", "msg-classify"), 1, None)
        .await
        .expect("first insert");
    sqlx::query("UPDATE intake_outbox SET status='done', completed_at=NOW() WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("terminate");

    // Duplicate (channel_id, user_msg_id, attempt_no) — must trip
    // the 3-tuple constraint.
    let dup_err = insert_pending(&pool, &payload("ch-classify", "msg-classify"), 1, None)
        .await
        .expect_err("dup attempt_no must reject");
    assert_eq!(
        classify_insert_pending_error(&dup_err),
        Some(IntakeInsertConflict::DuplicateMessageAttempt)
    );

    // OPEN-route violation: insert a fresh OPEN row, then try a
    // SECOND distinct user_msg_id on the same channel.
    let _open_id = insert_pending(&pool, &payload("ch-open", "msg-A"), 1, None)
        .await
        .expect("first open insert");
    let open_err = insert_pending(&pool, &payload("ch-open", "msg-B"), 1, None)
        .await
        .expect_err("second OPEN row must reject");
    assert_eq!(
        classify_insert_pending_error(&open_err),
        Some(IntakeInsertConflict::OpenRoutePerChannel)
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_failed_pre_accept_records_error_and_bumps_retry_count() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    insert_pending(&pool, &payload("ch-fail", "msg-fail"), 1, None)
        .await
        .expect("insert");
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim")
        .expect("row");

    mark_failed_pre_accept(&pool, claimed.id, "owner-1", "cwd validation failed")
        .await
        .expect("mark_failed_pre_accept");

    let row: IntakeOutboxRow = sqlx::query_as("SELECT * FROM intake_outbox WHERE id = $1")
        .bind(claimed.id)
        .fetch_one(&pool)
        .await
        .expect("read row");
    assert_eq!(row.status, IntakeOutboxStatus::FailedPreAccept);
    assert_eq!(row.retry_count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_failed_post_accept_works_from_accepted_or_spawned() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    // Setup A: fail from accepted.
    insert_pending(&pool, &payload("ch-postA", "msg-A"), 1, None)
        .await
        .expect("insert A");
    let row_a = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim A")
        .expect("row A");
    mark_accepted(&pool, row_a.id, "owner-1")
        .await
        .expect("accept A");
    mark_failed_post_accept(&pool, row_a.id, "owner-1", "spawned crash")
        .await
        .expect("fail from accepted");
    let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(row_a.id)
        .fetch_one(&pool)
        .await
        .expect("read");
    assert_eq!(status, "failed_post_accept");

    // Setup B: fail from spawned.
    insert_pending(&pool, &payload("ch-postB", "msg-B"), 1, None)
        .await
        .expect("insert B");
    let row_b = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim B")
        .expect("row B");
    mark_accepted(&pool, row_b.id, "owner-1")
        .await
        .expect("accept B");
    mark_spawned(&pool, row_b.id, "owner-1")
        .await
        .expect("spawn B");
    mark_failed_post_accept(&pool, row_b.id, "owner-1", "tmux died")
        .await
        .expect("fail from spawned");
    let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(row_b.id)
        .fetch_one(&pool)
        .await
        .expect("read");
    assert_eq!(status, "failed_post_accept");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_stale_pre_accept_claims_resets_only_stale_rows() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    // Insert + claim one row, then push claimed_at backwards to simulate
    // staleness. A SECOND row is claimed fresh and must NOT be reset.
    insert_pending(&pool, &payload("ch-stale", "msg-stale"), 1, None)
        .await
        .expect("insert stale");
    let stale = claim_pending_for_target(&pool, "worker-1", "claude", "owner-died")
        .await
        .expect("claim stale")
        .expect("row stale");
    sqlx::query(
        "UPDATE intake_outbox SET claimed_at = NOW() - INTERVAL '300 seconds' WHERE id = $1",
    )
    .bind(stale.id)
    .execute(&pool)
    .await
    .expect("backdate claimed_at");

    insert_pending(&pool, &payload("ch-fresh", "msg-fresh"), 1, None)
        .await
        .expect("insert fresh");
    let fresh = claim_pending_for_target(&pool, "worker-1", "claude", "owner-alive")
        .await
        .expect("claim fresh")
        .expect("row fresh");

    let reset = sweep_stale_pre_accept_claims(&pool, 60)
        .await
        .expect("sweep");
    assert_eq!(reset, 1, "only the stale row should reset");

    let stale_status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(stale.id)
        .fetch_one(&pool)
        .await
        .expect("read stale");
    assert_eq!(stale_status, "pending");

    let fresh_status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
        .bind(fresh.id)
        .fetch_one(&pool)
        .await
        .expect("read fresh");
    assert_eq!(fresh_status, "claimed", "fresh claim must not reset");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_accepted_unspawned_sla_returns_only_overdue_rows() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    // Stale: accepted long ago, never spawned.
    insert_pending(&pool, &payload("ch-sla1", "msg-sla1"), 1, None)
        .await
        .expect("insert sla1");
    let row1 = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim")
        .expect("row1");
    mark_accepted(&pool, row1.id, "owner-1")
        .await
        .expect("accept");
    sqlx::query(
        "UPDATE intake_outbox SET accepted_at = NOW() - INTERVAL '600 seconds' WHERE id = $1",
    )
    .bind(row1.id)
    .execute(&pool)
    .await
    .expect("backdate accepted_at");

    // Fresh accepted row — should NOT be flagged.
    insert_pending(&pool, &payload("ch-sla2", "msg-sla2"), 1, None)
        .await
        .expect("insert sla2");
    let row2 = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim")
        .expect("row2");
    mark_accepted(&pool, row2.id, "owner-1")
        .await
        .expect("accept");

    let stale = list_accepted_unspawned_sla(&pool, 60).await.expect("list");
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].0, row1.id);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_fail_and_retry_as_new_terminates_stuck_row_and_inserts_attempt_2() {
    // Phase 5 transition 12: a row stuck in 'spawned' (worker hung
    // mid-turn) is force-failed by the operator, and a fresh row
    // is inserted with attempt_no=2 + parent_outbox_id pointing
    // at the stuck row. Both writes happen in one transaction so
    // the partial unique index never sees both rows OPEN.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let mut retry_payload = payload("ch-stuck", "msg-stuck");
    retry_payload.preserve_on_cancel = true;
    insert_pending(&pool, &retry_payload, 1, None)
        .await
        .expect("seed");
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim")
        .expect("row");
    mark_accepted(&pool, claimed.id, "owner-1")
        .await
        .expect("accept");
    mark_spawned(&pool, claimed.id, "owner-1")
        .await
        .expect("spawn");

    let new_id = force_fail_and_retry_as_new(&pool, claimed.id, "operator: tmux pane crashed")
        .await
        .expect("force-fail");

    // Stuck row → failed_post_accept with operator reason embedded.
    let stuck: (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM intake_outbox WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .expect("read stuck row");
    assert_eq!(stuck.0, "failed_post_accept");
    let last_error = stuck.1.expect("last_error must be set");
    assert!(
        last_error.contains("operator force-fail (was: spawned)")
            && last_error.contains("tmux pane crashed"),
        "last_error must record original status + operator reason: {last_error}"
    );

    // New row → pending, attempt_no=2, parent points at stuck.
    let new_row: (String, i32, Option<i64>, String, String, Option<bool>) = sqlx::query_as(
        "SELECT status, attempt_no, parent_outbox_id, channel_id, user_msg_id,
                    preserve_on_cancel
             FROM intake_outbox WHERE id = $1",
    )
    .bind(new_id)
    .fetch_one(&pool)
    .await
    .expect("read new row");
    assert_eq!(new_row.0, "pending");
    assert_eq!(new_row.1, 2);
    assert_eq!(new_row.2, Some(claimed.id));
    assert_eq!(new_row.3, "ch-stuck");
    assert_eq!(new_row.4, "msg-stuck");
    assert_eq!(new_row.5, Some(true));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_fail_and_retry_as_new_on_already_terminal_row_just_appends_attempt() {
    // Preserve the original failed-post-accept regression proof alongside
    // the new official-unknown proof below: both are terminal evidence.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    insert_pending(&pool, &payload("ch-terminal", "msg-terminal"), 1, None)
        .await
        .expect("seed"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim") // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        .expect("row"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    mark_accepted(&pool, claimed.id, "owner-1")
        .await
        .expect("accept"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    mark_failed_post_accept(&pool, claimed.id, "owner-1", "original failure")
        .await
        .expect("fail post-accept"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

    let new_id = force_fail_and_retry_as_new(&pool, claimed.id, "operator: retry approved")
        .await
        .expect("force-fail"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    let source: (IntakeOutboxStatus, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM intake_outbox WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .expect("read failed-post-accept source"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    assert_eq!(
        source,
        (
            IntakeOutboxStatus::FailedPostAccept,
            Some("original failure".to_string()),
        ),
        "retry must preserve already-terminal source evidence"
    );

    let child: (IntakeOutboxStatus, i32, Option<i64>) = sqlx::query_as(
        "SELECT status, attempt_no, parent_outbox_id FROM intake_outbox WHERE id = $1",
    )
    .bind(new_id)
    .fetch_one(&pool)
    .await
    .expect("read failed-post-accept child attempt"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    assert_eq!(
        child,
        (IntakeOutboxStatus::Pending, 2, Some(claimed.id)),
        "retry must append one pending child"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_fail_and_retry_as_new_preserves_unknown_source_and_appends_pending_attempt()
-> Result<(), ForceFailError> {
    // Official unknown is terminal evidence, not a failure alias. Operator
    // retry appends a child but must not rewrite the source status/error.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    insert_pending(
        &pool,
        &payload("ch-unknown-retry", "msg-unknown-retry"),
        1,
        None,
    )
    .await
    .expect("seed"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    let source_id: i64 = sqlx::query_scalar(
        "UPDATE intake_outbox
            SET status = $1, completed_at = NOW(), last_error = 'delivery outcome unknown'
          WHERE channel_id = 'ch-unknown-retry'
      RETURNING id",
    )
    .bind(IntakeOutboxStatus::Unknown)
    .fetch_one(&pool)
    .await
    .expect("settle source as official unknown"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

    let new_id =
        match force_fail_and_retry_as_new(&pool, source_id, "operator: retry approved").await {
            Ok(new_id) => new_id,
            Err(error) => {
                pool.close().await;
                pg_db.drop().await;
                return Err(error);
            }
        };

    let source: (IntakeOutboxStatus, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM intake_outbox WHERE id = $1")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .expect("read unknown source"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    assert_eq!(source.0, IntakeOutboxStatus::Unknown);
    assert_eq!(
        source.1.as_deref(),
        Some("delivery outcome unknown"),
        "retry must preserve unknown evidence instead of collapsing it to failure"
    );

    let child: (IntakeOutboxStatus, i32, Option<i64>) = sqlx::query_as(
        "SELECT status, attempt_no, parent_outbox_id FROM intake_outbox WHERE id = $1",
    )
    .bind(new_id)
    .fetch_one(&pool)
    .await
    .expect("read child attempt"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    assert_eq!(
        child,
        (IntakeOutboxStatus::Pending, 2, Some(source_id)),
        "retry must append one pending child linked to the unchanged source"
    );

    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_fail_and_retry_as_new_refuses_done_row_to_prevent_double_emit() {
    // Codex Phase 5 P0 #2: a worker `mark_done` racing with the
    // operator CLI could otherwise rewrite a completed row into
    // `failed_post_accept` and trigger a re-execution that
    // double-emits a Discord turn. The helper MUST refuse `done`.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    insert_pending(&pool, &payload("ch-done", "msg-done"), 1, None)
        .await
        .expect("insert");
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim")
        .expect("row");
    mark_accepted(&pool, claimed.id, "owner-1")
        .await
        .expect("accept");
    mark_spawned(&pool, claimed.id, "owner-1")
        .await
        .expect("spawn");
    mark_done(&pool, claimed.id, "owner-1").await.expect("done");

    let err = force_fail_and_retry_as_new(&pool, claimed.id, "operator: bad timing")
        .await
        .expect_err("done row must be refused");
    match err {
        ForceFailError::DisallowedStatus { id, status } => {
            assert_eq!(id, claimed.id);
            assert_eq!(status, "done");
        }
        other => panic!("expected DisallowedStatus, got {other:?}"),
    }

    // Family did NOT grow — the rejection is total.
    let family_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM intake_outbox
         WHERE channel_id = 'ch-done' AND user_msg_id = 'msg-done'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(family_count, 1, "rejection must not insert a child");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_fail_and_retry_as_new_refuses_pending_and_claimed_states() -> Result<(), sqlx::Error>
{
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let pending_id = insert_pending(&pool, &payload("ch-p", "msg-p"), 1, None)
        .await
        .expect("insert pending");
    let err_pending = force_fail_and_retry_as_new(&pool, pending_id, "x")
        .await
        .expect_err("pending row must be refused");
    assert!(matches!(
        err_pending,
        ForceFailError::DisallowedStatus { .. }
    ));

    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim")
        .expect("row");
    let err_claimed = force_fail_and_retry_as_new(&pool, claimed.id, "x")
        .await
        .expect_err("claimed row must be refused");
    assert!(matches!(
        err_claimed,
        ForceFailError::DisallowedStatus { .. }
    ));

    let unknown_fixture = async {
        sqlx::query("ALTER TABLE intake_outbox DROP CONSTRAINT intake_outbox_status_check")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE intake_outbox SET status = 'future_status' WHERE id = $1")
            .bind(claimed.id)
            .execute(&pool)
            .await?;
        let unknown = force_fail_and_retry_as_new(&pool, claimed.id, "x")
            .await
            .expect_err("unknown strong-enum spelling must fail at row decode");
        let unchanged: (String, i64) = sqlx::query_as(
            "SELECT status, COUNT(*) OVER ()::BIGINT FROM intake_outbox WHERE channel_id = 'ch-p'",
        )
        .fetch_one(&pool)
        .await?;
        Ok::<_, sqlx::Error>((unknown, unchanged))
    }
    .await;
    pool.close().await;
    pg_db.drop().await;

    let (unknown, unchanged) = unknown_fixture?;
    assert!(
        matches!(
            unknown,
            ForceFailError::Db(sqlx::Error::ColumnDecode { ref index, .. })
                if index.trim_matches('"') == "status"
        ),
        "unknown spelling must surface as status ColumnDecode: {unknown:?}"
    );
    assert_eq!(unchanged, ("future_status".to_string(), 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_pending_for_target_filters_by_row_provider() {
    // Codex Phase 5 P0 #1: a single AgentDesk process can host
    // claude AND codex bots; both call `run_intake_worker_loop`
    // with the same `target_instance_id`. The provider filter
    // ensures a claude bot's worker only claims rows forwarded by
    // the claude bot and never picks up a codex row (which would
    // run with the wrong Http/SharedData/token).
    //
    // #4349: the filter reads `intake_outbox.provider`, not
    // `agents.provider`.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    // Seed two agents with different providers.
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-claude', 'Claude bot', 'claude', 'unused-ch-claude'),
                ('agent-codex', 'Codex bot', 'codex', 'unused-ch-codex')",
    )
    .execute(&pool)
    .await
    .expect("seed agents");

    // Insert a row for each provider — both targeted at worker-1.
    let mut claude_payload = payload("ch-claude", "msg-claude");
    claude_payload.agent_id = "agent-claude".to_string();
    claude_payload.provider = "claude".to_string();
    insert_pending(&pool, &claude_payload, 1, None)
        .await
        .expect("insert claude row");

    let mut codex_payload = payload("ch-codex", "msg-codex");
    codex_payload.agent_id = "agent-codex".to_string();
    codex_payload.provider = "codex".to_string();
    insert_pending(&pool, &codex_payload, 1, None)
        .await
        .expect("insert codex row");

    // Claude worker polls — must see ONLY the claude row.
    let claude_claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-claude")
        .await
        .expect("claude claim ok")
        .expect("must claim something");
    assert_eq!(claude_claimed.agent_id, "agent-claude");
    assert_eq!(claude_claimed.channel_id, "ch-claude");

    // Claude polls again — codex row must STILL be invisible.
    let again = claim_pending_for_target(&pool, "worker-1", "claude", "owner-claude")
        .await
        .expect("second claude claim ok");
    assert!(
        again.is_none(),
        "claude worker must not see codex's pending row"
    );

    // Codex worker polls — picks up the codex row.
    let codex_claimed = claim_pending_for_target(&pool, "worker-1", "codex", "owner-codex")
        .await
        .expect("codex claim ok")
        .expect("must claim something");
    assert_eq!(codex_claimed.agent_id, "agent-codex");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_scopes_on_row_provider_not_agent_provider_for_paired_agent() {
    // #4349 regression. A cc/cdx paired agent has ONE `agents.provider`
    // but TWO bots. `project-agentdesk` is stored with provider='codex'
    // while its `discord_channel_cc` is served by the claude bot.
    //
    // Claiming on `agents.provider` handed the claude bot's row to the
    // codex worker, which then ran the turn with the codex token — a
    // Codex reply in a Claude channel. Claim must key on the provider
    // that actually forwarded the row.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_cc)
         VALUES ('agent-paired', 'Paired', 'codex', 'ch-cdx', 'ch-cc')",
    )
    .execute(&pool)
    .await
    .expect("seed paired agent"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

    // The claude bot forwards a message from the agent's cc channel.
    let mut cc_row = payload("ch-cc", "msg-cc");
    cc_row.agent_id = "agent-paired".to_string();
    cc_row.provider = "claude".to_string();
    insert_pending(&pool, &cc_row, 1, None)
        .await
        .expect("insert cc row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

    // The codex worker shares `target_instance_id`. It must NOT claim
    // this row even though `agents.provider = 'codex'` matches it.
    let stolen = claim_pending_for_target(&pool, "worker-1", "codex", "owner-codex")
        .await
        .expect("codex claim ok"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    assert!(
        stolen.is_none(),
        "codex worker must not claim a row forwarded by the claude bot"
    );

    // The claude worker claims it, despite `agents.provider = 'codex'`.
    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-claude")
        .await
        .expect("claude claim ok") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        .expect("claude worker must claim its own row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    assert_eq!(claimed.channel_id, "ch-cc");
    assert_eq!(claimed.provider, "claude");
    assert_eq!(claimed.agent_id, "agent-paired");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_fail_and_retry_as_new_preserves_row_provider() {
    // #4349: a retry must stay on the bot that forwarded the original.
    // Rebuilding the row from `agents.provider` would silently move the
    // retry to the other bot on a paired agent.
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_cc)
         VALUES ('agent-paired', 'Paired', 'codex', 'ch-cdx2', 'ch-cc2')",
    )
    .execute(&pool)
    .await
    .expect("seed paired agent"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

    let mut cc_row = payload("ch-cc2", "msg-retry");
    cc_row.agent_id = "agent-paired".to_string();
    cc_row.provider = "claude".to_string();
    insert_pending(&pool, &cc_row, 1, None).await.expect("seed"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

    let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
        .await
        .expect("claim") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        .expect("row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    mark_accepted(&pool, claimed.id, "owner-1")
        .await
        .expect("accept"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    mark_spawned(&pool, claimed.id, "owner-1")
        .await
        .expect("spawn"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

    let new_id = force_fail_and_retry_as_new(&pool, claimed.id, "operator: hung")
        .await
        .expect("force-fail"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

    let retry_provider: String =
        sqlx::query_scalar("SELECT provider FROM intake_outbox WHERE id = $1")
            .bind(new_id)
            .fetch_one(&pool)
            .await
            .expect("read retry provider"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    assert_eq!(
        retry_provider, "claude",
        "retry must stay on the forwarding bot, not fall back to agents.provider"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_recent_rows_supports_channel_filter_and_orders_descending() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_default_test_agent(&pool).await;

    let _id_a1 = insert_pending(&pool, &payload("ch-A", "msg-A1"), 1, None)
        .await
        .expect("seed A1");
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let id_b = insert_pending(&pool, &payload("ch-B", "msg-B1"), 1, None)
        .await
        .expect("seed B");
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE channel_id='ch-A'")
        .execute(&pool)
        .await
        .expect("terminate A1");
    let id_a2 = insert_pending(&pool, &payload("ch-A", "msg-A2"), 1, None)
        .await
        .expect("seed A2");

    let all = list_recent_rows(&pool, None, 10).await.expect("list all");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, id_a2, "newest first");

    let just_a = list_recent_rows(&pool, Some("ch-A"), 10)
        .await
        .expect("list A");
    assert_eq!(just_a.len(), 2);
    assert!(just_a.iter().all(|r| r.channel_id == "ch-A"));

    let just_b = list_recent_rows(&pool, Some("ch-B"), 10)
        .await
        .expect("list B");
    assert_eq!(just_b.len(), 1);
    assert_eq!(just_b[0].id, id_b);

    pool.close().await;
    pg_db.drop().await;
}

/// #5320: no retention reader yet. Migration 0115 offers sweep ordering
/// bounded by failed_pre_accept rows. Pin both indexes' ordered keys and
/// predicates in the real catalog, without claiming a forced planner choice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_outbox_terminal_retention_index_catalog_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    for (index, expected_predicate) in [
        (
            "idx_intake_outbox_terminal_retention",
            "(status = ANY (ARRAY['done'::text, 'unknown'::text, 'failed_pre_accept'::text, 'failed_post_accept'::text]))",
        ),
        (
            "idx_intake_outbox_failed_pre_accept_sweep_order",
            "(status = 'failed_pre_accept'::text)",
        ),
    ] {
        let (valid, key_atts, first_key, second_key, predicate): (
            bool,
            i16,
            String,
            String,
            String,
        ) = sqlx::query_as(
            "SELECT i.indisvalid, i.indnkeyatts,
                pg_get_indexdef(i.indexrelid, 1, true),
                pg_get_indexdef(i.indexrelid, 2, true),
                pg_get_expr(i.indpred, i.indrelid)
           FROM pg_index i
           JOIN pg_class c ON c.oid = i.indexrelid
           JOIN pg_class t ON t.oid = i.indrelid
           JOIN pg_namespace ns ON ns.oid = c.relnamespace
          WHERE ns.nspname = 'public' AND t.relname = 'intake_outbox'
            AND c.relname = $1",
        )
        .bind(index)
        .fetch_one(&pool)
        .await
        .expect("read #5320 index catalog contract"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        assert!(valid, "fresh migration must build a valid index");
        assert_eq!(key_atts, 2, "index must have exactly two key attributes");
        assert_eq!(first_key, "updated_at", "first key orders by update time");
        assert_eq!(second_key, "id", "second key breaks updated_at ties");
        assert_eq!(
            predicate, expected_predicate,
            "index {index} must retain its exact predicate"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}
