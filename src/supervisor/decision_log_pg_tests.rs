use serde_json::{Value, json};
use sqlx::{PgPool, Row as _};

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

const AGENT: &str = "supervisor-decision-agent";
const DISPATCH: &str = "dispatch-orphan-decision";

fn policy_engine(pool: &PgPool) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pool.clone())).expect("create policy engine")
}

/// An in-progress card whose latest implementation dispatch has been pending for
/// ten minutes with no live session: the shape `load_orphan_candidate` accepts.
async fn seed_orphan_dispatch(pool: &PgPool) {
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ($1, $1, 'claude')")
        .bind(AGENT)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ('card-orphan-decision', 'orphan decision', 'in_progress', $1)",
    )
    .bind(AGENT)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches
            (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at)
         VALUES ($1, 'card-orphan-decision', $2, 'implementation', 'pending', $1, '{}',
                 NOW() - INTERVAL '10 minutes')",
    )
    .bind(DISPATCH)
    .bind(AGENT)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = 'card-orphan-decision'",
    )
    .bind(DISPATCH)
    .execute(pool)
    .await
    .unwrap();
}

/// Contract: each OrphanCandidate pass (probe, then confirmed resume) leaves one
/// runtime_decisions row whose evidence is the decision-log JSON object, not a string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_candidate_records_object_evidence_per_decision_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_orphan_dispatch(&pool).await;
    let supervisor = RuntimeSupervisor::new(Some(pool.clone()), policy_engine(&pool));
    let evidence = json!({
        "dispatch_id": DISPATCH,
        "card_id": "card-orphan-decision",
        "detected_from": "timeouts._section_K",
    });

    let probe = supervisor
        .emit_signal(SupervisorSignal::OrphanCandidate, evidence.clone())
        .expect("first orphan pass records a probe decision");
    assert_eq!(probe.chosen_action, SupervisorAction::Probe);
    assert!(!probe.executed);
    let resume = supervisor
        .emit_signal(SupervisorSignal::OrphanCandidate, evidence)
        .expect("confirmed orphan pass records a resume decision");
    assert_eq!(resume.chosen_action, SupervisorAction::Resume);
    assert!(resume.executed);

    let rows = sqlx::query(
        "SELECT signal, chosen_action, actor, dispatch_id,
                jsonb_typeof(evidence_json) AS kind, evidence_json
         FROM runtime_decisions ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "one audit row per orphan decision");
    for (row, (action, executed)) in rows.iter().zip([("Probe", false), ("Resume", true)]) {
        assert_eq!(row.get::<String, _>("signal"), "OrphanCandidate");
        assert_eq!(row.get::<String, _>("chosen_action"), action);
        assert_eq!(row.get::<String, _>("actor"), SUPERVISOR_ACTOR);
        assert_eq!(
            row.get::<Option<String>, _>("dispatch_id").as_deref(),
            Some(DISPATCH)
        );
        assert_eq!(row.get::<String, _>("kind"), "object");
        let stored: Value = row.get("evidence_json");
        assert_eq!(stored["dispatch_id"], json!(DISPATCH));
        assert_eq!(stored["detected_from"], json!("timeouts._section_K"));
        assert_eq!(
            stored["supervisor_signal_support"],
            json!("implemented_action")
        );
        assert_eq!(stored["supervisor_chosen_action"], json!(action));
        assert_eq!(stored["supervisor_action_executed"], json!(executed));
    }

    pool.close().await;
    db.drop().await;
}
