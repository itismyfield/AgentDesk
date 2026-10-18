use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

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

    let result = end_run_with_pg(None, &pool, &run_id).await;
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
