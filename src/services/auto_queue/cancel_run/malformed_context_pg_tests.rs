use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::cancel_run::{
    cancel_live_dispatches_for_runs_pg, cancel_selected_runs_with_pg, end_run_with_pg,
    skip_dispatched_entries_for_runs_pg,
};
use sqlx::PgPool;

const MATRIX_RUN: &str = "run-owner-matrix";

/// Context inputs and the `phase_gate.run_id` owner each must resolve to.
/// Strict path semantics: only an object `phase_gate` holding `run_id` names an owner.
const OWNER_CONTEXT_MATRIX: &[(Option<&str>, Option<&str>)] = &[
    (None, None),
    (Some(""), None),
    (Some("   "), None),
    (Some("null"), None),
    (Some("123"), None),
    (Some("\"s\""), None),
    (Some("true"), None),
    (Some("{"), None),
    (Some("{\"phase_gate\":"), None),
    (Some("[ "), None),
    (Some("{\"a\":1} x"), None),
    (Some("[1,2]"), None),
    (
        Some("[{\"phase_gate\":{\"run_id\":\"run-owner-matrix\"}}]"),
        None,
    ),
    (Some("{\"phase_gate\":\"run-owner-matrix\"}"), None),
    (
        Some("{\"phase_gate\":[{\"run_id\":\"run-owner-matrix\"}]}"),
        None,
    ),
    (Some("{\"phase_gate\":{\"run_id\":null}}"), None),
    (Some("{\"phase_gate\":{\"run_id\":123}}"), Some("123")),
    (Some("{\"phase_gate\":{\"run_id\":\"\"}}"), Some("")),
    (
        Some("  {\"phase_gate\":{\"run_id\":\"run-owner-matrix\"}}  "),
        Some(MATRIX_RUN),
    ),
    (
        Some("{\"phase_gate\":{\"run_id\":\"run-owner-matrix\"}}"),
        Some(MATRIX_RUN),
    ),
];

async fn seed_agent_and_card(pool: &PgPool, card_id: &str) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-malformed', 'Malformed Agent', 'claude', '123')
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, 'Malformed Card', 'in_progress', 'agent-malformed')",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .expect("seed card");
}

async fn seed_dispatch(
    pool: &PgPool,
    dispatch_id: &str,
    card_id: &str,
    status: &str,
    context: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO task_dispatches
            (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context)
         VALUES ($1, $2, 'agent-malformed', 'implementation', $3, 'Malformed Dispatch', $4)",
    )
    .bind(dispatch_id)
    .bind(card_id)
    .bind(status)
    .bind(context)
    .execute(pool)
    .await
    .expect("seed dispatch");
}

async fn dispatch_status(pool: &PgPool, dispatch_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .expect("load dispatch status")
}

async fn run_status(pool: &PgPool, run_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("load run status")
}

/// The owner lookup resolves every legacy context shape the same way on PostgreSQL 15
/// and later: only a strict `phase_gate.run_id` naming the run selects its dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_context_extract_matrix_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_agent_and_card(&pool, "card-owner-matrix").await;

    for (index, (context, expected_owner)) in OWNER_CONTEXT_MATRIX.iter().enumerate() {
        let owner: Option<String> =
            sqlx::query_scalar("SELECT json_extract($1::text, '$.phase_gate.run_id')")
                .bind(context)
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|error| panic!("extract owner from {context:?}: {error}"));
        assert_eq!(owner.as_deref(), *expected_owner, "owner of {context:?}");
        seed_dispatch(
            &pool,
            &format!("dispatch-owner-matrix-{index:02}"),
            "card-owner-matrix",
            "pending",
            *context,
        )
        .await;
    }

    let mut tx = pool.begin().await.expect("begin owner matrix cancel");
    let transitions = crate::dispatch::cancel_dispatches_for_runs_on_pg_tx_with_meta(
        &mut tx,
        &[MATRIX_RUN.to_string()],
        Some("operator_cancelled"),
        true,
    )
    .await
    .expect("legacy context shapes must not abort the owner lookup");
    tx.commit().await.expect("commit owner matrix cancel");

    let mut expected_cancelled = Vec::new();
    for (index, (context, expected_owner)) in OWNER_CONTEXT_MATRIX.iter().enumerate() {
        let dispatch_id = format!("dispatch-owner-matrix-{index:02}");
        let owned = *expected_owner == Some(MATRIX_RUN);
        if owned {
            expected_cancelled.push(dispatch_id.clone());
        }
        let expected_status = if owned { "cancelled" } else { "pending" };
        assert_eq!(
            dispatch_status(&pool, &dispatch_id).await,
            expected_status,
            "dispatch with context {context:?}"
        );
    }
    let mut cancelled: Vec<String> = transitions
        .into_iter()
        .map(|transition| transition.dispatch_id)
        .collect();
    cancelled.sort();
    assert_eq!(cancelled, expected_cancelled);

    pool.close().await;
    pg_db.drop().await;
}

/// A live run with its own entry and dispatch, next to two live dispatches no run owns
/// whose legacy context is truncated JSON.
async fn seed_run_beside_unrelated_malformed_dispatches(
    pool: &PgPool,
    suffix: &str,
) -> (String, String, [String; 2]) {
    let run_id = format!("run-malformed-{suffix}");
    let dispatch_id = format!("dispatch-malformed-{suffix}");
    let card_id = format!("card-malformed-{suffix}");
    seed_agent_and_card(pool, &card_id).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
         VALUES ($1, 'agent-malformed', 'active')",
    )
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("seed run");
    seed_dispatch(pool, &dispatch_id, &card_id, "dispatched", None).await;
    sqlx::query(
        "INSERT INTO auto_queue_entries
            (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index)
         VALUES ($1, $2, $3, 'agent-malformed', 'dispatched', $4, 0)",
    )
    .bind(format!("entry-malformed-{suffix}"))
    .bind(&run_id)
    .bind(&card_id)
    .bind(&dispatch_id)
    .execute(pool)
    .await
    .expect("seed entry");

    let unrelated = [
        format!("dispatch-unrelated-open-{suffix}"),
        format!("dispatch-unrelated-gate-{suffix}"),
    ];
    let unrelated_card = format!("card-unrelated-{suffix}");
    seed_agent_and_card(pool, &unrelated_card).await;
    for (dispatch, context) in unrelated.iter().zip(["{", "{\"phase_gate\":"]) {
        seed_dispatch(pool, dispatch, &unrelated_card, "pending", Some(context)).await;
    }
    (run_id, dispatch_id, unrelated)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_cancel_ignores_unrelated_malformed_context_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (run_id, dispatch_id, unrelated) =
        seed_run_beside_unrelated_malformed_dispatches(&pool, "cancel").await;

    let result = cancel_selected_runs_with_pg(
        crate::services::discord::health::slot_row_host_refusal,
        None,
        &pool,
        std::slice::from_ref(&run_id),
        "auto_queue_cancel",
    )
    .await;
    let durable_run_status = run_status(&pool, &run_id).await;
    let response = result.unwrap_or_else(|error| {
        panic!("cancel returned Err with run status {durable_run_status}: {error}")
    });

    assert_eq!(durable_run_status, "cancelled");
    assert_eq!(response["cancelled_runs"], 1);
    assert_eq!(response["cancelled_dispatches"], 1);
    assert_eq!(response["remaining_live_dispatches"], 0);
    assert_eq!(dispatch_status(&pool, &dispatch_id).await, "cancelled");
    for dispatch in &unrelated {
        assert_eq!(
            dispatch_status(&pool, dispatch).await,
            "pending",
            "{dispatch}"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_end_ignores_unrelated_malformed_context_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (run_id, _dispatch_id, unrelated) =
        seed_run_beside_unrelated_malformed_dispatches(&pool, "end").await;

    let result = end_run_with_pg(
        crate::services::discord::health::slot_row_host_refusal,
        None,
        &pool,
        &run_id,
    )
    .await;
    let durable_run_status = run_status(&pool, &run_id).await;
    let ended = result.unwrap_or_else(|error| {
        panic!("end returned Err with run status {durable_run_status}: {error}")
    });

    assert!(ended, "end must report the run it completed");
    assert_eq!(durable_run_status, "completed");
    for dispatch in &unrelated {
        assert_eq!(
            dispatch_status(&pool, dispatch).await,
            "pending",
            "{dispatch}"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

/// The census counts a live dispatch tied to the target only by a finished entry while a
/// foreign gate keeps it live, and skips terminal and unassociated dispatches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_cancel_counts_surviving_associated_dispatch_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (run_id, dispatch_id, unrelated) =
        seed_run_beside_unrelated_malformed_dispatches(&pool, "census").await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
         VALUES ('run-census-foreign', 'agent-malformed', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed foreign run");
    for (dispatch, status) in [
        ("dispatch-census-survivor", "pending"),
        ("dispatch-census-done", "completed"),
    ] {
        let card = format!("card-{dispatch}");
        seed_agent_and_card(&pool, &card).await;
        seed_dispatch(&pool, dispatch, &card, status, None).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index)
             VALUES ($1, $2, $3, 'agent-malformed', 'done', $4, 0)",
        )
        .bind(format!("entry-{dispatch}"))
        .bind(&run_id)
        .bind(&card)
        .bind(dispatch)
        .execute(&pool)
        .await
        .expect("seed finished entry");
    }
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (run_id, phase, status, dispatch_id)
         VALUES ('run-census-foreign', 1, 'pending', 'dispatch-census-survivor')",
    )
    .execute(&pool)
    .await
    .expect("seed foreign gate");

    let response = cancel_selected_runs_with_pg(
        crate::services::discord::health::slot_row_host_refusal,
        None,
        &pool,
        std::slice::from_ref(&run_id),
        "auto_queue_cancel",
    )
    .await
    .expect("cancel run");

    assert_eq!(response["remaining_live_dispatches"], 1);
    assert_eq!(response["cancelled_dispatches"], 1);
    assert_eq!(dispatch_status(&pool, &dispatch_id).await, "cancelled");
    assert_eq!(
        dispatch_status(&pool, "dispatch-census-survivor").await,
        "pending"
    );
    assert_eq!(
        dispatch_status(&pool, "dispatch-census-done").await,
        "completed"
    );
    for dispatch in &unrelated {
        assert_eq!(dispatch_status(&pool, dispatch).await, "pending");
    }

    pool.close().await;
    pg_db.drop().await;
}

/// A census failure aborts the cancel transaction: the caller gets the error and the run,
/// entry, dispatch and cleanup enrollment are all left as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_cancel_census_failure_rolls_back_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (run_id, dispatch_id, _unrelated) =
        seed_run_beside_unrelated_malformed_dispatches(&pool, "census-fail").await;
    // This database only: the owner-context read fails once the target run is terminal,
    // which on the cancel path is the census after the run update.
    sqlx::query("ALTER FUNCTION json_extract(text, text) RENAME TO json_extract_census_real")
        .execute(&pool)
        .await
        .expect("rename json_extract");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION json_extract(input TEXT, path TEXT) RETURNS TEXT
         LANGUAGE plpgsql VOLATILE AS $$
         BEGIN
             IF path = '$.phase_gate.run_id' AND EXISTS (
                 SELECT 1 FROM auto_queue_runs
                 WHERE id = '{run_id}' AND status IN ('cancelled', 'completed')
             ) THEN
                 RAISE EXCEPTION 'census sentinel';
             END IF;
             RETURN json_extract_census_real(input, path);
         END
         $$"
    )))
    .execute(&pool)
    .await
    .expect("install census sentinel");

    let result = cancel_selected_runs_with_pg(
        crate::services::discord::health::slot_row_host_refusal,
        None,
        &pool,
        std::slice::from_ref(&run_id),
        "auto_queue_cancel",
    )
    .await;

    let error = result.expect_err("census failure must fail the cancel");
    assert!(
        error.contains("count postgres live dispatches") && error.contains("census sentinel"),
        "{error}"
    );
    assert_eq!(run_status(&pool, &run_id).await, "active");
    assert_eq!(dispatch_status(&pool, &dispatch_id).await, "dispatched");
    let entry_status: String = sqlx::query_scalar(
        "SELECT status FROM auto_queue_entries WHERE id = 'entry-malformed-census-fail'",
    )
    .fetch_one(&pool)
    .await
    .expect("load entry status");
    assert_eq!(entry_status, "dispatched");
    let cleanup_tasks: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM auto_queue_run_cleanup_tasks")
            .fetch_one(&pool)
            .await
            .expect("count cleanup tasks");
    assert_eq!(cleanup_tasks, 0);

    pool.close().await;
    pg_db.drop().await;
}

async fn seed_cancel_fixture(pool: &PgPool, suffix: &str) -> (String, String, String) {
    let run_id = format!("run-cancel-{suffix}");
    let entry_id = format!("entry-cancel-{suffix}");
    let dispatch_id = format!("dispatch-cancel-{suffix}");
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-cancel', 'Cancel Agent', 'claude', '123')
             ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed cancel agent");
    let card_id = format!("card-cancel-{suffix}");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ($1, 'Cancel Card', 'in_progress', 'agent-cancel')",
    )
    .bind(&card_id)
    .execute(pool)
    .await
    .expect("seed cancel card");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-cancel', 'active')",
    )
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("seed cancel run");
    sqlx::query(
        "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-cancel', 'implementation', 'dispatched', 'Cancel Dispatch')",
    )
    .bind(&dispatch_id)
    .bind(&card_id)
    .execute(pool)
    .await
    .expect("seed cancel dispatch");
    sqlx::query(
        "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index)
             VALUES ($1, $2, $3, 'agent-cancel', 'dispatched', $4, 0)",
    )
    .bind(&entry_id)
    .bind(&run_id)
    .bind(&card_id)
    .bind(&dispatch_id)
    .execute(pool)
    .await
    .expect("seed cancel entry");
    sqlx::query(
        "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group)
             VALUES ('agent-cancel', 0, $1, 0)",
    )
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("seed cancel slot");
    (run_id, entry_id, dispatch_id)
}

#[derive(Clone, Copy)]
enum OwnerArm {
    Entry,
    PhaseGate,
    Context,
}

async fn add_dispatch_owner(pool: &PgPool, dispatch_id: &str, run_id: &str, arm: OwnerArm) {
    match arm {
        OwnerArm::Entry => {
            sqlx::query(
                "INSERT INTO auto_queue_entries
                        (id, run_id, agent_id, status, dispatch_id)
                     VALUES ($1, $2, 'agent-cancel', 'dispatched', $3)",
            )
            .bind(format!("entry-{dispatch_id}-{run_id}"))
            .bind(run_id)
            .bind(dispatch_id)
            .execute(pool)
            .await
            .expect("seed dispatch entry owner"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        }
        OwnerArm::PhaseGate => {
            sqlx::query(
                "INSERT INTO auto_queue_phase_gates (run_id, phase, status, dispatch_id)
                     VALUES ($1, 0, 'pending', $2)",
            )
            .bind(run_id)
            .bind(dispatch_id)
            .execute(pool)
            .await
            .expect("seed dispatch phase-gate owner"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        }
        OwnerArm::Context => {
            sqlx::query(
                "UPDATE task_dispatches
                     SET context = jsonb_build_object(
                         'phase_gate', jsonb_build_object('run_id', $1::text)
                     )::text
                     WHERE id = $2",
            )
            .bind(run_id)
            .bind(dispatch_id)
            .execute(pool)
            .await
            .expect("seed dispatch context owner"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        }
    }
}

async fn assert_foreign_owner_preserved(
    suffix: &str,
    target_arm: OwnerArm,
    foreign_arm: OwnerArm,
    sole_arm: OwnerArm,
) {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-cancel', 'Cancel Agent', 'claude', '123')
             ON CONFLICT (id) DO NOTHING",
    )
    .execute(&pool)
    .await
    .expect("seed cancel agent"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    let target_run = format!("run-target-{suffix}");
    let foreign_run = format!("run-foreign-{suffix}");
    let shared_dispatch = format!("dispatch-shared-{suffix}");
    let sole_dispatch = format!("dispatch-sole-{suffix}");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-cancel', 'active'), ($2, 'agent-cancel', 'active')",
    )
    .bind(&target_run)
    .bind(&foreign_run)
    .execute(&pool)
    .await
    .expect("seed owner runs"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    sqlx::query(
        "INSERT INTO task_dispatches
                (id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, 'agent-cancel', 'implementation', 'dispatched', 'Shared'),
                    ($2, 'agent-cancel', 'implementation', 'dispatched', 'Sole')",
    )
    .bind(&shared_dispatch)
    .bind(&sole_dispatch)
    .execute(&pool)
    .await
    .expect("seed owner dispatches"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    add_dispatch_owner(&pool, &shared_dispatch, &target_run, target_arm).await;
    add_dispatch_owner(&pool, &shared_dispatch, &foreign_run, foreign_arm).await;
    add_dispatch_owner(&pool, &sole_dispatch, &target_run, sole_arm).await;
    let foreign_entry = (matches!(foreign_arm, OwnerArm::Entry))
        .then(|| format!("entry-{shared_dispatch}-{foreign_run}"));

    let response = cancel_selected_runs_with_pg(
        crate::services::discord::health::slot_row_host_refusal,
        None,
        &pool,
        std::slice::from_ref(&target_run),
        "auto_queue_cancel",
    )
    .await
    .expect("cancel target run through production entrypoint"); // agentdesk-audit: allow-unwrap — test invokes the production entrypoint
    assert_eq!(response["cancelled_dispatches"], 1);
    let states = sqlx::query_as::<_, (String, String)>(
        "SELECT shared.status, sole.status
             FROM task_dispatches shared
             JOIN task_dispatches sole ON sole.id = $2
             WHERE shared.id = $1",
    )
    .bind(&shared_dispatch)
    .bind(&sole_dispatch)
    .fetch_one(&pool)
    .await
    .expect("load owner dispatch states"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    assert_eq!(states, ("dispatched".to_string(), "cancelled".to_string()));
    if let Some(foreign_entry) = foreign_entry {
        let entry = sqlx::query_as::<_, (String, Option<String>, i64)>(
            "SELECT status, dispatch_id,
                        (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = $1)
                 FROM auto_queue_entries WHERE id = $1",
        )
        .bind(&foreign_entry)
        .fetch_one(&pool)
        .await
        .expect("load foreign entry state"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(entry, ("dispatched".to_string(), Some(shared_dispatch), 0));
    }
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancel_live_dispatches_for_runs_pg_preserves_entry_until_force_pause_skip() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (run_id, entry_id, dispatch_id) = seed_cancel_fixture(&pool, "force-pause-entry").await;

    let cancelled = cancel_live_dispatches_for_runs_pg(
        &pool,
        std::slice::from_ref(&run_id),
        "auto_queue_pause",
    )
    .await
    .expect("cancel force-pause dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
    assert_eq!(cancelled.dispatch_ids, vec![dispatch_id.clone()]);
    let state = sqlx::query_as::<_, (String, Option<String>, String)>(
        "SELECT e.status, e.dispatch_id, d.status
             FROM auto_queue_entries e
             JOIN task_dispatches d ON d.id = $2
             WHERE e.id = $1",
    )
    .bind(&entry_id)
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .expect("load force-pause entry state"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    assert_eq!(
        state,
        (
            "dispatched".to_string(),
            Some(dispatch_id),
            "cancelled".to_string()
        )
    );
    assert_eq!(
        skip_dispatched_entries_for_runs_pg(&pool, &[run_id], "run_pause")
            .await
            .expect("skip force-pause entry"), // agentdesk-audit: allow-unwrap — production entrypoint assertion
        1
    );
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancel_live_dispatches_for_runs_pg_preserves_foreign_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (target_run, _entry_id, dispatch_id) =
        seed_cancel_fixture(&pool, "force-pause-session").await;
    let foreign_run = "run-foreign-force-pause-session";
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-cancel', 'active')",
    )
    .bind(foreign_run)
    .execute(&pool)
    .await
    .expect("seed force-pause foreign run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    add_dispatch_owner(&pool, &dispatch_id, foreign_run, OwnerArm::Context).await;
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id)
             VALUES ('session-force-pause-foreign', 'agent-cancel', 'turn_active', $1)",
    )
    .bind(&dispatch_id)
    .execute(&pool)
    .await
    .expect("seed force-pause foreign session"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

    assert!(
        cancel_live_dispatches_for_runs_pg(&pool, &[target_run], "auto_queue_pause")
            .await
            .expect("cancel target force-pause dispatches") // agentdesk-audit: allow-unwrap — production entrypoint assertion
            .dispatch_ids
            .is_empty()
    );
    let state = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT d.status, s.status, s.active_dispatch_id
             FROM task_dispatches d
             JOIN sessions s ON s.session_key = 'session-force-pause-foreign'
             WHERE d.id = $1",
    )
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .expect("load force-pause foreign session state"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    assert_eq!(
        state,
        (
            "dispatched".to_string(),
            "turn_active".to_string(),
            Some(dispatch_id)
        )
    );
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancel_selected_runs_with_pg_atomically_terminalizes_run_entry_and_dispatch() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (run_id, entry_id, dispatch_id) = seed_cancel_fixture(&pool, "entrypoint").await;

    let response = cancel_selected_runs_with_pg(
        crate::services::discord::health::slot_row_host_refusal,
        None,
        &pool,
        std::slice::from_ref(&run_id),
        "auto_queue_cancel",
    )
    .await
    .expect("cancel selected run through production entrypoint");
    assert_eq!(response["cancelled_runs"], 1);
    assert_eq!(response["cancelled_entries"], 1);
    assert_eq!(response["cancelled_dispatches"], 1);

    let state = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
        "SELECT r.status, e.status, e.dispatch_id, d.status, s.assigned_run_id
             FROM auto_queue_runs r
             JOIN auto_queue_entries e ON e.id = $2
             JOIN task_dispatches d ON d.id = $3
             JOIN auto_queue_slots s ON s.agent_id = 'agent-cancel' AND s.slot_index = 0
             WHERE r.id = $1",
    )
    .bind(&run_id)
    .bind(&entry_id)
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .expect("load cancelled run state");
    assert_eq!(
        state,
        (
            "cancelled".to_string(),
            "skipped".to_string(),
            None,
            "cancelled".to_string(),
            None,
        )
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancel_selected_runs_with_pg_preserves_foreign_context_owner() {
    assert_foreign_owner_preserved(
        "foreign-context",
        OwnerArm::Entry,
        OwnerArm::Context,
        OwnerArm::Context,
    )
    .await;
}

#[tokio::test]
async fn cancel_selected_runs_with_pg_preserves_foreign_entry_owner() {
    assert_foreign_owner_preserved(
        "foreign-entry",
        OwnerArm::Context,
        OwnerArm::Entry,
        OwnerArm::Entry,
    )
    .await;
}

#[tokio::test]
async fn cancel_selected_runs_with_pg_preserves_foreign_phase_gate_owner() {
    assert_foreign_owner_preserved(
        "foreign-phase-gate",
        OwnerArm::Context,
        OwnerArm::PhaseGate,
        OwnerArm::PhaseGate,
    )
    .await;
}
