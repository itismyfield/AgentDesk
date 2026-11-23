use crate::db::auto_queue::phase_gate_verdict::verdict_matches as phase_gate_verdict_matches;
use crate::db::auto_queue::phase_gate_verdict::{VerdictResolution, resolve_verdict};
use crate::db::auto_queue::phase_gates::{
    PhaseGateReconciliation, PhaseGateRepairOptions, PhaseGateStateWrite, parse_phase_gate_context,
    save_phase_gate_state_on_pg,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::transactions::{
    reconcile_phase_gate_for_terminal_dispatch_on_pg_tx, repair_phase_gates_for_run_on_pg,
};
use serde_json::json;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tokio::sync::Barrier;

async fn fixture(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-pg-test', 'Agent', 'claude', '999')",
    )
    .execute(pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-pg-test', 'repo', 'agent-pg-test', 'active')",
    )
    .execute(pool)
    .await
    .expect("seed run");
}

async fn insert_dispatch(
    pool: &PgPool,
    id: &str,
    status: &str,
    context: serde_json::Value,
    result: Option<serde_json::Value>,
) {
    sqlx::query(
        "INSERT INTO task_dispatches
                (id, to_agent_id, dispatch_type, status, title, context, result)
             VALUES ($1, 'agent-pg-test', 'phase-gate', $2, 'gate dispatch',
                     CAST($3 AS jsonb), CAST($4 AS jsonb))",
    )
    .bind(id)
    .bind(status)
    .bind(context.to_string())
    .bind(result.map(|v| v.to_string()))
    .execute(pool)
    .await
    .expect("seed gate dispatch");
}

async fn run_reconcile(
    pool: &PgPool,
    dispatch_id: &str,
    status: &str,
    context: serde_json::Value,
    result: Option<serde_json::Value>,
) -> PhaseGateReconciliation {
    let context_text = context.to_string();
    let result_text = result.as_ref().map(|v| v.to_string());
    let mut tx = pool.begin().await.expect("begin tx");
    let outcome = reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
        &mut tx,
        dispatch_id,
        status,
        Some(context_text.as_str()),
        result_text.as_deref(),
    )
    .await
    .expect("reconcile");
    tx.commit().await.expect("commit");
    outcome
}

async fn run_status(pool: &PgPool, run_id: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("run row")
}

async fn gate_count(pool: &PgPool, run_id: &str, phase: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2",
    )
    .bind(run_id)
    .bind(phase)
    .fetch_one(pool)
    .await
    .expect("gate count")
}

async fn gate_status(pool: &PgPool, run_id: &str, phase: i64) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT status FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2 LIMIT 1",
    )
    .bind(run_id)
    .bind(phase)
    .fetch_one(pool)
    .await
    .expect("gate status")
}

async fn gate_failure_diagnostic(
    pool: &PgPool,
    run_id: &str,
    phase: i64,
) -> (Option<String>, Option<String>) {
    let row = sqlx::query(
        "SELECT verdict, failure_reason FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2 LIMIT 1",
    )
    .bind(run_id)
    .bind(phase)
    .fetch_one(pool)
    .await
    .expect("gate failure diagnostic"); // agentdesk-audit: allow-unwrap — test-only PG assertion in #[cfg(test)] module
    (
        row.try_get("verdict").expect("decode verdict"), // agentdesk-audit: allow-unwrap — test-only PG assertion in #[cfg(test)] module
        row.try_get("failure_reason")
            .expect("decode failure reason"), // agentdesk-audit: allow-unwrap — test-only PG assertion in #[cfg(test)] module
    )
}

fn gate_context() -> serde_json::Value {
    let mut declaration =
        crate::phase_gate::resolve_declaration_value("pr-confirm").expect("pr-confirm declaration"); // agentdesk-audit: allow-unwrap — immutable built-in fixture in #[cfg(test)] module
    let gate = declaration.as_object_mut().expect("declaration object"); // agentdesk-audit: allow-unwrap — registry declaration is always a JSON object

    gate.insert("run_id".to_string(), json!("run-pg-test"));
    gate.insert("batch_phase".to_string(), json!(0));
    gate.insert("next_phase".to_string(), json!(1));
    gate.insert("final_phase".to_string(), json!(false));
    json!({"phase_gate": declaration})
}

#[test]
fn parse_phase_gate_context_handles_string_batch_phase() {
    let ctx = parse_phase_gate_context(Some(
        r#"{"phase_gate":{"run_id":"r","batch_phase":"0","pass_verdict":"p"}}"#,
    ))
    .expect("ctx");
    assert_eq!(ctx.run_id, "r");
    assert_eq!(ctx.phase, 0);
}

#[test]
fn parse_phase_gate_context_returns_none_without_run_id() {
    assert!(
        parse_phase_gate_context(Some(
            r#"{"phase_gate":{"batch_phase":0,"pass_verdict":"p"}}"#
        ))
        .is_none()
    );
    assert!(parse_phase_gate_context(Some("{}")).is_none());
    assert!(parse_phase_gate_context(None).is_none());
}

#[test]
fn explicit_phase_gate_verdict_key_blocks_inference() {
    let context = gate_context();
    let result = json!({
        "phase_gate_verdict": "manual_hold",
        "checks": {
            "merge_verified": { "status": "pass" },
            "issue_closed": { "status": "pass" },
            "build_passed": { "status": "pass" }
        }
    });

    assert_eq!(
        resolve_verdict(Some(&context), &result),
        VerdictResolution::Explicit("manual_hold".to_string())
    );
    assert!(!phase_gate_verdict_matches(
        Some("manual_hold"),
        "phase_gate_passed",
        Some(&context),
        Some(&result),
    ));
}

#[test]
fn pass_alias_matches_phase_gate_pass_verdict_when_checks_pass() {
    let context = gate_context();
    let result = json!({
        "verdict": "pass",
        "checks": {
            "merge_verified": { "status": "pass" },
            "issue_closed": { "status": "pass" },
            "build_passed": { "status": "pass" }
        }
    });

    assert!(phase_gate_verdict_matches(
        Some("pass"),
        "phase_gate_passed",
        Some(&context),
        Some(&result),
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_phase_gate_context_is_noop() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let outcome = run_reconcile(&pool, "dsp-noop", "completed", json!({}), Some(json!({}))).await;
    assert!(matches!(outcome, PhaseGateReconciliation::NoContext));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_phase_gate_row_returns_stale_row() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let outcome = run_reconcile(
        &pool,
        "dsp-stale",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    assert!(matches!(outcome, PhaseGateReconciliation::StaleRow));

    pool.close().await;
    pg_db.drop().await;
}

/// Seeds a pending entry so a gate clear resumes the run instead of finalizing it.
async fn seed_pending_entry(pool: &PgPool, run_id: &str, entry_id: &str, batch_phase: i64) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, repo_id, title, status)
             VALUES ($1, NULL, 'card', 'pending')",
    )
    .bind(format!("{entry_id}-card"))
    .execute(pool)
    .await
    .expect("seed kanban card");
    sqlx::query(
        "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, batch_phase, priority_rank)
             VALUES ($1, $2, $3, 'agent-pg-test', 'pending', $4, 0)",
    )
    .bind(entry_id)
    .bind(run_id)
    .bind(format!("{entry_id}-card"))
    .bind(batch_phase)
    .execute(pool)
    .await
    .expect("seed pending entry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pass_verdict_with_no_siblings_clears_gate_and_resumes_paused_run() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;
    // Seed a pending entry on the next phase so finalization does not
    // run the moment we clear the gate — we want to assert resume here.
    seed_pending_entry(&pool, "run-pg-test", "entry-next-phase", 1).await;

    insert_dispatch(
        &pool,
        "dsp-pass",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-pass".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .unwrap();

    let outcome = run_reconcile(
        &pool,
        "dsp-pass",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    match outcome {
        PhaseGateReconciliation::Cleared {
            run_resumed,
            next_phase,
            run_finalized,
            ..
        } => {
            assert!(run_resumed, "paused run should resume after pass");
            assert_eq!(next_phase, Some(1));
            assert!(
                !run_finalized,
                "run with pending entries must not finalize on gate clear"
            );
        }
        other => panic!("expected Cleared, got {other:?}"),
    }
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);
    assert_eq!(run_status(&pool, "run-pg-test").await, "active");

    pool.close().await;
    pg_db.drop().await;
}

/// #1980 (codex v2 HIGH): a final-phase gate clear must finalize the run
/// in-transaction so CRUD/recovery completion does not leave the run
/// active without a completion notification. Mirrors the JS hook's
/// `completeRunAndNotify` for the bypass path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_phase_pass_finalizes_run_and_queues_completion_notify() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    seed_pending_entry(&pool, "run-pg-test", "entry-drained", 0).await;
    sqlx::query("UPDATE auto_queue_entries SET status = 'done' WHERE id = 'entry-drained'")
        .execute(&pool)
        .await
        .expect("drain entry");
    let mut final_context = gate_context();
    final_context["phase_gate"]["final_phase"] = json!(true);
    insert_dispatch(
        &pool,
        "dsp-final",
        "completed",
        final_context.clone(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-final".into()],
            next_phase: None,
            final_phase: true,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    let outcome = run_reconcile(
        &pool,
        "dsp-final",
        "completed",
        final_context,
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    match outcome {
        PhaseGateReconciliation::Cleared {
            run_finalized,
            final_phase,
            ..
        } => {
            assert!(final_phase, "context flagged final_phase");
            assert!(
                run_finalized,
                "final-phase pass with drained run must finalize"
            );
        }
        other => panic!("expected Cleared, got {other:?}"),
    }
    assert_eq!(run_status(&pool, "run-pg-test").await, "completed");
    let notify_count: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM message_outbox WHERE bot = 'notify'",
    )
    .fetch_one(&pool)
    .await
    .expect("count notify rows");
    assert!(
        notify_count >= 1,
        "expected completion notification queued, got {notify_count}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verdict_mismatch_marks_gate_failed_and_pauses_run() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    insert_dispatch(
        &pool,
        "dsp-bad",
        "completed",
        gate_context(),
        Some(json!({"verdict":"please_revise","summary":"reviewer wants edits"})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-bad".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    let outcome = run_reconcile(
        &pool,
        "dsp-bad",
        "completed",
        gate_context(),
        Some(json!({"verdict":"please_revise","summary":"reviewer wants edits"})),
    )
    .await;
    match outcome {
        PhaseGateReconciliation::MarkedFailed { failed_reason, .. } => {
            assert_eq!(failed_reason, "reviewer wants edits");
        }
        other => panic!("expected MarkedFailed, got {other:?}"),
    }
    assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");
    assert_eq!(run_status(&pool, "run-pg-test").await, "paused");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_string_explicit_payload_is_redacted_in_failure_diagnostics() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let context = gate_context();
    let result = json!({
        "verdict": {"authorization": "Bearer secret"},
        "checks": {"build_passed": {"status": "fail"}}
    });
    insert_dispatch(
        &pool,
        "dsp-diagnostic",
        "completed",
        context.clone(),
        Some(result.clone()),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-diagnostic".into()],
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module

    let outcome = run_reconcile(&pool, "dsp-diagnostic", "completed", context, Some(result)).await;
    let outcome_reason = match &outcome {
        PhaseGateReconciliation::MarkedFailed { failed_reason, .. } => Some(failed_reason),
        _ => None,
    };
    assert!(
        outcome_reason.is_some_and(|reason| reason.contains("<non-string:object>")),
        "outcome should retain only the verdict class: {outcome:?}"
    );
    assert!(
        outcome_reason.is_some_and(|reason| {
            !reason.contains("authorization") && !reason.contains("Bearer secret")
        }),
        "outcome must redact verdict keys and values: {outcome:?}"
    );

    let (verdict, reason) = gate_failure_diagnostic(&pool, "run-pg-test", 0).await;
    assert_eq!(verdict.as_deref(), Some("<non-string:object>"));
    assert!(
        reason
            .as_deref()
            .is_some_and(|reason| reason.contains("<non-string:object>")),
        "failure reason should retain only the verdict class: {reason:?}"
    );
    for forbidden in ["authorization", "Bearer secret"] {
        assert!(
            verdict
                .as_deref()
                .is_none_or(|verdict| !verdict.contains(forbidden)),
            "persisted verdict leaked {forbidden}: {verdict:?}"
        );
        assert!(
            reason
                .as_deref()
                .is_none_or(|reason| !reason.contains(forbidden)),
            "persisted failure reason leaked {forbidden}: {reason:?}"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_group_pass_verdict_uses_each_dispatch_context() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let context_a = gate_context();
    let mut deploy =
        crate::phase_gate::resolve_declaration_value("deploy-gate").expect("deploy declaration"); // agentdesk-audit: allow-unwrap — immutable built-in fixture in #[cfg(test)] module
    let deploy_gate = deploy.as_object_mut().expect("declaration object"); // agentdesk-audit: allow-unwrap — registry declaration is always a JSON object
    deploy_gate.insert("run_id".to_string(), json!("run-pg-test"));
    deploy_gate.insert("batch_phase".to_string(), json!(0));
    deploy_gate.insert("next_phase".to_string(), json!(1));
    deploy_gate.insert("final_phase".to_string(), json!(false));
    let context_b = json!({"phase_gate": deploy});
    for (id, context) in [
        ("dsp-group-a", context_a.clone()),
        ("dsp-group-b", context_b.clone()),
    ] {
        insert_dispatch(
            &pool,
            id,
            "completed",
            context.clone(),
            Some(json!({
                "verdict": "phase_gate_passed",
                "checks": {
                    "merge_verified": "pass",
                    "issue_closed": "pass",
                    "build_passed": "pass"
                }
            })),
        )
        .await;
    }
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-group-a".into(), "dsp-group-b".into()],
            ..Default::default()
        },
    )
    .await
    .expect("seed mixed gate groups"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module

    let outcome = run_reconcile(
        &pool,
        "dsp-group-a",
        "completed",
        context_a,
        Some(json!({
            "verdict": "phase_gate_passed",
            "checks": {
                "merge_verified": "pass",
                "issue_closed": "pass",
                "build_passed": "pass"
            }
        })),
    )
    .await;
    assert!(
        matches!(outcome, PhaseGateReconciliation::MarkedFailed { .. }),
        "the deploy sibling's authoritative unavailable context must not be replaced by the primary pr-confirm context: {outcome:?}"
    );
    assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noncompleted_sibling_redacts_non_string_verdict_payload() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let context = gate_context();
    let passing = json!({
        "verdict": "phase_gate_passed",
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "pass",
            "build_passed": "pass"
        }
    });
    insert_dispatch(
        &pool,
        "dsp-primary-private",
        "completed",
        context.clone(),
        Some(passing.clone()),
    )
    .await;
    insert_dispatch(
        &pool,
        "dsp-sibling-private",
        "failed",
        context.clone(),
        Some(json!({"verdict": {"authorization": "Bearer secret"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-primary-private".into(), "dsp-sibling-private".into()],
            ..Default::default()
        },
    )
    .await
    .expect("seed sibling privacy gate"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module

    let outcome = run_reconcile(
        &pool,
        "dsp-primary-private",
        "completed",
        context,
        Some(passing),
    )
    .await;
    let PhaseGateReconciliation::MarkedFailed { failed_reason, .. } = outcome else {
        panic!("expected sibling failure"); // agentdesk-audit: allow-unwrap — test assertion for the required fail-closed outcome
    };
    assert!(!failed_reason.contains("authorization"));
    assert!(!failed_reason.contains("Bearer secret"));
    let (verdict, reason) = gate_failure_diagnostic(&pool, "run-pg-test", 0).await;
    assert_eq!(verdict.as_deref(), Some("<non-string:object>"));
    assert!(
        reason
            .as_deref()
            .is_none_or(|text| !text.contains("authorization") && !text.contains("Bearer secret"))
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_failed_gate_is_left_alone() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    insert_dispatch(
        &pool,
        "dsp-already",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-already".into()],
            next_phase: Some(1),
            final_phase: false,
            failure_reason: Some("earlier failure".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .unwrap();

    let outcome = run_reconcile(
        &pool,
        "dsp-already",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    assert!(matches!(outcome, PhaseGateReconciliation::AlreadyFailed));
    assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");
    assert_eq!(run_status(&pool, "run-pg-test").await, "paused");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_null_kind_context_reconciles_and_repairs_from_registry() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-legacy-gate", 0).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-next-legacy", 1).await;

    let legacy_context = json!({
        "phase_gate": {
            "run_id": "run-pg-test",
            "batch_phase": 0,
            "next_phase": 1,
            "final_phase": false,
            "pass_verdict": "attacker_override",
            "checks": ["attacker_override"]
        }
    });
    let passing = json!({
        "verdict": "phase_gate_passed",
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "pass",
            "build_passed": "pass"
        }
    });
    insert_dispatch(
        &pool,
        "dsp-legacy-repair",
        "completed",
        legacy_context.clone(),
        Some(passing.clone()),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            pass_verdict: "attacker_override".into(),
            dispatch_ids: vec!["dsp-legacy-repair".into()],
            next_phase: Some(1),
            failure_reason: Some("pre-deploy strict reducer failure".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed legacy failed gate"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .expect("pause run"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module

    let summary =
        repair_phase_gates_for_run_on_pg(&pool, "run-pg-test", PhaseGateRepairOptions::default())
            .await
            .expect("repair legacy gate"); // agentdesk-audit: allow-unwrap — test-only PG assertion in #[cfg(test)] module
    assert_eq!(summary.cleared_gates, 1);
    assert_eq!(summary.blocking_gates_remaining, 0);
    assert_eq!(summary.run_status.as_deref(), Some("active"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_context_with_nonblank_persisted_kind_remains_failed_closed() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    TestPostgresDb::emulate_pre_0100_deploy_gate_rows(&pool).await;
    fixture(&pool).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-explicit-kind", 0).await;
    sqlx::query(
        "UPDATE auto_queue_entries SET phase_gate_kind = 'deploy-gate' WHERE id = 'entry-explicit-kind'",
    )
    .execute(&pool)
    .await
    .expect("set explicit deploy kind"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module

    let legacy_context = json!({
        "phase_gate": {"run_id": "run-pg-test", "batch_phase": 0}
    });
    let passing = json!({
        "verdict": "phase_gate_passed",
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "pass",
            "build_passed": "pass"
        }
    });
    insert_dispatch(
        &pool,
        "dsp-explicit-kind",
        "completed",
        legacy_context.clone(),
        Some(passing.clone()),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            dispatch_ids: vec!["dsp-explicit-kind".into()],
            ..Default::default()
        },
    )
    .await
    .expect("seed explicit-kind gate"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module

    let outcome = run_reconcile(
        &pool,
        "dsp-explicit-kind",
        "completed",
        legacy_context,
        Some(passing),
    )
    .await;
    assert!(matches!(
        outcome,
        PhaseGateReconciliation::MarkedFailed { .. }
    ));
    assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_ascii_control_whitespace_context_repairs_from_registry() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-whitespace-gate", 0).await;
    sqlx::query(
        "UPDATE auto_queue_entries SET phase_gate_kind = E' \\t\\n\\r'
             WHERE id = 'entry-whitespace-gate'",
    )
    .execute(&pool)
    .await
    .expect("set ASCII control whitespace kind"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    seed_pending_entry(&pool, "run-pg-test", "entry-next-whitespace", 1).await;

    let legacy_context = json!({
        "phase_gate": {"run_id": "run-pg-test", "batch_phase": 0, "next_phase": 1}
    });
    let passing = json!({
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "pass",
            "build_passed": "pass"
        }
    });
    insert_dispatch(
        &pool,
        "dsp-whitespace-repair",
        "completed",
        legacy_context,
        Some(passing),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            dispatch_ids: vec!["dsp-whitespace-repair".into()],
            next_phase: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("seed whitespace failed gate"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .expect("pause whitespace run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

    let summary =
        repair_phase_gates_for_run_on_pg(&pool, "run-pg-test", PhaseGateRepairOptions::default())
            .await
            .expect("repair whitespace gate"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    assert_eq!(summary.cleared_gates, 1);
    assert_eq!(summary.blocking_gates_remaining, 0);
    assert_eq!(summary.run_status.as_deref(), Some("active"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_null_and_nonblank_legacy_context_remains_failed_closed_on_repair() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    TestPostgresDb::emulate_pre_0100_deploy_gate_rows(&pool).await;
    fixture(&pool).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-mixed-null", 0).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-mixed-deploy", 0).await;
    sqlx::query(
        "UPDATE auto_queue_entries SET phase_gate_kind = 'deploy-gate'
             WHERE id = 'entry-mixed-deploy'",
    )
    .execute(&pool)
    .await
    .expect("set mixed explicit deploy kind"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

    let legacy_context = json!({
        "phase_gate": {"run_id": "run-pg-test", "batch_phase": 0}
    });
    let passing = json!({
        "verdict": "phase_gate_passed",
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "pass",
            "build_passed": "pass"
        }
    });
    insert_dispatch(
        &pool,
        "dsp-mixed-repair",
        "completed",
        legacy_context,
        Some(passing),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            dispatch_ids: vec!["dsp-mixed-repair".into()],
            failure_reason: Some("mixed provenance".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed mixed failed gate"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .expect("pause mixed run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

    let summary =
        repair_phase_gates_for_run_on_pg(&pool, "run-pg-test", PhaseGateRepairOptions::default())
            .await
            .expect("repair mixed gate"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    assert_eq!(summary.cleared_gates, 0);
    assert_eq!(summary.blocking_gates_remaining, 1);
    assert_eq!(summary.run_status.as_deref(), Some("paused"));
    assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_failed_gate_is_idempotent_after_clear() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-next-phase-repair", 1).await;

    insert_dispatch(
        &pool,
        "dsp-repair-idempotent",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-repair-idempotent".into()],
            next_phase: Some(1),
            final_phase: false,
            failure_reason: Some("stale failed verdict".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed failed gate state");
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .unwrap();

    let first =
        repair_phase_gates_for_run_on_pg(&pool, "run-pg-test", PhaseGateRepairOptions::default())
            .await
            .expect("first repair");
    assert_eq!(first.candidate_dispatches, 1);
    assert_eq!(first.cleared_gates, 1);
    assert_eq!(first.blocking_gates_remaining, 0);
    assert_eq!(first.run_status.as_deref(), Some("active"));
    assert_eq!(first.outcomes[0].outcome, "cleared");
    assert!(first.outcomes[0].run_resumed);

    let second =
        repair_phase_gates_for_run_on_pg(&pool, "run-pg-test", PhaseGateRepairOptions::default())
            .await
            .expect("second repair");
    assert_eq!(second.candidate_dispatches, 0);
    assert_eq!(second.cleared_gates, 0);
    assert_eq!(second.blocking_gates_remaining, 0);
    assert_eq!(second.run_status.as_deref(), Some("active"));
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_reports_orphan_null_dispatch_gates_without_clearing_them() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec![],
            next_phase: Some(1),
            final_phase: false,
            failure_reason: Some("orphaned phase gate".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed orphan gate state");
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .unwrap();

    let summary =
        repair_phase_gates_for_run_on_pg(&pool, "run-pg-test", PhaseGateRepairOptions::default())
            .await
            .expect("repair reports orphan gates");

    assert_eq!(summary.candidate_dispatches, 0);
    assert_eq!(summary.cleared_gates, 0);
    assert_eq!(summary.orphan_gates_skipped, 1);
    assert_eq!(summary.blocking_gates_remaining, 1);
    assert_eq!(summary.run_status.as_deref(), Some("paused"));
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 1);

    let dispatch_filtered = repair_phase_gates_for_run_on_pg(
        &pool,
        "run-pg-test",
        PhaseGateRepairOptions {
            dispatch_id: Some("dispatch-that-cannot-match-null".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("dispatch-filtered repair ignores null-dispatch orphans");
    assert_eq!(dispatch_filtered.candidate_dispatches, 0);
    assert_eq!(dispatch_filtered.cleared_gates, 0);
    assert_eq!(dispatch_filtered.orphan_gates_skipped, 0);
    assert_eq!(dispatch_filtered.blocking_gates_remaining, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_repair_calls_clear_failed_gate_once() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-next-phase-concurrent", 1).await;

    insert_dispatch(
        &pool,
        "dsp-repair-concurrent",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-repair-concurrent".into()],
            next_phase: Some(1),
            final_phase: false,
            failure_reason: Some("stale failed verdict".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed failed gate state");
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let left_pool = pool.clone();
    let left_barrier = Arc::clone(&barrier);
    let left = tokio::spawn(async move {
        left_barrier.wait().await;
        repair_phase_gates_for_run_on_pg(
            &left_pool,
            "run-pg-test",
            PhaseGateRepairOptions::default(),
        )
        .await
    });
    let right_pool = pool.clone();
    let right_barrier = Arc::clone(&barrier);
    let right = tokio::spawn(async move {
        right_barrier.wait().await;
        repair_phase_gates_for_run_on_pg(
            &right_pool,
            "run-pg-test",
            PhaseGateRepairOptions::default(),
        )
        .await
    });
    barrier.wait().await;

    let left = left.await.expect("left task").expect("left repair");
    let right = right.await.expect("right task").expect("right repair");
    let summaries = [left, right];
    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.cleared_gates)
            .sum::<usize>(),
        1,
        "only one repair call may clear the same gate"
    );
    assert_eq!(
        summaries
            .iter()
            .flat_map(|summary| summary.outcomes.iter())
            .filter(|outcome| outcome.run_resumed)
            .count(),
        1,
        "only one repair call may resume the paused run"
    );
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);
    assert_eq!(run_status(&pool, "run-pg-test").await, "active");

    pool.close().await;
    pg_db.drop().await;
}

/// #2257 concern 4 regression: a concurrent direct UPDATE on
/// `task_dispatches.status` (which is what `PATCH /api/dispatches/{id}`
/// runs through `set_dispatch_status_on_pg_with_sync`) acquires the
/// same `task_dispatches` row lock the repair path now takes. The two
/// paths must serialize on the dispatch row without deadlock and the
/// final phase_gate state must reflect the post-PATCH dispatch state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_serializes_with_concurrent_dispatch_update_without_deadlock() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;
    seed_pending_entry(&pool, "run-pg-test", "entry-next-phase-deadlock", 1).await;

    insert_dispatch(
        &pool,
        "dsp-repair-vs-patch",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "failed".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-repair-vs-patch".into()],
            next_phase: Some(1),
            final_phase: false,
            failure_reason: Some("stale failed verdict".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed failed gate state");
    sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
        .execute(&pool)
        .await
        .unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let repair_pool = pool.clone();
    let repair_barrier = Arc::clone(&barrier);
    let repair_task = tokio::spawn(async move {
        repair_barrier.wait().await;
        repair_phase_gates_for_run_on_pg(
            &repair_pool,
            "run-pg-test",
            PhaseGateRepairOptions::default(),
        )
        .await
    });
    // Simulate PATCH /api/dispatches/{id}: open its own tx, lock the
    // same task_dispatches row, mutate, commit. Mirrors the lock
    // order that `set_dispatch_status_on_pg_with_sync` takes
    // (task_dispatches first via UPDATE row lock).
    let patch_pool = pool.clone();
    let patch_barrier = Arc::clone(&barrier);
    let patch_task = tokio::spawn(async move {
        patch_barrier.wait().await;
        let mut tx = patch_pool.begin().await.expect("begin patch-simulation tx");
        sqlx::query(
            "UPDATE task_dispatches
                 SET result = CAST($1 AS jsonb), updated_at = NOW()
                 WHERE id = 'dsp-repair-vs-patch'",
        )
        .bind(r#"{"verdict":"phase_gate_passed","note":"post-patch","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}}"#)
        .execute(&mut *tx)
        .await
        .expect("patch task_dispatches");
        // Hold the row lock briefly to maximize lock-order overlap
        // with the repair task — exposes deadlock if the two paths
        // ever drift to opposite orders.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.commit().await.expect("commit patch-simulation tx");
    });
    barrier.wait().await;

    // Bounded wait so a regression that re-introduces the deadlock
    // fails fast instead of hanging the test runner.
    let repair = tokio::time::timeout(std::time::Duration::from_secs(15), repair_task)
        .await
        .expect("repair task did not deadlock")
        .expect("repair task panicked");
    let _patch = tokio::time::timeout(std::time::Duration::from_secs(15), patch_task)
        .await
        .expect("patch task did not deadlock")
        .expect("patch task panicked");
    let summary = repair.expect("repair returned Ok");

    // The repair eventually cleared the gate (one of the two orderings
    // — either it read post-PATCH state and cleared, or pre-PATCH and
    // still cleared because the existing verdict was already a pass).
    // The invariant we care about is no deadlock + final consistent
    // state: the gate row is gone, run resumed.
    assert_eq!(summary.cleared_gates + summary.failed_gates, 1);
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaiting_siblings_keeps_gate_open() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    insert_dispatch(
        &pool,
        "dsp-pass-1",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    insert_dispatch(&pool, "dsp-pending-2", "dispatched", gate_context(), None).await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-pass-1".into(), "dsp-pending-2".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    let outcome = run_reconcile(
        &pool,
        "dsp-pass-1",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    match outcome {
        PhaseGateReconciliation::AwaitingSiblings { pending_count, .. } => {
            assert_eq!(pending_count, 1);
        }
        other => panic!("expected AwaitingSiblings, got {other:?}"),
    }
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 2);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_status_marks_gate_failed_even_when_verdict_absent() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    insert_dispatch(&pool, "dsp-cancelled", "cancelled", gate_context(), None).await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-cancelled".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    let outcome = run_reconcile(&pool, "dsp-cancelled", "cancelled", gate_context(), None).await;
    match outcome {
        PhaseGateReconciliation::MarkedFailed { failed_reason, .. } => {
            assert!(failed_reason.contains("cancelled"));
        }
        other => panic!("expected MarkedFailed, got {other:?}"),
    }
    assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");

    pool.close().await;
    pg_db.drop().await;
}

/// #1980 + #699: legacy / checks-only results have all required checks
/// passing but no explicit `verdict` / `decision`. The Rust reconciler
/// must mirror the JS hook's inference and treat these as a pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checks_only_pass_without_explicit_verdict_is_inferred() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let context = gate_context();
    let result = json!({
        "checks": {
            "merge_verified": { "status": "pass" },
            "issue_closed": "passed",
            "build_passed": "pass",
        }
    });

    insert_dispatch(
        &pool,
        "dsp-checks-only",
        "completed",
        context.clone(),
        Some(result.clone()),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-checks-only".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    let outcome = run_reconcile(&pool, "dsp-checks-only", "completed", context, Some(result)).await;
    assert!(
        matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
        "checks-only pass should infer phase_gate_passed and clear: got {outcome:?}"
    );
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

    pool.close().await;
    pg_db.drop().await;
}

/// Preserve the pre-#4884 finalize contract: non-string explicit fields are
/// ignored and passing declared checks may still infer the pass verdict.
#[test]
fn non_string_explicit_verdict_preserves_checks_inference() {
    let context = gate_context();
    let result = json!({
        "verdict": 1,
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "pass",
            "build_passed": "pass"
        },
    });
    assert_eq!(
        resolve_verdict(Some(&context), &result),
        VerdictResolution::Inferred("phase_gate_passed".to_string())
    );
}

/// Persisted check statuses remain whitespace-sensitive.
/// A padded `" pass "` must never infer a passing verdict.
#[test]
fn whitespace_padded_check_status_is_not_inferred_as_pass() {
    let context = gate_context();
    let result = json!({
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "pass",
            "build_passed": " pass "
        }
    });
    assert_eq!(
        resolve_verdict(Some(&context), &result),
        VerdictResolution::Missing,
        "whitespace-padded status must not be inferred (persisted contract)"
    );
}

/// #1980: failing check entry must NOT be inferred as pass; the gate
/// stays open as a fail (mirroring the JS guard).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checks_with_any_fail_is_not_inferred_as_pass() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let context = gate_context();
    let result = json!({
        "checks": {
            "merge_verified": { "status": "pass" },
            "issue_closed": { "status": "pass" },
            "build_passed": { "status": "fail" },
        }
    });

    insert_dispatch(
        &pool,
        "dsp-mixed-checks",
        "completed",
        context.clone(),
        Some(result.clone()),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-mixed-checks".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    let outcome = run_reconcile(
        &pool,
        "dsp-mixed-checks",
        "completed",
        context,
        Some(result),
    )
    .await;
    assert!(
        matches!(outcome, PhaseGateReconciliation::MarkedFailed { .. }),
        "mixed-pass/fail checks must not be inferred as pass: got {outcome:?}"
    );
    assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");

    pool.close().await;
    pg_db.drop().await;
}

/// #1980: when this dispatch passes and ALL its sibling phase-gate
/// dispatches have already completed with a passing verdict, the gate
/// should be cleared. This exercises the multi-sibling all-pass branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_siblings_completed_and_passing_clears_gate() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    for id in ["dsp-pass-a", "dsp-pass-b"] {
        insert_dispatch(
            &pool,
            id,
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
        )
        .await;
    }
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-pass-a".into(), "dsp-pass-b".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    let outcome = run_reconcile(
        &pool,
        "dsp-pass-b",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    assert!(
        matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
        "all siblings passing should clear gate: got {outcome:?}"
    );
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn primary_and_sibling_checks_only_evidence_use_same_reducer() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    let context = gate_context();
    let checks_only = json!({
        "checks": {
            "merge_verified": {"status": "pass"},
            "issue_closed": {"status": "", "result": "passed"},
            "build_passed": {"status": "pass"}
        }
    });
    for id in ["dsp-reducer-primary", "dsp-reducer-sibling"] {
        insert_dispatch(
            &pool,
            id,
            "completed",
            context.clone(),
            Some(checks_only.clone()),
        )
        .await;
    }
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-reducer-primary".into(), "dsp-reducer-sibling".into()],
            ..Default::default()
        },
    )
    .await
    .expect("seed reducer path fixture"); // agentdesk-audit: allow-unwrap — test-only PG fixture in #[cfg(test)] module

    let outcome = run_reconcile(
        &pool,
        "dsp-reducer-primary",
        "completed",
        context,
        Some(checks_only),
    )
    .await;
    assert!(
        matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
        "primary and sibling checks-only rows must both infer the canonical verdict: {outcome:?}"
    );
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

    pool.close().await;
    pg_db.drop().await;
}

/// #1980 fix #2: callers may write `status='completed'` without a `result`
/// (CRUD route). Reconciliation must fall back to the persisted dispatch
/// row's result so a previously-recorded passing verdict is honored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_only_completion_uses_persisted_result_for_pass() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    fixture(&pool).await;

    // Seed dispatch row with persisted passing verdict already stored.
    insert_dispatch(
        &pool,
        "dsp-status-only",
        "completed",
        gate_context(),
        Some(json!({"verdict":"phase_gate_passed","checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}})),
    )
    .await;
    save_phase_gate_state_on_pg(
        &pool,
        "run-pg-test",
        0,
        &PhaseGateStateWrite {
            status: "pending".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec!["dsp-status-only".into()],
            next_phase: Some(1),
            final_phase: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed gate state");

    // Caller passes None for the result_json — simulating a status-only
    // CRUD update. Reconciliation should still see the persisted result.
    let context_text = gate_context().to_string();
    let mut tx = pool.begin().await.expect("begin tx");
    // Mirror what `set_dispatch_status_on_pg_with_sync` does: read the
    // persisted result text from the row and pass it as the fallback.
    let persisted: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT result::TEXT FROM task_dispatches WHERE id = $1",
    )
    .bind("dsp-status-only")
    .fetch_one(&mut *tx)
    .await
    .expect("load persisted result");
    let outcome = reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
        &mut tx,
        "dsp-status-only",
        "completed",
        Some(context_text.as_str()),
        persisted.as_deref(),
    )
    .await
    .expect("reconcile");
    tx.commit().await.expect("commit");
    assert!(
        matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
        "status-only completion with persisted pass result should clear: got {outcome:?}"
    );
    assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

    pool.close().await;
    pg_db.drop().await;
}
