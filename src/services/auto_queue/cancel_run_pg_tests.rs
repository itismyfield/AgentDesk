use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::cancel_run::{
    cancel_live_dispatches_for_runs_pg, cancel_selected_runs_with_pg,
    skip_dispatched_entries_for_runs_pg,
};
use sqlx::PgPool;

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
